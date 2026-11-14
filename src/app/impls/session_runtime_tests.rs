use super::*;

fn ssh_session(id: &str, jump: &str) -> Session {
    let mut s = Session::new_empty();
    s.id = id.into();
    s.kind = SessionKind::Ssh;
    s.jump_session_id = jump.into();
    s
}

#[test]
fn jump_target_requires_ssh_and_a_configured_jump_host() {
    assert_eq!(jump_target(&ssh_session("a", "b")), Some("b"));
    assert_eq!(jump_target(&ssh_session("a", "")), None, "没配跳板机");
    assert_eq!(
        jump_target(&ssh_session("a", "   ")),
        None,
        "全空白不算配了"
    );
}

/// 自引用必须拦住：拿自己当跳板只会得到一连串连接失败。
/// 带空白的形式（早期配置里可能有）同样要拦。
#[test]
fn jump_target_rejects_self_reference() {
    assert_eq!(jump_target(&ssh_session("a", "a")), None);
    assert_eq!(
        jump_target(&ssh_session("a", " a ")),
        None,
        "带空白的自引用"
    );
}

/// 只有 SSH 有跳板概念（串口 / Telnet / 本地会话没有）。
#[test]
fn jump_target_ignores_non_ssh_kinds() {
    let mut s = ssh_session("a", "b");
    s.kind = SessionKind::Telnet;
    assert_eq!(jump_target(&s), None);
}

/// 带空白的 id 要归一后再返回：调用方拿它精确查表，返回原始串会匹配不到
/// 任何会话 -> 本该走跳板机的会话**静默直连**目标机（网络边界与审计链路一起失效）。
#[test]
fn jump_target_trims_padded_ids() {
    assert_eq!(jump_target(&ssh_session("a", " b ")), Some("b"));
    assert_eq!(jump_target(&ssh_session("a", "\tb\n")), Some("b"));
}

#[test]
fn monitoring_starts_for_the_active_tab() {
    assert!(should_start_monitoring(false, true));
}

#[test]
fn monitoring_is_off_for_background_tabs() {
    assert!(!should_start_monitoring(false, false));
}

/// MCP 要读任意标签的状态：启用时必须全量开，漏掉这条会让新标签一直为 0。
#[test]
fn monitoring_is_always_on_when_mcp_is_enabled() {
    assert!(should_start_monitoring(true, false));
    assert!(should_start_monitoring(true, true));
}
