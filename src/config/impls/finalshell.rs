//! FinalShell connection export compatibility.
//!
//! FinalShell stores the first eight decoded bytes as a per-password header. The
//! header seeds `java.util.Random`; an MD5 digest derived from that stream supplies
//! the DES key for the remaining bytes (`DES/ECB/PKCS5Padding`).

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use des::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use des::Des;
use serde::Deserialize;

use super::structs::{AuthMethod, Secret, Session, SessionKind};

const JAVA_RANDOM_MULTIPLIER: u64 = 0x5DEECE66D;
const JAVA_RANDOM_ADDEND: u64 = 0xB;
const JAVA_RANDOM_MASK: u64 = (1_u64 << 48) - 1;

/// The fields used by FinalShell's `*_connect_config.json` files. Unknown fields
/// are intentionally ignored so exports from different FinalShell releases remain
/// importable.
#[derive(Debug, Deserialize)]
struct FinalShellConnection {
    conection_type: i64,
    #[serde(default)]
    name: String,
    host: String,
    #[serde(default = "default_ssh_port")]
    port: u16,
    #[serde(default)]
    user_name: String,
    #[serde(default)]
    password: String,
    #[serde(default = "default_password_auth")]
    authentication_type: i64,
    #[serde(default)]
    terminal_encoding: String,
    #[serde(default)]
    description: String,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_password_auth() -> i64 {
    1
}

/// Parse either one FinalShell connection object or an array of connection
/// objects. FinalShell currently identifies SSH connection exports with type 100.
pub(super) fn parse_export(raw: &str) -> Result<Vec<Session>> {
    let value: serde_json::Value = serde_json::from_str(raw).context("not valid JSON")?;
    let connections: Vec<FinalShellConnection> = match value {
        object @ serde_json::Value::Object(_) => {
            vec![serde_json::from_value(object).context("not a FinalShell connection export")?]
        }
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| {
                serde_json::from_value(value).context("invalid FinalShell connection entry")
            })
            .collect::<Result<_>>()?,
        _ => bail!("not a FinalShell connection export"),
    };

    if connections.is_empty() {
        bail!("FinalShell export contains no connections");
    }

    connections
        .into_iter()
        .map(FinalShellConnection::into_session)
        .collect()
}

impl FinalShellConnection {
    fn into_session(self) -> Result<Session> {
        if self.conection_type != 100 {
            bail!(
                "unsupported FinalShell connection type {} (only SSH type 100 is supported)",
                self.conection_type
            );
        }
        if self.authentication_type != 1 {
            bail!(
                "unsupported FinalShell authentication type {} (only password type 1 is supported)",
                self.authentication_type
            );
        }
        let host = self.host.trim().to_string();
        if host.is_empty() {
            bail!("FinalShell connection has an empty host");
        }
        if self.port == 0 {
            bail!("FinalShell connection {host} has an invalid port");
        }

        let password = if self.password.is_empty() {
            Secret::default()
        } else {
            Secret::new(
                decode_password(&self.password)
                    .with_context(|| format!("failed to decrypt FinalShell password for {host}"))?,
            )
        };
        let name = match self.name.trim() {
            "" => host.clone(),
            name => name.to_string(),
        };
        let encoding = match self.terminal_encoding.trim() {
            "" => "UTF-8".to_string(),
            encoding => encoding.to_string(),
        };

        Ok(Session {
            name,
            host,
            port: self.port,
            user: self.user_name.trim().to_string(),
            auth: AuthMethod::Password,
            password,
            kind: SessionKind::Ssh,
            encoding,
            note: self.description,
            ..Session::new_empty()
        })
    }
}

fn decode_password(encoded: &str) -> Result<String> {
    let blob = STANDARD
        .decode(encoded)
        .context("password is not valid Base64")?;
    if blob.len() < 16 || (blob.len() - 8) % 8 != 0 {
        bail!("password payload has an invalid length");
    }
    let (head, ciphertext) = blob.split_at(8);
    let key = derive_des_key(head)?;
    let cipher = Des::new_from_slice(&key).expect("DES keys are always eight bytes");
    let mut plaintext = ciphertext.to_vec();
    for block in plaintext.chunks_exact_mut(8) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    remove_pkcs5_padding(&mut plaintext)?;

    // FinalShell has existed across both UTF-8-default and legacy Chinese JREs.
    // Match Java's `new String(bytes)` behaviour for the common encodings without
    // ever using a lossy conversion for a credential.
    match String::from_utf8(plaintext) {
        Ok(value) => Ok(value),
        Err(err) => encoding_rs::GBK
            .decode_without_bom_handling_and_without_replacement(err.as_bytes())
            .map(|value| value.into_owned())
            .context("decrypted password is neither UTF-8 nor GBK"),
    }
}

