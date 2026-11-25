//! Docker 容器与主机键的领域模型。
//!
//! 只放纯数据与解析，不依赖 Slint / SSH；UI、MCP、控制器共用同一套类型。

/// Docker 容器状态。`docker ps` 的 `State` 字段是权威值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContainerState {
    Running,
    Exited,
    Paused,
    Created,
    Restarting,
    Removing,
    Dead,
    Unknown,
}

impl ContainerState {
    pub(crate) fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "running" | "up" => Self::Running,
            s if s.starts_with("up ") => Self::Running,
            "exited" | "stopped" => Self::Exited,
            "paused" => Self::Paused,
            "created" => Self::Created,
            "restarting" => Self::Restarting,
            "removing" => Self::Removing,
            "dead" => Self::Dead,
            _ => Self::Unknown,
        }
    }

    /// Slint 侧用于比较的稳定 key。
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Paused => "paused",
            Self::Created => "created",
            Self::Restarting => "restarting",
            Self::Removing => "removing",
            Self::Dead => "dead",
            Self::Unknown => "unknown",
        }
    }

}

/// `docker images --format '{{json .}}'` 的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockerImageInfo {
    pub(crate) id: String,
    pub(crate) repository: String,
    pub(crate) tag: String,
    /// `repo:tag`，dangling 镜像为 `<none>:<none>`。
    pub(crate) display_name: String,
    pub(crate) size: String,
    pub(crate) created: String,
    pub(crate) dangling: bool,
}

/// `docker compose ls --format '{{json .}}'` 的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComposeProjectInfo {
    pub(crate) name: String,
    /// Docker 原文，如 `running(1)` / `exited(0)`。
    pub(crate) status: String,
    pub(crate) running: bool,
    pub(crate) containers: i32,
    pub(crate) config_files: String,
}

impl ComposeProjectInfo {
    /// 解析 `running(2)` / `exited(1)`；未知格式按停止处理、容器数为 0。
    pub(crate) fn parse_status(raw: &str) -> (bool, i32) {
        let raw = raw.trim();
        let (state, count) = match raw.split_once('(') {
            Some((state, rest)) => (state.trim(), rest.trim_end_matches(')').trim()),
            None => (raw, ""),
        };
        let running = state.eq_ignore_ascii_case("running");
        let containers = count.parse::<i32>().unwrap_or(0).max(0);
        (running, containers)
    }
}

/// `docker network ls` 的一行 + `network inspect` 关联的容器名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockerNetworkInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) driver: String,
    pub(crate) scope: String,
}

/// Docker Hub 搜索结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegistryRepo {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) pulls: i64,
    pub(crate) stars: i64,
    /// `1B+` / `120M` / `8.5K`，Rust 侧格式化；`docker search` 不提供时为空。
    pub(crate) display_pulls: String,
    /// Docker 官方镜像标记。
    pub(crate) official: bool,
}


/// `docker ps` 的一行；字段名与 `--format '{{json .}}'` 的输出一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockerContainerInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) state: ContainerState,
    /// Docker 原文，如 "Up 3 hours"。
    pub(crate) status: String,
    pub(crate) ports: String,
    pub(crate) created: String,
    /// `com.docker.compose.project` 标签；非 compose 容器为 `None`。
    pub(crate) compose_project: Option<String>,
}

/// 主机键：`user@host`。同一键的多个 SSH 会话共用一份 Docker 缓存。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DockerHostKey(String);

impl DockerHostKey {
    /// `host` 为空或明显不是 SSH 主机（本地 / 串口 / Telnet）时返回 `None`。
    pub(crate) fn parse(host: &str, user: &str) -> Option<Self> {
        let host = host.trim();
        if host.is_empty() || host.starts_with("local") || host.starts_with("serial") {
            return None;
        }
        let value = if host.contains('@') {
            host.to_string()
        } else {
            let user = user.trim();
            if user.is_empty() {
                host.to_string()
            } else {
                format!("{user}@{host}")
            }
        };
        Some(Self(normalize(&value)))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// host 段大小写不敏感，user 段原样；去掉首尾空白与重复空白。
fn normalize(value: &str) -> String {
    let value = value.trim();
    let Some((user, host)) = value.split_once('@') else {
        return value.to_ascii_lowercase();
    };
    format!("{}@{}", user.trim(), host.trim().to_ascii_lowercase())
}

/// 单个主机的 Docker 页状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostStatus {
    Idle,
    Loading,
    Ok,
    Empty,
    Error,
    Offline,
}

impl HostStatus {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Ok => "ok",
            Self::Empty => "empty",
            Self::Error => "error",
            Self::Offline => "disconnected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_does_not_double_prefix_user() {
        // TabStatus.host 已经是 conn_label(user@host)，不能再拼一次 user。
        let key = DockerHostKey::parse("root@10.0.0.5", "root").unwrap();
        assert_eq!(key.as_str(), "root@10.0.0.5");
        let key = DockerHostKey::parse("10.0.0.5", "root").unwrap();
        assert_eq!(key.as_str(), "root@10.0.0.5");
        assert_eq!(
            DockerHostKey::parse("ROOT@HOST.Example", "ignored").unwrap().as_str(),
            "ROOT@host.example"
        );
        assert!(DockerHostKey::parse("local prod", "root").is_none());
        assert!(DockerHostKey::parse("", "root").is_none());
    }

    #[test]
    fn container_state_parses_docker_words() {
        assert_eq!(ContainerState::parse("running"), ContainerState::Running);
        assert_eq!(ContainerState::parse("Up 3 hours"), ContainerState::Running);
        assert_eq!(ContainerState::parse("exited"), ContainerState::Exited);
        assert_eq!(ContainerState::parse("paused"), ContainerState::Paused);
        assert_eq!(ContainerState::parse("weird"), ContainerState::Unknown);
        assert_eq!(ContainerState::Running.key(), "running");
    }
}
