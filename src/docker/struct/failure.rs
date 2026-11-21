//! Docker 失败分类。UI 只管展示，MCP 只管映射到错误码；分类逻辑集中在这里。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockerFailure {
    NoPermission,
    /// sudo / su 已经拿到密码但认证被拒（密码错或账号无提权权限）。
    SudoAuthFailed,
    NoCli,
    DaemonDown,
    Timeout,
    SessionLost,
    NotFound,
    Conflict,
    Parse,
    Other,
}

impl DockerFailure {}
