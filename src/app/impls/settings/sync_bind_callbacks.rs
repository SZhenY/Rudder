use super::*;

pub(super) fn wire_sync_settings(window: &AppWindow, store: &Store) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            persist(&store, |s| {
                s.set_sync_upload(v);
            });
            // 主窗口属性是设置窗的播种源，不回写会显示旧值。
            if let Some(w) = weak.upgrade() {
                w.set_sync_upload_enabled(v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  accept_invalid_certs: bool| {
                let password =
                    effective_password(password.as_str(), store.borrow().webdav_password());
                persist(&store, |s| {
                    s.set_webdav_settings(
                        enabled,
                        url.to_string(),
                        username.to_string(),
                        password,
                        remote_path.to_string(),
                        accept_invalid_certs,
                    );
                });
                // 存完就把输入框清空：口令不再以明文留在界面层里。
                if let Some(w) = weak.upgrade() {
                    // 主窗口属性是设置窗的播种源：不回写的话，"保存 → 关掉 →
                    // 重开"会看到保存前的旧值（配置其实已经写进去了）。
                    // 用 store 里的值回填，顺带带上规范化结果（裁尾斜杠、空路径回默认）。
                    {
                        let s = store.borrow();
                        w.set_webdav_enabled(s.webdav_enabled());
                        w.set_webdav_url(s.webdav_url().into());
                        w.set_webdav_username(s.webdav_username().into());
                        w.set_webdav_remote_path(s.webdav_remote_path().into());
                        w.set_webdav_accept_invalid_certs(s.webdav_accept_invalid_certs());
                    }
                    w.set_webdav_password(String::new().into());
                }
            },
        );
    }
}
