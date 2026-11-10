use super::*;

impl SftpHandle {
    pub fn list_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ListDir(path));
    }
    pub fn refresh_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::RefreshDir(path));
    }
    pub fn download(&self, remote: String, local_dir: String, conflict: DownloadConflict) {
        let _ = self.commands.send(SftpCommand::Download {
            remote,
            local_dir,
            conflict,
        });
    }
    pub fn download_archive(&self, remote_dir: String, names: Vec<String>, local_dir: String) {
        let _ = self.commands.send(SftpCommand::DownloadArchive {
            remote_dir,
            names,
            local_dir,
        });
    }
    pub fn cancel_transfer(&self, id: String) {
        let _ = self.commands.send(SftpCommand::CancelTransfer(id));
    }
    pub fn upload(&self, local: PathBuf, remote_dir: String) {
        let _ = self.commands.send(SftpCommand::Upload {
            local,
            remote_dir,
            cleanup_after: None,
        });
    }
    pub fn copy_to(
        &self,
        remotes: Vec<String>,
        target: UnboundedSender<SftpCommand>,
        target_dir: String,
    ) {
        let _ = self.commands.send(SftpCommand::CopyTo {
            remotes,
            target,
            target_dir,
        });
    }
    pub fn toggle_tree_node(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ToggleTreeNode(path));
    }
    pub fn delete(&self, path: String) {
        let _ = self.commands.send(SftpCommand::Delete(path));
    }
    pub fn open_temp(&self, remote: String, edit: bool) {
        let _ = self.commands.send(SftpCommand::OpenTemp { remote, edit });
    }
    pub fn rename(&self, from: String, to: String) {
        let _ = self.commands.send(SftpCommand::Rename { from, to });
    }
    pub fn chmod(&self, path: String, mode: u32) {
        let _ = self.commands.send(SftpCommand::Chmod { path, mode });
    }
    pub fn mkdir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::MkDir(path));
    }
    /// MCP：列目录并取回结果。
    pub fn mcp_list(
        &self,
        path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<Vec<RemoteEntry>, String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SftpCommand::McpList { path, reply });
        rx
    }
    /// MCP：新建目录，返回成败。
    pub fn mcp_mkdir(&self, path: String) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SftpCommand::McpMkDir { path, reply });
        rx
    }
    /// MCP：删除文件 / 目录（`recursive` 决定能否删非空目录）。
    pub fn mcp_remove(
        &self,
        path: String,
        recursive: bool,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SftpCommand::McpRemove {
            path,
            recursive,
            reply,
        });
        rx
    }
    /// MCP：改名 / 移动。
    pub fn mcp_rename(
        &self,
        from: String,
        to: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self
            .commands
            .send(SftpCommand::McpRename { from, to, reply });
        rx
    }
    /// MCP：改权限位。
    pub fn mcp_chmod(
        &self,
        path: String,
        mode: u32,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self
            .commands
            .send(SftpCommand::McpChmod { path, mode, reply });
        rx
    }
    /// MCP：下载到下载目录，同名自动避让（`name (1).ext`）。
    pub fn mcp_download_auto(
        &self,
        remote: String,
        dir: PathBuf,
    ) -> tokio::sync::oneshot::Receiver<Result<(PathBuf, bool, u64), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self
            .commands
            .send(SftpCommand::McpDownloadAuto { remote, dir, reply });
        rx
    }
    /// MCP：把本地文件上传到远端路径。
    pub fn mcp_upload(
        &self,
        local: PathBuf,
        remote: String,
    ) -> tokio::sync::oneshot::Receiver<Result<u64, String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SftpCommand::McpUpload {
            local,
            remote,
            reply,
        });
        rx
    }
    pub fn touch(&self, path: String) {
        let _ = self.commands.send(SftpCommand::TouchFile(path));
    }
    pub fn read_text(&self, remote: String, edit: bool) {
        let _ = self.commands.send(SftpCommand::ReadText { remote, edit });
    }
    pub fn write_text(&self, remote: String, content: String) {
        let _ = self
            .commands
            .send(SftpCommand::WriteText { remote, content });
    }
    pub fn close(&self) {
        let _ = self.commands.send(SftpCommand::Close);
    }
}