fn remove_pkcs5_padding(bytes: &mut Vec<u8>) -> Result<()> {
    let padding = bytes.last().copied().context("empty DES plaintext")? as usize;
    if padding == 0
        || padding > 8
        || padding > bytes.len()
        || !bytes[bytes.len() - padding..]
            .iter()
            .all(|byte| *byte as usize == padding)
    {
        bail!("invalid DES padding");
    }
    bytes.truncate(bytes.len() - padding);
    Ok(())
}

fn derive_des_key(head: &[u8]) -> Result<[u8; 8]> {
    if head.len() != 8 {
        bail!("FinalShell password header must be eight bytes");
    }

    let divisor = JavaRandom::new(java_byte(head[5])).next_int(127);
    if divisor == 0 {
        bail!("FinalShell password header produced an invalid key divisor");
    }
    let seed = 3_680_984_568_597_093_857_i64 / divisor as i64;
    let mut random = JavaRandom::new(seed);
    for _ in 0..java_byte(head[0]).max(0) {
        random.next_long();
    }

    let mut secondary = JavaRandom::new(random.next_long());
    let values = [
        java_byte(head[4]),
        secondary.next_long(),
        java_byte(head[7]),
        java_byte(head[3]),
        secondary.next_long(),
        java_byte(head[1]),
        random.next_long(),
        java_byte(head[2]),
    ];
    let mut key_material = Vec::with_capacity(values.len() * 8);
    for value in values {
        key_material.extend_from_slice(&value.to_be_bytes());
    }
    let digest = md5::compute(&key_material);
    let mut key = [0_u8; 8];
    key.copy_from_slice(&digest.0[..8]);
    Ok(key)
}

fn java_byte(byte: u8) -> i64 {
    i8::from_ne_bytes([byte]) as i64
}

/// Bit-for-bit implementation of the parts of `java.util.Random` used by
/// FinalShell's password-key derivation.
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    fn new(seed: i64) -> Self {
        Self {
            seed: ((seed as u64) ^ JAVA_RANDOM_MULTIPLIER) & JAVA_RANDOM_MASK,
        }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = self
            .seed
            .wrapping_mul(JAVA_RANDOM_MULTIPLIER)
            .wrapping_add(JAVA_RANDOM_ADDEND)
            & JAVA_RANDOM_MASK;
        (self.seed >> (48 - bits)) as u32 as i32
    }

    fn next_int(&mut self, bound: i32) -> i32 {
        debug_assert!(bound > 0);
        if bound & (bound - 1) == 0 {
            return ((bound as i64 * self.next(31) as i64) >> 31) as i32;
        }
        loop {
            let bits = self.next(31);
            let value = bits % bound;
            if bits.wrapping_sub(value).wrapping_add(bound - 1) >= 0 {
                return value;
            }
        }
    }

    fn next_long(&mut self) -> i64 {
        let high = self.next(32) as i64;
        let low = self.next(32) as i64;
        high.wrapping_shl(32).wrapping_add(low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_java_generated_password_vector() {
        // Generated with the Java algorithm and a fixed eight-byte header.
        assert_eq!(
            decode_password("AwcLDRETFx1OXQgZJNatCplesw+x/P04").unwrap(),
            "meatshell-test"
        );
    }

    #[test]
    fn parses_single_finalshell_connection() {
        let raw = r#"{
            "conection_type": 100,
            "name": "development",
            "host": "192.0.2.10",
            "port": 2222,
            "user_name": "admin",
            "password": "AwcLDRETFx1OXQgZJNatCplesw+x/P04",
            "authentication_type": 1,
            "terminal_encoding": "UTF-8",
            "description": "imported from FinalShell"
        }"#;
        let sessions = parse_export(raw).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.name, "development");
        assert_eq!(session.host, "192.0.2.10");
        assert_eq!(session.port, 2222);
        assert_eq!(session.user, "admin");
        assert_eq!(session.password.as_str(), "meatshell-test");
        assert_eq!(session.note, "imported from FinalShell");
    }

    #[test]
    fn rejects_non_ssh_finalshell_connections() {
        let raw = r#"{"conection_type": 7, "host": "192.0.2.10"}"#;
        let error = parse_export(raw).unwrap_err().to_string();
        assert!(error.contains("unsupported FinalShell connection type"));
    }
}
