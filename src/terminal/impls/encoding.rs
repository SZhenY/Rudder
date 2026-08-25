//! Terminal character-encoding support for the SSH PTY boundary (#338,
//! upstream 7742c80). Local shells (CMD/PowerShell/WSL) and Telnet stay UTF-8;
//! only SSH sessions carry a per-session encoding.

use encoding_rs::{CoderResult, Encoding, UTF_8};

/// Stateful terminal decoder plus outbound encoder. Keeping the decoder alive
/// across SSH packets is important because a multibyte character may be split
/// between two channel data messages.
pub(crate) struct TerminalEncoding {
    encoding: &'static Encoding,
    decoder: encoding_rs::Decoder,
}

impl TerminalEncoding {
    pub(crate) fn new(label: &str) -> Self {
        let encoding = Encoding::for_label(label.trim().as_bytes()).unwrap_or(UTF_8);
        Self {
            encoding,
            decoder: encoding.new_decoder_without_bom_handling(),
        }
    }

    pub(crate) fn decode(&mut self, input: &[u8]) -> String {
        let mut output = String::with_capacity(input.len().saturating_mul(2).max(16));
        let mut offset = 0;
        loop {
            let (result, read, _) =
                self.decoder
                    .decode_to_string(&input[offset..], &mut output, false);
            offset += read;
            match result {
                CoderResult::InputEmpty => break,
                CoderResult::OutputFull => output.reserve(input.len().max(16)),
            }
        }
        output
    }

    pub(crate) fn encode(&self, input: &[u8]) -> Vec<u8> {
        if self.encoding == UTF_8 {
            return input.to_vec();
        }
        // Keystrokes originate as UTF-8 from Slint; re-encode to the session's
        // charset before they hit the remote PTY.
        let text = String::from_utf8_lossy(input);
        let (encoded, _, _) = self.encoding.encode(&text);
        encoded.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gbk_round_trip_and_split_decode() {
        let encoder = TerminalEncoding::new("gbk");
        let bytes = encoder.encode("中文测试".as_bytes());
        let mut decoder = TerminalEncoding::new("gbk");
        let split = 3; // splits the second multibyte character in half
        let mut decoded = decoder.decode(&bytes[..split]);
        decoded.push_str(&decoder.decode(&bytes[split..]));
        assert_eq!(decoded, "中文测试");
    }

    #[test]
    fn unknown_label_falls_back_to_utf8() {
        let mut codec = TerminalEncoding::new("not-a-real-charset");
        assert_eq!(codec.decode("你好".as_bytes()), "你好");
    }
}
