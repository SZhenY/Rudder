//! Docker CLI 命令构造。
//!
//! 远端 SSH exec 是非交互 shell，PATH 常常不含 `/usr/local/bin`；所有命令统一带
//! PATH 前缀。容器 id / name 先做字符白名单校验，避免把 MCP / UI 参数拼进 shell。
//!
//! 提权分三档：
//! - `Direct`：按登录用户执行（用户已在 docker 组时最优）；
//! - `SudoNoPass`：`sudo -n`，依赖 NOPASSWD；
//! - `SudoPassword` / `SuPassword`：`sudo -S` / `su -c`，密码经 exec 通道 stdin 写入。
//!
//! `sudo` 不是简单地插在命令前面：sudo 默认 `secure_path` 不含 snap / 自编译安装
//! 目录，所以用 `env PATH="$PATH"` 把外层 shell 已经展开的 PATH 传进去。

pub(crate) const PATH_PREFIX: &str =
    "PATH=/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/snap/bin; export PATH; ";

const CONTAINER_ID_MAX: usize = 128;
const ACCOUNT_MAX: usize = 64;

/// 提权方式。`SudoPassword` / `SuPassword` 需要配合 `exec_privileged` 写入密码。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Direct,
    SudoNoPass,
    SudoPassword,
    SuPassword,
}

impl Mode {
    /// 是否要通过 exec 通道把密码写进 stdin。
    pub(crate) fn uses_password(self) -> bool {
        matches!(self, Mode::SudoPassword | Mode::SuPassword)
    }
}

/// 一条命令的提权参数。`target_user` 为空时按 root 处理。
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Elevation {
    pub mode: Mode,
    pub target_user: String,
}

impl Elevation {
    pub(crate) fn direct() -> Self {
        Self {
            mode: Mode::Direct,
            target_user: String::new(),
        }
    }

    pub(crate) fn sudo_nopass(user: &str) -> Self {
        Self {
            mode: Mode::SudoNoPass,
            target_user: user.to_string(),
        }
    }

    pub(crate) fn sudo_password(user: &str) -> Self {
        Self {
            mode: Mode::SudoPassword,
            target_user: user.to_string(),
        }
    }

    pub(crate) fn su(user: &str) -> Self {
        Self {
            mode: Mode::SuPassword,
            target_user: user.to_string(),
        }
    }
}

/// 统一的命令外壳。
fn wrap(elev: &Elevation, argv: &str) -> String {
    match elev.mode {
        Mode::Direct => format!("{PATH_PREFIX}{argv}"),
        Mode::SudoNoPass => {
            let user = sudo_user_flag(&elev.target_user);
            format!("{PATH_PREFIX}sudo -n{user} env PATH=\"$PATH\" {argv}")
        }
        Mode::SudoPassword => {
            let user = sudo_user_flag(&elev.target_user);
            format!("{PATH_PREFIX}LC_ALL=C sudo -S -p 'Password:'{user} env PATH=\"$PATH\" {argv}")
        }
        Mode::SuPassword => {
            // `su -` 会重置 PATH，内层命令必须自带 PATH 前缀；整段命令单引号转义后
            // 交给 `su -c`，避免容器 id / format 里的引号破坏外层。
            let inner = format!("{PATH_PREFIX}{argv}");
            format!(
                "LC_ALL=C su - {user} -c {quoted}",
                user = account_name(&elev.target_user),
                quoted = shell_single_quote(&inner)
            )
        }
    }
}

/// `sudo -u`：空 / root 时不加参数，root 是默认目标。
fn sudo_user_flag(user: &str) -> String {
    let user = user.trim();
    if user.is_empty() || user == "root" {
        String::new()
    } else {
        format!(" -u {}", account_name(user))
    }
}

