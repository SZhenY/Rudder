//! 把 `docker` 的 stderr / 退出码归到可展示、可判定的失败类型。

use crate::docker::failure::DockerFailure;

pub(crate) fn classify(stderr: &str, exit_code: Option<i32>, timed_out: bool) -> DockerFailure {
    if timed_out {
        return DockerFailure::Timeout;
    }
    let msg = stderr.to_ascii_lowercase();
    // sudo / su 已经拿到密码但被拒：与「没权限读 socket」区分开，UI 好引导重配凭据。
    if msg.contains("sorry, try again")
        || msg.contains("incorrect password attempt")
        || msg.contains("no password was provided")
        || msg.contains("authentication failure")
    {
        return DockerFailure::SudoAuthFailed;
    }
    // 镜像仓库认证：`pull access denied` 也含 "access denied"，必须排在 NoPermission 前。
    if msg.contains("unauthorized: authentication required")
        || msg.contains("pull access denied")
        || msg.contains("requested access to the resource is denied")
        || msg.contains("no basic auth credentials")
    {
        return DockerFailure::RegistryAuth;
    }
    // compose v2 插件缺失时 docker 的固定报错。
    if msg.contains("'compose' is not a docker command")
        || msg.contains("compose is not a docker command")
    {
        return DockerFailure::NoCompose;
    }
    if msg.contains("permission denied") || msg.contains("got permission denied") {
        return DockerFailure::NoPermission;
    }
    // `sudo -n` 没有免密授权时的固定报错。
    if msg.contains("sudo: a password is required") || msg.contains("a terminal is required") {
        return DockerFailure::NoPermission;
    }
    // 未提权直接跑 `systemctl start docker` / docker 命令时的常见拒绝。
    if msg.contains("interactive authentication required")
        || msg.contains("authentication is required")
        || msg.contains("access denied")
    {
        return DockerFailure::NoPermission;
    }
    if msg.contains("command not found") || msg.contains("executable file not found") {
        return DockerFailure::NoCli;
    }
    if msg.contains("docker.service is not running") || msg.contains("failed to start docker") {
        return DockerFailure::DaemonDown;
    }
    if msg.contains("cannot connect to the docker daemon")
        || msg.contains("is the docker daemon running")
        || msg.contains("connection refused")
    {
        return DockerFailure::DaemonDown;
    }
    if msg.contains("no such container") || msg.contains("no such object") || msg.contains("manifest unknown")
    {
        return DockerFailure::NotFound;
    }
    if msg.contains("toomanyrequests")
        || msg.contains("tls handshake timeout")
        || msg.contains("i/o timeout")
        || msg.contains("dial tcp")
    {
        return DockerFailure::Timeout;
    }
    if msg.contains("is already in use")
        || msg.contains("container is not running")
        || msg.contains("conflict")
    {
        return DockerFailure::Conflict;
    }
    if exit_code == Some(137) || exit_code == Some(143) {
        return DockerFailure::Other;
    }
    DockerFailure::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_failures() {
        assert_eq!(
            classify("permission denied while trying to connect", Some(1), false),
            DockerFailure::NoPermission
        );
        assert_eq!(
            classify("bash: docker: command not found", Some(127), false),
            DockerFailure::NoCli
        );
        assert_eq!(
            classify("Cannot connect to the Docker daemon", Some(1), false),
            DockerFailure::DaemonDown
        );
        assert_eq!(
            classify("Error: No such container: web", Some(1), false),
            DockerFailure::NotFound
        );
        assert_eq!(
            classify("sudo: a password is required", Some(1), false),
            DockerFailure::NoPermission
        );
        assert_eq!(
            classify("Sorry, try again.\nsudo: 1 incorrect password attempt", Some(1), false),
            DockerFailure::SudoAuthFailed
        );
        assert_eq!(
            classify("docker.service is not running", Some(1), false),
            DockerFailure::DaemonDown
        );
        assert_eq!(
            classify("Interactive authentication required.", Some(1), false),
            DockerFailure::NoPermission
        );
        assert_eq!(
            classify("Failed to start docker.service: Access denied", Some(1), false),
            DockerFailure::NoPermission
        );
        assert_eq!(
            classify(
                "Error response from daemon: unauthorized: authentication required",
                Some(1),
                false
            ),
            DockerFailure::RegistryAuth
        );
        assert_eq!(
            classify(
                "pull access denied for private/app, repository does not exist or may require 'docker login'",
                Some(1),
                false
            ),
            DockerFailure::RegistryAuth
        );
        assert_eq!(
            classify("docker: 'compose' is not a docker command", Some(1), false),
            DockerFailure::NoCompose
        );
        assert_eq!(
            classify("Error response from daemon: manifest unknown", Some(1), false),
            DockerFailure::NotFound
        );
        assert_eq!(
            classify("net/http: TLS handshake timeout", Some(1), false),
            DockerFailure::Timeout
        );
        // 文件类报错不能落进「容器不存在」。
        assert_eq!(
            classify(
                "stat /opt/app/docker-compose.yml: no such file or directory",
                Some(1),
                false
            ),
            DockerFailure::Other
        );
        assert_eq!(classify("", None, true), DockerFailure::Timeout);
        assert_eq!(classify("something else", Some(1), false), DockerFailure::Other);
    }
}
