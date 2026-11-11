use super::*;

pub(super) fn wire_update_transfers(window: &AppWindow, ctx: &AppContext) {
    let sftp_handles = &ctx.sftp_handles;

    // Transfer records (download/upload progress + history) shown in the popup.
    let transfers_model: Rc<VecModel<TransferInfo>> = Rc::new(VecModel::default());
    window.set_transfers(ModelRc::from(transfers_model.clone()));
    {
        let tm = transfers_model.clone();
        window.on_clear_transfers(move || tm.set_vec(Vec::<TransferInfo>::new()));
    }
    {
        // Cancel a transfer by id. The id is a UUID unique across sessions, so we
        // broadcast to every SFTP handle — only the owning one has it registered
        // and will act on it (#100).
        let sftp_handles = sftp_handles.clone();
        window.on_cancel_transfer(move |id: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                for h in handles.values() {
                    h.cancel_transfer(id.to_string());
                }
            }
        });
    }

    // Open-source libraries with resolved versions, shown in the About popup.
    // Versions are baked in at compile time by build.rs → $OUT_DIR/deps.rs
    // (included as module-level `DEP_VERSIONS` above).
    {
        let get_ver = dep_version;

        let zh = crate::i18n::t;
        let libs: Vec<SharedString> = vec![
            SharedString::from(format!(
                "Slint v{} — {}",
                get_ver("slint"),
                zh("图形界面框架", "GUI framework")
            )),
            SharedString::from(format!(
                "russh v{} — {}",
                get_ver("russh"),
                zh("SSH 协议实现", "SSH protocol")
            )),
            SharedString::from(format!(
                "russh-sftp v{} — {}",
                get_ver("russh-sftp"),
                zh("SFTP 文件传输", "SFTP file transfer")
            )),
            SharedString::from(format!(
                "ssh-key v{} — {}",
                get_ver("ssh-key"),
                zh("SSH 密钥解析", "SSH key parsing")
            )),
            SharedString::from(format!(
                "tokio v{} — {}",
                get_ver("tokio"),
                zh("异步运行时", "async runtime")
            )),
            SharedString::from(format!(
                "alacritty_terminal v{} — {}",
                get_ver("alacritty_terminal"),
                zh("终端模拟与解析", "terminal emulator & parser")
            )),
            SharedString::from(format!(
                "sysinfo v{} — {}",
                get_ver("sysinfo"),
                zh("本机资源采集", "local resource sampling")
            )),
            SharedString::from(format!(
                "serde v{} — {}",
                get_ver("serde"),
                zh("配置序列化", "config serialization")
            )),
            SharedString::from(format!(
                "arboard v{} — {}",
                get_ver("arboard"),
                zh("系统剪贴板", "system clipboard")
            )),
            SharedString::from(format!(
                "rfd v{} — {}",
                get_ver("rfd"),
                zh("原生文件对话框", "native file dialogs")
            )),
            SharedString::from(format!(
                "directories v{} — {}",
                get_ver("directories"),
                zh("配置目录定位", "config dir lookup")
            )),
            SharedString::from(format!(
                "chrono v{} — {}",
                get_ver("chrono"),
                zh("日期时间处理", "date/time handling")
            )),
            SharedString::from(format!(
                "uuid v{} — {}",
                get_ver("uuid"),
                zh("唯一标识符", "unique identifiers")
            )),
            SharedString::from(format!(
                "anyhow v{} — {}",
                get_ver("anyhow"),
                zh("错误处理", "error handling")
            )),
            SharedString::from(format!(
                "tracing v{} — {}",
                get_ver("tracing"),
                zh("日志", "logging")
            )),
            SharedString::from(format!(
                "futures v{} — {}",
                get_ver("futures"),
                zh("异步辅助", "async helpers")
            )),
            SharedString::from(format!(
                "rand v{} — {}",
                get_ver("rand"),
                zh("随机数", "randomness")
            )),
            SharedString::from(format!(
                "winresource v{} — {}",
                get_ver("winresource"),
                zh("Windows 图标嵌入", "Windows icon embedding")
            )),
        ]
        .to_vec();
        window.set_about_libs(ModelRc::from(Rc::new(VecModel::from(libs))));
    }
}
