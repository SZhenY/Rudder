use super::*;

// ---------------------------------------------------------------------------

pub(crate) struct SftpClientHandler {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) events: UnboundedSender<SessionEvent>,
}

pub(crate) fn sftp_handler(
    session: &Session,
    events: &UnboundedSender<SessionEvent>,
) -> SftpClientHandler {
    SftpClientHandler {
        host: session.host.clone(),
        port: session.port,
        events: events.clone(),
    }
}

impl Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // 0.65 的参数可能是主机证书：我们不用 CA，按证书认证的那把 key 校验。
        let key = server_public_key.public_key();
        Ok(crate::ssh::verify_host_key(
            &self.host,
            self.port,
            &key,
            &self.events,
            // SFTP 在 shell 连上之后才连（见 `session_runtime` 的 ready 门），
            // 那时主机密钥已被记住，正常走 Match；这里不需要也不该再放行未知主机。
            false,
        )
        .await)
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}
