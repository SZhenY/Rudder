//! Docker CLI 命令构造。
//!
//! 远端 SSH exec 是非交互 shell，PATH 常常不含 `/usr/local/bin`；所有命令统一带
//! PATH 前缀。容器 id / name 先做字符白名单校验，避免把 MCP / UI 参数拼进 shell。

pub(crate) const PATH_PREFIX: &str =
    "PATH=/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/snap/bin; export PATH; ";

const CONTAINER_ID_MAX: usize = 128;

/// `docker ps` 输出每行一个 JSON 对象，字段由 `PsRow` 解析。
pub(crate) fn ps(all: bool) -> String {
    let all = if all { " --all" } else { "" };
    format!(
        "{PATH_PREFIX}docker ps{all} --format '{{{{json .}}}}'"
    )
}

pub(crate) fn start(id: &str) -> Result<String, ()> {
    with_id("start", id, "")
}

pub(crate) fn stop(id: &str) -> Result<String, ()> {
    with_id("stop", id, "-t 5 ")
}

pub(crate) fn restart(id: &str) -> Result<String, ()> {
    with_id("restart", id, "")
}

pub(crate) fn remove(id: &str, force: bool) -> Result<String, ()> {
    with_id("rm", id, if force { "-f " } else { "" })
}

/// 删除：运行中先 `stop -t 5`，再 `rm`；stop 失败不阻断 rm（已停止时 stop 会报错）。
pub(crate) fn remove_sequence(id: &str) -> Result<String, ()> {
    let id = checked_id(id)?;
    Ok(format!(
        "{PATH_PREFIX}docker stop -t 5 -- {id} >/dev/null 2>&1 || true; docker rm -- {id}"
    ))
}

pub(crate) fn logs(id: &str, tail: u32, timestamps: bool) -> Result<String, ()> {
    let id = checked_id(id)?;
    let tail = tail.clamp(1, 2000);
    let ts = if timestamps { "--timestamps " } else { "" };
    Ok(format!(
        "{PATH_PREFIX}docker logs --tail {tail} {ts}-- {id}"
    ))
}

pub(crate) fn exec_shell(id: &str) -> Result<String, ()> {
    let id = checked_id(id)?;
    Ok(format!("{PATH_PREFIX}docker exec -it -- {id} sh"))
}

fn with_id(subcommand: &str, id: &str, tail: &str) -> Result<String, ()> {
    let id = checked_id(id)?;
    let tail = tail.trim();
    Ok(if tail.is_empty() {
        format!("{PATH_PREFIX}docker {subcommand} -- {id}")
    } else {
        format!("{PATH_PREFIX}docker {subcommand} {tail} -- {id}")
    })
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

    #[test]
    fn ps_always_has_path_and_json_format() {
        let cmd = ps(true);
        assert!(cmd.starts_with(PATH_PREFIX));
        assert!(cmd.contains("docker ps --all --format '{{json .}}'"));
        assert!(ps(false).contains("docker ps --format"));
    }

    #[test]
    fn actions_quote_and_validate_container_ids() {
        assert!(start("web_1").unwrap().ends_with("docker start -- web_1"));
        assert!(stop("9f2c").unwrap().contains("docker stop -t 5 -- 9f2c"));
        assert!(restart("web-1").unwrap().contains("restart -- web-1"));
        assert!(remove("web", true).unwrap().contains("docker rm -f -- web"));
        assert!(start("a b; rm -rf /").is_err());
        assert!(start("").is_err());
        assert!(start(&"x".repeat(CONTAINER_ID_MAX + 1)).is_err());
    }

    #[test]
    fn logs_clamps_tail_and_quotes_id() {
        let cmd = logs("web", 9999, true).unwrap();
        assert!(cmd.contains("--tail 2000 --timestamps -- web"));
        let cmd = logs("web", 0, false).unwrap();
        assert!(cmd.contains("--tail 1 "));
        assert!(logs("bad name", 10, false).is_err());
    }

    #[test]
    fn exec_uses_interactive_shell() {
        let cmd = exec_shell("web").unwrap();
        assert!(cmd.contains("docker exec -it -- web sh"));
    }
}
