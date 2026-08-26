use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::{ChannelId, ChannelMsg};
use slint::ComponentHandle;
use ssh_key::{HashAlg, PublicKey};
use tokio::sync::{mpsc, oneshot};

slint::include_modules!();

type HostKeyReply = Arc<Mutex<Option<oneshot::Sender<bool>>>>;

struct ClientHandler {
    host: String,
    port: u16,
    window: slint::Weak<AppWindow>,
    host_key_reply: HostKeyReply,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let message = format!(
            "{}:{}\n{}\n\nBeta 版本暂不保存 known_hosts，请核对指纹后决定是否连接。",
            self.host, self.port, fingerprint
        );
        let (tx, rx) = oneshot::channel();
        if let Ok(mut slot) = self.host_key_reply.lock() {
            *slot = Some(tx);
        } else {
            return Ok(false);
        }
        let weak = self.window.clone();
        if slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.set_host_key_message(message.into());
                window.set_host_key_open(true);
            }
        })
        .is_err()
        {
            return Ok(false);
        }
        Ok(rx.await.unwrap_or(false))
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn set_status(window: &slint::Weak<AppWindow>, status: impl Into<String>, connected: bool) {
    let weak = window.clone();
    let status = status.into();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_status(status.into());
            window.set_connected(connected);
            if connected {
                window.set_password("".into());
            }
        }
    });
}

fn show_output(window: &slint::Weak<AppWindow>, buffer: &Arc<Mutex<String>>, bytes: &[u8]) {
    let plain = strip_ansi_escapes::strip(bytes);
    let text = String::from_utf8_lossy(&plain).replace('\0', "");
    let snapshot = {
        let Ok(mut output) = buffer.lock() else {
            return;
        };
        output.push_str(&text);
        const MAX_OUTPUT: usize = 256 * 1024;
        if output.len() > MAX_OUTPUT {
            let mut cut = output.len() - MAX_OUTPUT;
            while !output.is_char_boundary(cut) {
                cut += 1;
            }
            output.drain(..cut);
        }
        output.clone()
    };
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_terminal_output(snapshot.into());
        }
    });
}

async fn run_session(
    host: String,
    port: u16,
    username: String,
    password: String,
    mut input: mpsc::UnboundedReceiver<Vec<u8>>,
    window: slint::Weak<AppWindow>,
    output: Arc<Mutex<String>>,
    host_key_reply: HostKeyReply,
) -> Result<()> {
    set_status(&window, format!("正在连接 {host}:{port}…"), false);
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(30)),
        ..Default::default()
    });
    let handler = ClientHandler {
        host: host.clone(),
        port,
        window: window.clone(),
        host_key_reply,
    };
    let mut handle = client::connect(config, (host.as_str(), port), handler)
        .await
        .with_context(|| format!("无法连接 {host}:{port}"))?;
    let authenticated = handle
        .authenticate_password(username.clone(), password)
        .await
        .context("密码认证失败")?;
    if !authenticated {
        anyhow::bail!("服务器拒绝了用户名或密码");
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .context("无法打开 SSH 会话")?;
    channel
        .request_pty(true, "xterm-256color", 80, 28, 0, 0, &[])
        .await
        .context("无法请求 PTY")?;
    channel
        .request_shell(true)
        .await
        .context("无法启动 Shell")?;

    set_status(&window, format!("已连接 {username}@{host}"), true);
    show_output(
        &window,
        &output,
        format!("\r\n[MeatShell] 已连接 {username}@{host}\r\n").as_bytes(),
    );

    loop {
        tokio::select! {
            command = input.recv() => {
                let Some(command) = command else { break };
                channel.data(&command[..]).await.context("发送命令失败")?;
            }
            message = channel.wait() => {
                match message {
                    Some(ChannelMsg::Data { data })
                    | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        show_output(&window, &output, &data);
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        }
    }
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    Ok(())
}

#[no_mangle]
#[cfg(target_os = "android")]
fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app).expect("failed to initialize Slint Android backend");
    let window = AppWindow::new().expect("failed to create Android window");
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create Tokio runtime"),
    );
    let current_input = Arc::new(Mutex::new(None::<mpsc::UnboundedSender<Vec<u8>>>));
    let current_task = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));
    let output = Arc::new(Mutex::new(String::from("MeatShell Android Beta\r\n")));
    let host_key_reply: HostKeyReply = Arc::new(Mutex::new(None));

    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let current_input = current_input.clone();
        let current_task = current_task.clone();
        let output = output.clone();
        let host_key_reply = host_key_reply.clone();
        window.on_connect_requested(move |host, port, username, password| {
            let host = host.trim().to_string();
            let username = username.trim().to_string();
            if host.is_empty() || username.is_empty() || !(1..=65535).contains(&port) {
                set_status(&weak, "请填写有效的主机、端口和用户名", false);
                return;
            }
            if let Some(window) = weak.upgrade() {
                window.set_host_key_open(false);
            }
            if let Ok(mut reply) = host_key_reply.lock() {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(false);
                }
            }
            if let Ok(mut task) = current_task.lock() {
                if let Some(old) = task.take() {
                    old.abort();
                }
            }
            if let Ok(mut buffer) = output.lock() {
                *buffer = String::from("MeatShell Android Beta\r\n");
            }
            let (tx, rx) = mpsc::unbounded_channel();
            if let Ok(mut input) = current_input.lock() {
                *input = Some(tx);
            }
            let task_weak = weak.clone();
            let task_output = output.clone();
            let task_host_keys = host_key_reply.clone();
            let join = runtime.spawn(async move {
                if let Err(error) = run_session(
                    host,
                    port as u16,
                    username,
                    password.to_string(),
                    rx,
                    task_weak.clone(),
                    task_output,
                    task_host_keys,
                )
                .await
                {
                    set_status(&task_weak, format!("连接失败：{error:#}"), false);
                } else {
                    set_status(&task_weak, "已断开", false);
                }
            });
            if let Ok(mut task) = current_task.lock() {
                *task = Some(join);
            }
        });
    }

    {
        let current_input = current_input.clone();
        window.on_send_requested(move |command| {
            if let Ok(input) = current_input.lock() {
                if let Some(tx) = input.as_ref() {
                    let mut bytes = command.as_bytes().to_vec();
                    bytes.push(b'\r');
                    let _ = tx.send(bytes);
                }
            }
        });
    }

    {
        let weak = window.as_weak();
        let current_input = current_input.clone();
        let current_task = current_task.clone();
        let host_key_reply = host_key_reply.clone();
        window.on_disconnect_requested(move || {
            if let Ok(mut task) = current_task.lock() {
                if let Some(task) = task.take() {
                    task.abort();
                }
            }
            if let Ok(mut input) = current_input.lock() {
                *input = None;
            }
            if let Some(window) = weak.upgrade() {
                window.set_host_key_open(false);
            }
            if let Ok(mut reply) = host_key_reply.lock() {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(false);
                }
            }
            set_status(&weak, "已断开", false);
        });
    }

    {
        let weak = window.as_weak();
        let host_key_reply = host_key_reply.clone();
        window.on_host_key_response(move |accepted| {
            if let Some(window) = weak.upgrade() {
                window.set_host_key_open(false);
            }
            if let Ok(mut reply) = host_key_reply.lock() {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(accepted);
                }
            }
        });
    }

    window.run().expect("Android event loop failed");
}
