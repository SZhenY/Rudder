//! Docker CLI 输出解析。纯函数，输入输出都不碰 Slint / SSH。

use serde::Deserialize;

use crate::docker::model::{ContainerState, DockerContainerInfo};

/// `docker ps --format '{{json .}}'` 的一行。
#[derive(Deserialize)]
struct PsRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Ports", default)]
    ports: String,
    #[serde(rename = "CreatedAt", default)]
    created: String,
}

pub(crate) struct ParseResult {
    pub(crate) rows: Vec<DockerContainerInfo>,
    pub(crate) bad_lines: usize,
}

/// 逐行解析 `docker ps`。单行坏数据只计数，不影响其它行。
pub(crate) fn parse_ps(stdout: &str) -> ParseResult {
    let mut rows = Vec::new();
    let mut bad_lines = 0;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<PsRow>(line) {
            Ok(row) => {
                let state = ContainerState::parse(&row.state);
                rows.push(DockerContainerInfo {
                    id: row.id,
                    name: first_name(&row.names).to_string(),
                    image: row.image,
                    state,
                    status: row.status,
                    ports: row.ports,
                    created: row.created,
                });
            }
            Err(_) => bad_lines += 1,
        }
    }
    ParseResult { rows, bad_lines }
}

/// `docker logs` 的 stdout/stderr 合并：stdout 在前，stderr 追加；按行截断到 `max_lines`。
pub(crate) fn parse_logs(stdout: &str, stderr: &str, max_lines: usize) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = stdout.lines().map(ToOwned::to_owned).collect();
    if !stderr.trim().is_empty() {
        lines.extend(stderr.lines().map(ToOwned::to_owned));
    }
    let max_lines = max_lines.max(1);
    let truncated = lines.len() > max_lines;
    if truncated {
        lines.drain(..lines.len() - max_lines);
    }
    (lines, truncated)
}

/// `Names` 可能是逗号分隔的多个别名，列表只显示第一个。
fn first_name(names: &str) -> &str {
    names.split(',').next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_name_takes_the_leading_alias() {
        assert_eq!(first_name("web,web-1"), "web");
        assert_eq!(first_name(" solo "), "solo");
        assert_eq!(first_name(""), "");
    }

    #[test]
    fn parses_json_lines_and_ignores_bad_rows() {
        let stdout = concat!(
            r#"{"ID":"abc","Names":"web,web-1","Image":"nginx:1","State":"running","Status":"Up 3h","Ports":"0.0.0.0:80->80/tcp","CreatedAt":"2026-11-20"}"#,
            "\n",
            "not-json\n",
            r#"{"ID":"def","Names":"db","Image":"pg:16","State":"exited","Status":"Exited (0) 2h ago"}"#,
        );
        let out = parse_ps(stdout);
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.bad_lines, 1);
        assert_eq!(out.rows[0].name, "web");
        assert_eq!(out.rows[0].state, ContainerState::Running);
        assert_eq!(out.rows[1].state, ContainerState::Exited);
        assert!(out.rows[1].ports.is_empty());
        assert!(out.rows[1].created.is_empty());
    }

    #[test]
    fn empty_output_is_empty_result() {
        let out = parse_ps("\n \n");
        assert!(out.rows.is_empty());
        assert_eq!(out.bad_lines, 0);
    }

    #[test]
    fn logs_merge_and_tail() {
        let (lines, truncated) = parse_logs("a\nb\nc\n", "err\n", 2);
        assert_eq!(lines, vec!["c".to_string(), "err".to_string()]);
        assert!(truncated);
        let (lines, truncated) = parse_logs("only\n", "", 10);
        assert_eq!(lines, vec!["only".to_string()]);
        assert!(!truncated);
    }
}
