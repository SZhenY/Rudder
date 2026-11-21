//! Local terminal session worker.
//!
//! Local shells need a real PTY/ConPTY. Plain stdin/stdout pipes break normal
//! console editing (Backspace/Delete/IME composition) and make Windows shells
//! disagree about encodings. `portable-pty` gives us ConPTY on Windows and a
//! Unix PTY on Linux/macOS while reusing the same UI event path as SSH.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::Session;
use crate::i18n::t;
use crate::ssh::{SessionCommand, SessionEvent, SessionHandle};

/// 本进程创建的本地 shell 子进程 PID。
///
/// ConPTY 子进程不会随 `rudder.exe` 硬终止自动消失，关窗前要按 PID 一起收掉。
fn local_children() -> &'static Mutex<HashSet<u32>> {
    static REGISTRY: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 在会话生命周期内登记子进程；Drop 时注销。
struct LocalChildGuard(Option<u32>);

impl LocalChildGuard {
    fn register(pid: Option<u32>) -> Self {
        if let Some(pid) = pid {
            local_children()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pid);
        }
        Self(pid)
    }
}

impl Drop for LocalChildGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            local_children()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pid);
        }
    }
}

/// 关窗 / 硬退出前杀掉所有仍在运行的本地 shell；已退出的 PID 直接忽略。
pub(crate) fn kill_all_children() {
    let pids: Vec<u32> = {
        let mut set = local_children().lock().unwrap_or_else(|e| e.into_inner());
        set.drain().collect()
    };
    for pid in pids {
        kill_pid(pid);
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    const PROCESS_TERMINATE: u32 = 0x0001;
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
        fn TerminateProcess(handle: *mut core::ffi::c_void, code: u32) -> i32;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    }
    // SAFETY: 句柄只在本函数内使用并立即关闭；PID 来自本进程 spawn 的子进程。
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    // SAFETY: 只向本进程创建的子进程发送 SIGKILL；PID 已退出时返回错误，忽略即可。
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

pub fn spawn_local_session(
    runtime: &tokio::runtime::Handle,
    session: Session,
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_for_task = evt_tx.clone();
    let task_runtime = runtime.clone();
    // 丢弃 `JoinHandle` = detach（tokio 语义）：任务照常运行。
    runtime.spawn(async move {
        if let Err(err) = run_local(
            task_runtime,
            session,
            cmd_rx,
            evt_for_task.clone(),
            initial_cols,
            initial_rows,
        )
        .await
        {
            let _ = evt_for_task.send(SessionEvent::Closed(format!("{err:#}")));
        }
    });

    (
        SessionHandle { commands: cmd_tx },
        evt_rx,
    )
}

async fn run_local(
    runtime: tokio::runtime::Handle,
    session: Session,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
) -> Result<()> {
    let (program, args) = local_program(&session);
    let label = if session.name.trim().is_empty() {
        program.clone()
    } else {
        session.name.clone()
    };
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}",
        t("启动本地终端", "Starting local terminal"),
        label
    )));

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: initial_rows.clamp(1, u16::MAX as u32) as u16,
            cols: initial_cols.clamp(1, u16::MAX as u32) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open local pty")?;

    let mut cmd = CommandBuilder::new(&program);
    for arg in &args {
        cmd.arg(arg);
    }
    cmd.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to start local terminal: {program}"))?;
    let _child_guard = LocalChildGuard::register(child.process_id());
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("local pty reader")?;
    let writer = pair.master.take_writer().context("local pty writer")?;
    let writer = Arc::new(Mutex::new(writer));
    let child = Arc::new(Mutex::new(child));

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}",
        t("已启动", "Started"),
        label
    )));

    {
        let reader_events = events.clone();
        let reader_runtime = runtime;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                crate::output_budget::wait_for_drain_blocking(&reader_runtime);
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = reader_events.send(SessionEvent::Closed(
                            t("本地终端已退出", "local terminal exited").into(),
                        ));
                        break;
                    }
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                        if !crate::output_budget::emit_output(&reader_events, text) {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = reader_events.send(SessionEvent::Closed(format!(
                            "{}: {e}",
                            t("本地终端读取失败", "local terminal read failed")
                        )));
                        break;
                    }
                }
            }
        });
    }

    while let Some(cmd) = commands.recv().await {
        match cmd {
            // 本地 / 串口会话没有远端监控通道，忽略采样周期切换。
            SessionCommand::SetSampleInterval(_) => {}
            SessionCommand::RawInput(bytes) => {
                tracing::debug!("local pty write len={} bytes", bytes.len());
                let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
                if guard.write_all(&bytes).and_then(|_| guard.flush()).is_err() {
                    let _ = events.send(SessionEvent::Closed(t("写入失败", "write failed").into()));
                    break;
                }
            }
            SessionCommand::Resize(cols, rows) => {
                let _ = pair.master.resize(PtySize {
                    rows: rows.clamp(1, u16::MAX as u32) as u16,
                    cols: cols.clamp(1, u16::MAX as u32) as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            SessionCommand::AddTunnel { .. }
            | SessionCommand::StopTunnel(_)
            | SessionCommand::SetResourceMonitoring(_) => {}
            SessionCommand::KillProcess { reply, .. } => {
                let _ = reply.send(crate::ssh::ProcessKillResult {
                    success: false,
                    message: t(
                        "本地终端不支持远程进程操作",
                        "Remote process control is unavailable for local terminals",
                    )
                    .into(),
                });
            }
            SessionCommand::Exec { reply, .. } => {
                let _ = reply.send(crate::ssh::ExecResult::unsupported(t(
                    "本地终端不支持 exec 通道",
                    "exec is unavailable for local terminals",
                )));
            }
            SessionCommand::Close => {
                let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
                break;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
const WSL_LOGIN_SHELL: &str = "shell=$(getent passwd \"$(id -un)\" 2>/dev/null | cut -d: -f7); \
     [ -x \"$shell\" ] || shell=${SHELL:-/bin/sh}; exec \"$shell\" -l";

fn local_program(session: &Session) -> (String, Vec<String>) {
    match session.host.as_str() {
        #[cfg(windows)]
        "cmd" => (
            "cmd.exe".to_string(),
            vec![
                "/Q".to_string(),
                "/K".to_string(),
                "chcp 65001>nul".to_string(),
            ],
        ),
        #[cfg(windows)]
        // Do not rely on wsl.exe's implicit shell launch. In particular, Arch
        // WSL installations whose passwd login shell is fish can open a PTY
        // without ever presenting an interactive prompt (#352). Resolve the
        // current Linux user's configured shell inside the distribution, then
        // replace the temporary POSIX shell with it in login mode.
        //
        // Multiple WSL profiles (upstream a64097e): `--distribution` selects a
        // non-default distro and `--cd` starts in the profile's directory
        // (default `~`, the selected user's home). Both are global options, so
        // they must precede `--exec`.
        "wsl" => {
            let mut args = Vec::new();
            if !session.local_distribution.trim().is_empty() {
                args.push("--distribution".to_string());
                args.push(session.local_distribution.clone());
            }
            args.push("--cd".to_string());
            args.push(if session.local_working_dir.trim().is_empty() {
                "~".to_string()
            } else {
                session.local_working_dir.clone()
            });
            args.push("--exec".to_string());
            args.push("/bin/sh".to_string());
            args.push("-lc".to_string());
            args.push(WSL_LOGIN_SHELL.to_string());
            ("wsl.exe".to_string(), args)
        }
        #[cfg(windows)]
        _ => (
            "powershell.exe".to_string(),
            vec![
                "-NoLogo".to_string(),
                "-NoExit".to_string(),
                "-Command".to_string(),
                "$utf8 = New-Object System.Text.UTF8Encoding $false; [Console]::InputEncoding = $utf8; [Console]::OutputEncoding = $utf8; $OutputEncoding = $utf8".to_string(),
            ],
        ),
        #[cfg(not(windows))]
        _ => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            (shell, Vec::new())
        }
    }
}

// WSL_LOGIN_SHELL and the wsl branch only exist on Windows, so the tests are
// Windows-only as well (upstream 547b588 gates the whole module the same way).
#[cfg(all(test, windows))]
mod tests {
    use super::{
        WSL_LOGIN_SHELL, LocalChildGuard, kill_all_children, local_children, local_program,
    };
    use crate::config::Session;

    #[cfg(windows)]
    fn session_for(host: &str) -> Session {
        let mut session = Session::new_empty();
        session.host = host.to_string();
        session
    }

    #[cfg(windows)]
    #[test]
    fn windows_shells_start_in_utf8_mode() {
        let (_, ps_args) = local_program(&session_for("powershell"));
        assert!(ps_args.iter().any(|arg| arg.contains("OutputEncoding")));
        assert!(ps_args.iter().any(|arg| arg.contains("InputEncoding")));

        let (_, cmd_args) = local_program(&session_for("cmd"));
        assert!(cmd_args.iter().any(|arg| arg.contains("chcp 65001")));
    }

    #[cfg(windows)]
    #[test]
    fn wsl_explicitly_starts_the_passwd_login_shell() {
        let (program, args) = local_program(&session_for("wsl"));
        assert_eq!(program, "wsl.exe");
        assert_eq!(
            args,
            [
                "--cd",
                "~",
                "--exec",
                "/bin/sh",
                "-lc",
                WSL_LOGIN_SHELL,
            ]
        );
        assert!(WSL_LOGIN_SHELL.contains("getent passwd"));
        assert!(WSL_LOGIN_SHELL.contains("exec \"$shell\" -l"));
    }

    #[test]
    fn kill_all_children_terminates_registered_process() {
        #[cfg(windows)]
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .spawn()
            .expect("spawn local child");
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn local child");

        let pid = child.id();
        let guard = LocalChildGuard::register(Some(pid));
        assert!(
            local_children()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid)
        );

        kill_all_children();

        let mut exited = false;
        for _ in 0..100 {
            if child.try_wait().ok().flatten().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(guard);
        assert!(exited, "registered local child was not terminated");
        assert!(
            !local_children()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid)
        );
    }

    #[cfg(windows)]
    #[test]
    fn wsl_uses_distribution_and_startup_directory() {
        let mut session = session_for("wsl");
        session.local_distribution = "Ubuntu-24.04".to_string();
        session.local_working_dir = "/home/deploy".to_string();
        let (_, args) = local_program(&session);
        assert_eq!(args[0], "--distribution");
        assert_eq!(args[1], "Ubuntu-24.04");
        assert_eq!(args[2], "--cd");
        assert_eq!(args[3], "/home/deploy");
        // The #352 explicit login-shell launch is preserved after the options.
        assert_eq!(&args[4..], ["--exec", "/bin/sh", "-lc", WSL_LOGIN_SHELL]);
    }
}
