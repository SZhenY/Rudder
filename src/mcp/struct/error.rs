//! 工具调用的错误：给客户端的 code、可执行的提示。


/// 服务启动失败的原因类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpErrorKind {
    /// 没有可监听的地址：回环关了、又没勾任何网口。
    NoAddress,
    /// 某个地址绑定失败（端口被占、权限不足等）。
    BindFailed,
}

/// 工具层错误。区分"界面没了""超时""参数不对""这一传输不支持"，客户端据此
/// 决定重试还是换用法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpError {
    UiUnavailable,
    UiTimeout,
    Reentrant,
    Invalid(String),
    NotSupported(&'static str),
    NotConnected(&'static str),
    TransferFailed(String),
    TimedOut,
    /// 弹窗超时（用户没在 45 秒内决定）。
    ConsentTimeout,
    /// 用户撤销了本次会话的授权。
    Revoked,
    /// 内部故障（采样线程崩了之类），不该让调用方按"用法错误"处理。
    Internal(String),
}

impl McpError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UiUnavailable => "ui_unavailable",
            Self::UiTimeout => "ui_timeout",
            Self::Reentrant => "reentrant",
            Self::Invalid(_) => "invalid_argument",
            Self::NotSupported(_) => "unsupported",
            Self::NotConnected(_) => "not_connected",
            Self::TransferFailed(_) => "transfer_failed",
            Self::TimedOut => "timeout",
            Self::ConsentTimeout => "consent_timeout",
            Self::Revoked => "revoked",
            Self::Internal(_) => "internal",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::UiUnavailable => "the Rudder window is not available".to_string(),
            Self::UiTimeout => "the UI thread did not answer in time".to_string(),
            Self::Reentrant => "bridge call issued from the UI thread".to_string(),
            Self::Invalid(message) => message.clone(),
            Self::NotSupported(message) => (*message).to_string(),
            Self::NotConnected(message) => (*message).to_string(),
            Self::TransferFailed(message) => message.clone(),
            Self::TimedOut => "the tool call exceeded its time budget".to_string(),
            Self::ConsentTimeout => "no answer from the Rudder window in time".to_string(),
            Self::Revoked => "the user revoked this session".to_string(),
            Self::Internal(message) => message.clone(),
        }
    }
}

