use super::*;

// ---------------------------------------------------------------------------
// Session callbacks (welcome page + dialog)
// ---------------------------------------------------------------------------

/// Re-sync the session list honouring the current Quick Connect search box
/// contents (upstream 547b588). All mutation paths funnel through this so the
/// filtered view stays live.
pub(crate) fn sync_sessions_for_window(
    window: &slint::Weak<AppWindow>,
    store: &ConfigStore,
    model: &VecModel<SessionInfo>,
) {
    let query = window
        .upgrade()
        .map(|window| window.get_host_search_query().to_string())
        .unwrap_or_default();
    sync_sessions_to_model_with_filter(store, model, &query);
}

/// Read the current mount-filter string from the store.  Empty = show all.
pub(crate) fn mount_filter() -> String {
    HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().mount_filter().to_owned())
            .unwrap_or_default()
    })
}

/// Read the hide-special-partitions flag from the store.
pub(crate) fn hide_special_partitions() -> bool {
    HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().hide_special_partitions())
            .unwrap_or(true)
    })
}

/// Read the convert-eol flag from the store.
pub(crate) fn convert_eol() -> bool {
    HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().convert_eol())
            .unwrap_or(false)
    })
}

/// Known pseudo-filesystems and tiny special partitions to suppress in the
/// resource panel when hide-special-partitions is on.
pub(crate) fn is_special_partition(mount: &str) -> bool {
    mount == "/proc"
        || mount == "/sys"
        || mount == "/dev"
        || mount == "/run"
        || mount.starts_with("/proc/")
        || mount.starts_with("/sys/")
        || mount.starts_with("/dev/")
        || mount.starts_with("/run/")
        || mount.starts_with("/boot/efi")
        || mount == "/tmp"
        || mount.starts_with("/snap/")
        || mount.starts_with("/var/snap/")
}
