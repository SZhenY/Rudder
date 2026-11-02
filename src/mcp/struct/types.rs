//! `types` 已按职责拆分到同目录的 config / error / labels / lease / state。
//! 这里只保留再导出，模块内的 `types::X` 引用不必改路径。

pub(crate) use super::config::{ConsentPolicy, GrantUpdate, McpConfigSnapshot, McpStatus};
// 只有测试构造配置快照时用到；进不了非测试构建，避免 unused 告警。
#[cfg(test)]
pub(crate) use super::config::AuditConfig;
pub(crate) use super::error::{McpError, McpErrorKind};
pub(crate) use super::labels::CloseReason;
pub(crate) use super::lease::{
    ClientLease, ConsentState, McpSessionInfo, grant_allows_silent, grant_perms, session8,
};
pub(crate) use super::state::{AppHandles, McpState, SharedState};