/// 账号名白名单；非法值退回 root，绝不把 UI 输入原样拼进 shell。
fn account_name(user: &str) -> String {
    let user = user.trim();
    if !user.is_empty()
        && user.len() <= ACCOUNT_MAX
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        user.to_string()
    } else {
        "root".to_string()
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `docker ps` 输出每行一个 JSON 对象，字段由 `PsRow` 解析。
pub(crate) fn ps(all: bool, elev: &Elevation) -> String {
    let all = if all { " --all" } else { "" };
    wrap(elev, &format!("docker ps{all} --format '{{{{json .}}}}'"))
}

pub(crate) fn start(id: &str, elev: &Elevation) -> Result<String, ()> {
    with_id("start", id, "", elev)
}

pub(crate) fn stop(id: &str, elev: &Elevation) -> Result<String, ()> {
    with_id("stop", id, "-t 5 ", elev)
}

pub(crate) fn restart(id: &str, elev: &Elevation) -> Result<String, ()> {
    with_id("restart", id, "", elev)
}

pub(crate) fn remove(id: &str, force: bool, elev: &Elevation) -> Result<String, ()> {
    with_id("rm", id, if force { "-f " } else { "" }, elev)
}

/// 删除：运行中先 `stop -t 5`，再 `rm`；stop 失败不阻断 rm（已停止时 stop 会报错）。
/// 两段都要带同一套提权前缀。
pub(crate) fn remove_sequence(id: &str, elev: &Elevation) -> Result<String, ()> {
    let id = checked_id(id)?;
    let stop = wrap(
        elev,
        &format!("docker stop -t 5 -- {id} >/dev/null 2>&1 || true"),
    );
    let rm = wrap(elev, &format!("docker rm -- {id}"));
    Ok(format!("{stop}; {rm}"))
}

pub(crate) fn logs(id: &str, tail: u32, timestamps: bool, elev: &Elevation) -> Result<String, ()> {
    let id = checked_id(id)?;
    let tail = tail.clamp(1, 2000);
    let ts = if timestamps { "--timestamps " } else { "" };
    Ok(wrap(
        elev,
        &format!("docker logs --tail {tail} {ts}-- {id}"),
    ))
}

pub(crate) fn exec_shell(id: &str, elev: &Elevation) -> Result<String, ()> {
    let id = checked_id(id)?;
    Ok(wrap(elev, &format!("docker exec -it -- {id} sh")))
}

/// 一次探测：服务状态 / 开机自启 / 能否直连 docker / 是否有免密 sudo。
///
/// 全部用标记行走，解析不依赖行序；`docker ps` 只用于判断直连能力，daemon
/// 未运行时它也会失败，但错误分类仍以真实命令的 stderr 为准。
pub(crate) fn probe() -> String {
    format!(
        "{PATH_PREFIX}\
         echo RUDDER_ACTIVE=$(systemctl is-active docker 2>/dev/null || true); \
         echo RUDDER_ENABLED=$(systemctl is-enabled docker 2>/dev/null || true); \
         docker ps >/dev/null 2>&1 && echo RUDDER_DIRECT=1 || echo RUDDER_DIRECT=0; \
         sudo -n true >/dev/null 2>&1 && echo RUDDER_SUDO=1 || echo RUDDER_SUDO=0"
    )
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Probe {
    pub service_up: Option<bool>,
    pub service_enabled: Option<bool>,
    pub direct: Option<bool>,
    pub sudo_nopass: Option<bool>,
}

impl Probe {
    pub(crate) fn is_empty(self) -> bool {
        self.service_up.is_none()
            && self.service_enabled.is_none()
            && self.direct.is_none()
            && self.sudo_nopass.is_none()
    }
}

/// 解析 [`probe`] 的输出；未知 / 缺行的字段留 `None`。
pub(crate) fn parse_probe(text: &str) -> Probe {
    let mut probe = Probe::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("RUDDER_ACTIVE=") {
            probe.service_up = match value {
                "active" => Some(true),
                "inactive" | "failed" => Some(false),
                _ => None,
            };
        } else if let Some(value) = line.strip_prefix("RUDDER_ENABLED=") {
            probe.service_enabled = match value {
                "enabled" => Some(true),
                "disabled" => Some(false),
                _ => None,
            };
        } else if let Some(value) = line.strip_prefix("RUDDER_DIRECT=") {
            probe.direct = match value {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            };
        } else if let Some(value) = line.strip_prefix("RUDDER_SUDO=") {
            probe.sudo_nopass = match value {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            };
        }
    }
    probe
}

/// 启停 Docker 服务。
pub(crate) fn service_start(elev: &Elevation) -> String {
    wrap(elev, "systemctl start docker")
}

pub(crate) fn service_stop(elev: &Elevation) -> String {
    wrap(elev, "systemctl stop docker")
}

fn with_id(subcommand: &str, id: &str, tail: &str, elev: &Elevation) -> Result<String, ()> {
    let id = checked_id(id)?;
    let tail = tail.trim();
    let argv = if tail.is_empty() {
        format!("docker {subcommand} -- {id}")
    } else {
        format!("docker {subcommand} {tail} -- {id}")
    };
    Ok(wrap(elev, &argv))
}

fn checked_id(id: &str) -> Result<&str, ()> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > CONTAINER_ID_MAX
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(());
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct() -> Elevation {
        Elevation::direct()
    }

    #[test]
    fn ps_always_has_path_and_json_format() {
        let cmd = ps(true, &direct());
        assert!(cmd.starts_with(PATH_PREFIX));
        assert!(cmd.contains("docker ps --all --format '{{json .}}'"));
        assert!(ps(false, &direct()).contains("docker ps --format"));
    }

    #[test]
    fn sudo_nopass_prefix_wraps_every_docker_call() {
        let elev = Elevation::sudo_nopass("");
        let cmd = ps(true, &elev);
        assert!(cmd.contains("sudo -n env PATH=\"$PATH\" docker ps --all"));
        let seq = remove_sequence("web", &elev).unwrap();
        assert_eq!(seq.matches("sudo -n env PATH=\"$PATH\" docker").count(), 2);
        assert!(seq.contains("docker rm -- web"));
    }

    #[test]
    fn sudo_password_and_target_user() {
        let elev = Elevation::sudo_password("deploy");
        let cmd = ps(false, &elev);
        assert!(cmd.contains("LC_ALL=C sudo -S -p 'Password:' -u deploy env PATH=\"$PATH\""));
        // root 是默认目标，不写 -u。
        let elev = Elevation::sudo_password("root");
        assert!(!ps(false, &elev).contains(" -u "));
    }

    #[test]
    fn su_mode_quotes_inner_command() {
        let elev = Elevation::su("root");
        let cmd = ps(true, &elev);
        assert!(cmd.starts_with("LC_ALL=C su - root -c '"));
        // 内层 json 格式里的单引号必须被转义，否则会截断 su -c 的参数。
        assert!(cmd.contains(r"'\''{{json .}}'\''"));
    }

    #[test]
    fn account_names_are_sanitized() {
        let elev = Elevation::sudo_password("bad; rm -rf /");
        assert!(ps(false, &elev).contains(" -u root "));
        let elev = Elevation::su("");
        assert!(ps(false, &elev).starts_with("LC_ALL=C su - root -c "));
    }

    #[test]
    fn actions_quote_and_validate_container_ids() {
        assert!(start("web_1", &direct())
            .unwrap()
            .ends_with("docker start -- web_1"));
        assert!(stop("9f2c", &direct())
            .unwrap()
            .contains("docker stop -t 5 -- 9f2c"));
        assert!(restart("web-1", &direct())
            .unwrap()
            .contains("restart -- web-1"));
        assert!(remove("web", true, &direct())
            .unwrap()
            .contains("docker rm -f -- web"));
        assert!(start("a b; rm -rf /", &direct()).is_err());
        assert!(start("", &direct()).is_err());
        assert!(start(&"x".repeat(CONTAINER_ID_MAX + 1), &direct()).is_err());
    }

    #[test]
    fn logs_clamps_tail_and_quotes_id() {
        let cmd = logs("web", 9999, true, &direct()).unwrap();
        assert!(cmd.contains("--tail 2000 --timestamps -- web"));
        let cmd = logs("web", 0, false, &direct()).unwrap();
        assert!(cmd.contains("--tail 1 "));
        assert!(logs("bad name", 10, false, &direct()).is_err());
    }

    #[test]
    fn exec_uses_interactive_shell() {
        let cmd = exec_shell("web", &direct()).unwrap();
        assert!(cmd.contains("docker exec -it -- web sh"));
        assert!(exec_shell("web", &Elevation::sudo_nopass(""))
            .unwrap()
            .contains("sudo -n env PATH=\"$PATH\" docker exec -it"));
    }

    #[test]
    fn service_commands_follow_the_elevation() {
        assert!(service_start(&Elevation::sudo_nopass(""))
            .contains("sudo -n env PATH=\"$PATH\" systemctl start docker"));
        assert!(service_start(&direct()).ends_with("systemctl start docker"));
        assert!(service_stop(&Elevation::sudo_nopass(""))
            .contains("sudo -n env PATH=\"$PATH\" systemctl stop docker"));
        assert!(service_stop(&direct()).ends_with("systemctl stop docker"));
        assert!(!probe().contains("sudo -S"));
    }

    #[test]
    fn probe_emits_all_four_markers() {
        let cmd = probe();
        assert!(cmd.contains("echo RUDDER_ACTIVE=$("));
        assert!(cmd.contains("echo RUDDER_ENABLED=$("));
        assert!(cmd.contains("echo RUDDER_DIRECT=1"));
        assert!(cmd.contains("echo RUDDER_SUDO=1"));
    }

    #[test]
    fn probe_parser_is_order_tolerant() {
        let text = "RUDDER_SUDO=0\nnoise line\nRUDDER_DIRECT=1\nRUDDER_ACTIVE=active\nRUDDER_ENABLED=disabled\n";
        let probe = parse_probe(text);
        assert_eq!(probe.sudo_nopass, Some(false));
        assert_eq!(probe.direct, Some(true));
        assert_eq!(probe.service_up, Some(true));
        assert_eq!(probe.service_enabled, Some(false));
    }

    #[test]
    fn probe_parser_leaves_unknown_values_none() {
        let probe = parse_probe("RUDDER_ACTIVE=activating\nRUDDER_DIRECT=\n");
        assert_eq!(probe.service_up, None);
        assert_eq!(probe.direct, None);
        assert!(probe.is_empty());
    }
}
