//! 会话关闭原因与其中英文文案（设置页与审计共用）。


/// 移除租约的原因。审计用 `reason` 字段区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// 用户在连接弹窗里点了拒绝。
    Denied,
    /// 弹窗超时：视为未接入。
    Timeout,
    /// 用户在 AI 页点了断开。
    Disconnected,
    /// 令牌轮换，旧客户端全部失效。
    TokenRotated,
    /// 空闲超过 `IDLE`。
    Idle,
    /// 客户端主动 DELETE。
    Delete,
    /// rmcp 已不认这个会话。
    NotFound,
    /// 服务被停止 / 重启（换端口、换网口、关掉开关）。这条路径原来完全无痕。
    Stopped,
}

impl CloseReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Timeout => "timeout",
            Self::Disconnected => "disconnected",
            Self::TokenRotated => "token_rotated",
            Self::Idle => "idle",
            Self::Delete => "delete",
            Self::NotFound => "not_found",
            Self::Stopped => "stopped",
        }
    }

    /// AI 页「最近断开」里的一行短句。
    pub(crate) fn label(self, en: bool) -> &'static str {
        match self {
            Self::Denied => {
                if en {
                    "you denied it"
                } else {
                    "你在弹窗里拒绝了"
                }
            }
            Self::Timeout => {
                if en {
                    "authorisation timed out"
                } else {
                    "授权弹窗超时（45 秒未确认）"
                }
            }
            Self::Disconnected => {
                if en {
                    "you disconnected it"
                } else {
                    "你在设置里断开了"
                }
            }
            Self::TokenRotated => {
                if en {
                    "access token changed"
                } else {
                    "访问令牌已更换"
                }
            }
            Self::Idle => {
                if en {
                    "idle for 15 minutes"
                } else {
                    "闲置 15 分钟被回收"
                }
            }
            Self::Delete => {
                if en {
                    "closed by the client"
                } else {
                    "客户端主动关闭"
                }
            }
            Self::NotFound => {
                if en {
                    "session expired in the MCP runtime"
                } else {
                    "会话在 MCP 运行时已失效"
                }
            }
            Self::Stopped => {
                if en {
                    "MCP service stopped"
                } else {
                    "MCP 服务已停止"
                }
            }
        }
    }

    /// 是否需要用户注意（界面上用告警色）。客户端自己关闭 / 闲置回收不算异常。
    pub(crate) fn alarm(self) -> bool {
        matches!(
            self,
            Self::Denied | Self::Timeout | Self::TokenRotated | Self::Stopped
        )
    }
}

/// 标签页连接状态码转字符串（审计与工具输出共用同一套名字）。
pub(super) fn state_name(state: u8) -> &'static str {
    match state {
        0 => "connecting",
        1 => "connected",
        2 => "closed",
        _ => "unknown",
    }
}

#[cfg(test)]
mod close_reason_tests {
    use super::CloseReason;

    const ALL: [CloseReason; 8] = [
        CloseReason::Denied,
        CloseReason::Timeout,
        CloseReason::Disconnected,
        CloseReason::TokenRotated,
        CloseReason::Idle,
        CloseReason::Delete,
        CloseReason::NotFound,
        CloseReason::Stopped,
    ];

    /// 每个变体都要有中英两套文案，且非空 —— 漏一个界面上就是空白。
    #[test]
    fn every_reason_has_a_non_empty_label_in_both_languages() {
        for reason in ALL {
            assert!(!reason.label(false).is_empty(), "{reason:?} 缺中文文案");
            assert!(!reason.label(true).is_empty(), "{reason:?} 缺英文文案");
            assert_ne!(reason.label(false), reason.label(true));
        }
    }

    /// 告警色只给"用户需要看一眼"的原因；客户端自己关、闲置回收都不该报警。
    #[test]
    fn alarm_covers_only_user_actionable_reasons() {
        for reason in [
            CloseReason::Denied,
            CloseReason::Timeout,
            CloseReason::TokenRotated,
            CloseReason::Stopped,
        ] {
            assert!(reason.alarm(), "{reason:?} 应该告警");
        }
        for reason in [
            CloseReason::Disconnected,
            CloseReason::Idle,
            CloseReason::Delete,
            CloseReason::NotFound,
        ] {
            assert!(!reason.alarm(), "{reason:?} 不该告警");
        }
    }

    /// `as_str` 是审计字段，必须与变体一一对应且不重复。
    #[test]
    fn as_str_values_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for reason in ALL {
            assert!(seen.insert(reason.as_str()), "{} 重复", reason.as_str());
        }
    }
}
