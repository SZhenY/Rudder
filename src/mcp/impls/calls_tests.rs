#[cfg(test)]
mod tests {
    //! 只读路径不依赖 UI 线程：直接准备一块终端缓冲就能验证。

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use crate::mcp::types::{AppHandles, McpConfigSnapshot, McpState};

    /// 流式截断：只留前 `cap` 字节，但必须把管道读空 —— 否则子进程会卡在写端。
    #[tokio::test]
    async fn read_capped_stops_at_the_cap_and_drains_the_pipe() {
        let data = vec![b'x'; 10 * 1024 * 1024];
        let mut reader = &data[..];
        let (kept, cut) = super::super::read_capped(&mut reader, 4096)
            .await
            .expect("read");
        assert_eq!(kept.len(), 4096);
        assert!(cut, "超出上限必须标记为截断");
        assert!(reader.is_empty(), "抽干管道后不应有剩余");
    }

    #[tokio::test]
    async fn read_capped_leaves_short_output_untouched() {
        let data = b"hello".to_vec();
        let mut reader = &data[..];
        let (kept, cut) = super::super::read_capped(&mut reader, 4096)
            .await
            .expect("read");
        assert_eq!(kept, b"hello");
        assert!(!cut);
    }

    fn state_with_buffer() -> super::super::SharedState {
        let mut buffer = crate::terminal::TermBuffer::new(24, 80, 1000);
        buffer.ingest(b"hello from pty\r\n");
        let mut bufs = HashMap::new();
        bufs.insert("term-1".to_string(), Arc::new(Mutex::new(buffer)));
        Arc::new(McpState::new(
            McpConfigSnapshot {
                enabled: true,
                port: 8765,
                loopback: true,
                lan_enabled: false,
                lan_ifaces: Vec::new(),
                token: "rk_test".to_string(),
                client_grants: Vec::new(),
                consent: crate::mcp::ConsentPolicy::Auto,
                pty_commands: false,
                audit: crate::mcp::types::AuditConfig::default(),
            },
            AppHandles {
                bufs: Arc::new(Mutex::new(bufs)),
                tab_statuses: Arc::new(Mutex::new(HashMap::new())),
                local_snap: Arc::new(Mutex::new(Default::default())),
                sftp_handles: Arc::new(Mutex::new(HashMap::new())),
            },
            tokio::runtime::Handle::current(),
        ))
    }

    /// 快照新鲜时直接复用 UI 采样器的结果，不额外采样。
    #[tokio::test]
    async fn host_info_reuses_a_fresh_snapshot() {
        let state = state_with_buffer();
        {
            let mut snap = state
                .app
                .local_snap
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            snap.cpu_percent = 0.25;
            snap.mem_total_mib = 16384;
            snap.sampled_at = Some(std::time::Instant::now());
        }
        let value = super::super::call(&state, "get_host_info", &json!({}))
            .await
            .unwrap();
        assert_eq!(value["scope"], "local");
        assert_eq!(value["source"], "snapshot");
        assert_eq!(value["memory"]["total_mib"], 16384);
    }

    /// 快照陈旧（窗口隐藏时采样器会整拍跳过）→ 现场采一次，数字必须是真的。
    #[tokio::test]
    async fn host_info_samples_on_demand_when_the_snapshot_is_stale() {
        let state = state_with_buffer();
        {
            let mut snap = state
                .app
                .local_snap
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            snap.sampled_at = None;
        }
        // 首次采样要预热 200ms（新建的 System 没有 CPU 基线）。
        let value = super::super::call(&state, "get_host_info", &json!({}))
            .await
            .unwrap();
        assert_eq!(value["source"], "on_demand");
        assert!(
            value["memory"]["total_mib"].as_u64().unwrap_or(0) > 0,
            "本机总内存不该是 0"
        );
        assert!(value["filesystems"].is_array());
    }

