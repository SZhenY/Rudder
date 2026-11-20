//! 把 `docker` 的 stderr / 退出码归到可展示、可判定的失败类型。

use crate::docker::failure::DockerFailure;

pub(crate) fn classify(stderr: &str, exit_code: Option<i32>, timed_out: bool) -> DockerFailure {
    if timed_out {
        return DockerFailure::Timeout;
    }
    let msg = stderr.to_ascii_lowercase();
    if msg.contains("permission denied") || msg.contains("got permission denied") {
        return DockerFailure::NoPermission;
    }
    if msg.contains("command not found") || msg.contains("executable file not found") {
        return DockerFailure::NoCli;
    }
    if msg.contains("cannot connect to the docker daemon")
        || msg.contains("is the docker daemon running")
        || msg.contains("connection refused")
    {
        return DockerFailure::DaemonDown;
    }
    if msg.contains("no such container") || msg.contains("no such object") {
        return DockerFailure::NotFound;
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
        assert_eq!(classify("", None, true), DockerFailure::Timeout);
        assert_eq!(classify("something else", Some(1), false), DockerFailure::Other);
    }
}
