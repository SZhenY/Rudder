use super::*;

pub(super) fn wire_mcp_status(
    window: &AppWindow,
    store: &Store,
    ifaces: &Rc<VecModel<McpIface>>,
    clients: &Rc<VecModel<McpClientRow>>,
    timer: &slint::Timer,
) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        let clients = clients.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(window) = weak.upgrade() {
                    publish_status(&window, &store, &clients);
                }
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_audit_enabled(move |value: bool| {
            persist(&store, |s| s.set_mcp_audit_enabled(value));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_audit_retention(move |text: SharedString| -> bool {
            let trimmed = text.trim();
            let Ok(days) = trimmed.parse::<u16>() else {
                return false;
            };
            if days > 365 {
                return false;
            }
            persist(&store, |s| s.set_mcp_audit_retention_days(days));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
            true
        });
    }
    {
        window.on_open_mcp_log_dir(move || {
            let dir = crate::config::log_dir();
            let _ = std::fs::create_dir_all(&dir);
            open_directory(&dir);
        });
    }
}