    #[tokio::test]
    async fn read_terminal_returns_screen_text() {
        let state = state_with_buffer();
        let value = super::super::call(&state, "read_terminal", &json!({ "tab_id": "term-1" }))
            .await
            .unwrap();
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("hello from pty"), "{text}");
        assert_eq!(value["output_seq"], 16);
    }

    #[tokio::test]
    async fn unknown_tab_is_reported_as_invalid_argument() {
        let state = state_with_buffer();
        let err = super::super::call(&state, "read_terminal", &json!({ "tab_id": "nope" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "invalid_argument");
    }

    #[tokio::test]
    async fn list_sessions_reports_ui_unavailable_without_a_window() {
        let state = state_with_buffer();
        let err = super::super::call(&state, "list_sessions", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "ui_unavailable");
    }

    /// 回归：静默模式曾经永远不命中 —— `seen_seq` 在检测到变化时就被推进，
    /// 再用它判断"有新输出"第二圈恒为假，于是一路等到超时。
    #[tokio::test]
    async fn wait_for_output_returns_once_output_goes_quiet() {
        let state = state_with_buffer();
        let value = super::super::call(
            &state,
            "wait_for_output",
            &json!({
                "tab_id": "term-1",
                "since_seq": 0,
                "quiet_ms": 50,
                "timeout_ms": 2000,
            }),
        )
        .await
        .unwrap();
        assert_eq!(value["matched"], true, "{value}");
        assert!(value.get("timed_out").is_none(), "{value}");
    }
}

#[cfg(test)]
mod sys_json_tests {
    use super::super::sys_json;
    use crate::ssh::SystemDetails;

    /// 全空时给 `null`，不给 6 个空对象 —— 否则调用方会以为"这台机器没有内存"。
    #[test]
    fn empty_details_serialize_to_null() {
        assert!(sys_json(&SystemDetails::default()).is_null());
    }

    /// 两个 5 元组节要输出成具名对象，并丢掉采集脚本的占位列。
    #[test]
    fn tuple_sections_drop_placeholder_columns() {
        let sys = SystemDetails {
            overview: vec![("系统".to_string(), "Linux".to_string())],
            networks: vec![(
                "eth0".to_string(),
                "1 GB".to_string(),
                "2 GB".to_string(),
                "-".to_string(),
                "-".to_string(),
            )],
            filesystems: vec![(
                "/".to_string(),
                "120G".to_string(),
                "61%".to_string(),
                "46G".to_string(),
                "/".to_string(),
            )],
            ..Default::default()
        };

        let value = sys_json(&sys);
        assert_eq!(value["overview"]["系统"], "Linux");
        let net = value["networks"][0].as_object().expect("networks 是对象");
        assert_eq!(net.len(), 3, "占位列不该输出：{net:?}");
        assert_eq!(net["iface"], "eth0");
        let fs = value["filesystems"][0]
            .as_object()
            .expect("filesystems 是对象");
        assert_eq!(fs.len(), 4, "重复的 mount 列不该输出：{fs:?}");
        assert_eq!(fs["used_percent"], "61%");
    }

    /// 只有部分节有值时，缺的节不出现在对象里（而不是给空对象）。
    #[test]
    fn missing_sections_are_omitted() {
        let sys = SystemDetails {
            memory: vec![("总计".to_string(), "16 GB".to_string())],
            ..Default::default()
        };
        let value = sys_json(&sys);
        assert!(value.get("overview").is_none());
        assert!(value.get("networks").is_none());
        assert_eq!(value["memory"]["总计"], "16 GB");
    }
}

#[cfg(test)]
mod mode_tests {
    use super::super::parse_mode;

    /// 八进制是唯一接受的写法：`"755"` 与 `"0755"` 等价，`"999"` 这种必须在进 worker 前拒掉。
    #[test]
    fn mode_is_parsed_as_octal() {
        assert_eq!(parse_mode("755").unwrap(), 0o755);
        assert_eq!(parse_mode("0755").unwrap(), 0o755);
        assert_eq!(parse_mode("0644").unwrap(), 0o644);
        assert_eq!(parse_mode("  777 ").unwrap(), 0o777);
    }

    #[test]
    fn bad_modes_are_rejected() {
        for raw in ["999", "75", "", "abc", "8888", "-1"] {
            assert!(parse_mode(raw).is_err(), "{raw} 应该被拒");
        }
    }
}
