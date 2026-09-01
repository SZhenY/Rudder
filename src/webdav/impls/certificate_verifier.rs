use super::types::WebDavAcceptAnyCertVerifier;

impl ureq::rustls::client::danger::ServerCertVerifier for WebDavAcceptAnyCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &ureq::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[ureq::rustls::pki_types::CertificateDer<'_>],
        _server_name: &ureq::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: ureq::rustls::pki_types::UnixTime,
    ) -> std::result::Result<ureq::rustls::client::danger::ServerCertVerified, ureq::rustls::Error>
    {
        // Optional certificate pinning: when a fingerprint is configured,
        // only a matching end-entity cert passes. This keeps the
        // accept-invalid-certs escape hatch usable without silently trusting
        // any MITM (#webdav-pin).
        // Copy the 64-char fingerprint so no borrow of `self` escapes the
        // method (the verifier trait callback's return type outlives `self`).
        let pin: Option<String> = self
            .pin
            .clone()
            .or_else(|| crate::webdav::webdav_cert_pin().map(str::to_string));
        if let Some(pin) = pin {
            use ring::digest::{Context, SHA256};
            let mut ctx = Context::new(&SHA256);
            ctx.update(end_entity.as_ref());
            let hex: String = ctx
                .finish()
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if hex == pin {
                return Ok(ureq::rustls::client::danger::ServerCertVerified::assertion());
            }
            return Err(ureq::rustls::Error::General(
                "server certificate does not match the pinned fingerprint".into(),
            ));
        }
        Ok(ureq::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &ureq::rustls::pki_types::CertificateDer<'_>,
        _dss: &ureq::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        ureq::rustls::client::danger::HandshakeSignatureValid,
        ureq::rustls::Error,
    > {
        Ok(ureq::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &ureq::rustls::pki_types::CertificateDer<'_>,
        _dss: &ureq::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        ureq::rustls::client::danger::HandshakeSignatureValid,
        ureq::rustls::Error,
    > {
        Ok(ureq::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<ureq::rustls::SignatureScheme> {
        use ureq::rustls::SignatureScheme;
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
