/// `CRED_DECIDED` 里缓存的一份回答 —— 口令用 `Secret` 装着（Drop 时零化、
/// Debug 只打 `***`），与配置里的凭据同一套处理。
pub(crate) struct SecretCred {
    user: crate::config::Secret,
    password: crate::config::Secret,
    remember: bool,
}

impl SecretCred {
    pub(crate) fn from_reply(reply: &crate::ssh::CredentialReply) -> Self {
        Self {
            user: crate::config::Secret::new(reply.0.clone()),
            password: crate::config::Secret::new(reply.1.clone()),
            remember: reply.2,
        }
    }

    /// 还原成 `CredentialReply` 交给 SSH 层（那边要的就是明文三元组）。
    pub(crate) fn to_reply(&self) -> crate::ssh::CredentialReply {
        (
            self.user.as_str().to_string(),
            self.password.as_str().to_string(),
            self.remember,
        )
    }
}
