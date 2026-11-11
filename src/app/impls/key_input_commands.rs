use super::*;

pub(super) fn wire_key_commands(window: &AppWindow, app: &AppContext) {
    let handles = app.handles.clone();
    let store = app.store.clone();

    // 运行时隧道只在当前连接内生效，不写回会话配置。
    {
        let handles_rc = handles.clone();
        window.on_tunnel_add(
            move |tab_id: SharedString,
                  name: SharedString,
                  kind: SharedString,
                  bind: SharedString,
                  bind_port: SharedString,
                  host: SharedString,
                  host_port: SharedString| {
                let Some(forward) =
                    parse_tunnel_forward(&kind, &name, &bind, &bind_port, &host, &host_port)
                else {
                    return;
                };
                // 非 loopback 绑定会把监听端口暴露到局域网。
                if forward.bind_addr != "127.0.0.1" && forward.bind_addr != "localhost" {
                    tracing::warn!(
                        "runtime tunnel binds non-loopback {}:{} (LAN exposure: {})",
                        forward.bind_addr,
                        forward.bind_port,
                        forward.kind
                    );
                }
                if let Some(handle) = handles_rc.borrow().get(tab_id.as_str()) {
                    handle.add_tunnel(format!("runtime-{}", uuid::Uuid::new_v4()), forward);
                }
            },
        );
    }
    {
        let handles_rc = handles.clone();
        window.on_tunnel_stop(move |tab_id: SharedString, tunnel_id: SharedString| {
            if let Some(handle) = handles_rc.borrow().get(tab_id.as_str()) {
                handle.stop_tunnel(tunnel_id.to_string());
            }
        });
    }

    // --- Command bar (#55): run command + quick-command management ---------
    {
        let handles_rc = handles.clone();
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_run_command(
            move |tab_id: SharedString, cmd: SharedString, to_all: bool| {
                let Some((line, bytes)) = encode_command_bar_input(&cmd) else {
                    return;
                };
                {
                    let h = handles_rc.borrow();
                    if to_all {
                        for handle in h.values() {
                            handle.send_raw(bytes.clone());
                        }
                    } else if let Some(handle) = h.get(tab_id.as_str()) {
                        handle.send_raw(bytes);
                    }
                }
                {
                    let mut s = store_rc.borrow_mut();
                    s.push_command_history(line);
                }
                // 每执行一条命令都全量写盘不划算（还要重新加密口令）→ 防抖。
                crate::app::settings::debounced_save(&store_rc);
                if let Some(w) = weak.upgrade() {
                    w.set_command_history(history_model(&store_rc.borrow()));
                }
            },
        );
    }
    // Copy a history command to the clipboard (#96).
    {
        window.on_copy_text(move |text: SharedString| {
            let t = text.to_string();
            std::thread::spawn(move || clipboard_set_text(t));
        });
    }
    // Delete a history entry (#96). The command-history model remains in
    // storage order, so this legacy row index still maps straight through.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_delete_history(move |i: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let idx = i as usize;
                if idx < s.command_history().len() {
                    s.remove_command_history(idx);
                    s.save_logging();
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_command_history(history_model(&store_rc.borrow()));
            }
        });
    }
}
