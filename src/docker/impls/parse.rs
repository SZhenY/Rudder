//! Docker CLI 输出解析。纯函数，输入输出都不碰 Slint / SSH。

use std::collections::HashMap;

use serde::Deserialize;

use crate::docker::model::{
    ComposeProjectInfo, ContainerState, DockerContainerInfo, DockerImageInfo, DockerNetworkInfo,
    RegistryRepo,
};

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
    /// `k=v,k=v`；compose 项目的容器带 `com.docker.compose.project`。
    #[serde(rename = "Labels", default)]
    labels: String,
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
                    ports: mapped_ports(&row.ports),
                    created: row.created,
                    compose_project: compose_project(&row.labels),
                });
            }
            Err(_) => bad_lines += 1,
        }
    }
    ParseResult { rows, bad_lines }
}

/// 通用逐行解析结果：单行坏数据只计数，不影响其它行。
pub(crate) struct Parsed<T> {
    pub(crate) rows: Vec<T>,
    pub(crate) bad_lines: usize,
}

#[derive(Deserialize)]
struct ImageRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Repository", default)]
    repository: String,
    #[serde(rename = "Tag", default)]
    tag: String,
    #[serde(rename = "Size", default)]
    size: String,
    #[serde(rename = "CreatedAt", default)]
    created: String,
}

/// `docker images --format '{{json .}}'`。
pub(crate) fn parse_images(stdout: &str) -> Parsed<DockerImageInfo> {
    let mut rows = Vec::new();
    let mut bad_lines = 0;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ImageRow>(line) {
            Ok(row) => {
                let dangling = row.repository == "<none>" || row.repository.is_empty();
                let tag = if row.tag.is_empty() {
                    "<none>".to_string()
                } else {
                    row.tag.clone()
                };
                rows.push(DockerImageInfo {
                    display_name: format!("{}:{}", row.repository, tag),
                    id: row.id,
                    repository: row.repository,
                    tag,
                    size: row.size,
                    created: short_created(&row.created),
                    dangling,
                });
            }
            Err(_) => bad_lines += 1,
        }
    }
    Parsed { rows, bad_lines }
}

#[derive(Deserialize)]
struct ComposeRow {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "ConfigFiles", default)]
    config_files: String,
}

/// `docker compose ls --all --format json`。
///
/// 正常输出是一段 JSON 数组（可能带缩进、可能是 `[]` / `null`）。老版本仍走
/// legacy `{{json .}}` 时，多个对象首尾相接且不带换行，用流式解析兜底。
pub(crate) fn parse_compose_ls(stdout: &str) -> Parsed<ComposeProjectInfo> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Parsed {
            rows: Vec::new(),
            bad_lines: 0,
        };
    }
    if let Ok(raw) = serde_json::from_str::<Vec<ComposeRow>>(trimmed) {
        return Parsed {
            rows: compose_rows(raw),
            bad_lines: 0,
        };
    }
    let mut raw = Vec::new();
    let mut bad_lines = 0;
    for item in serde_json::Deserializer::from_str(trimmed).into_iter::<ComposeRow>() {
        match item {
            Ok(row) => raw.push(row),
            Err(_) => {
                bad_lines += 1;
                break;
            }
        }
    }
    Parsed {
        rows: compose_rows(raw),
        bad_lines,
    }
}

fn compose_rows(raw: Vec<ComposeRow>) -> Vec<ComposeProjectInfo> {
    raw.into_iter()
        .map(|row| {
            let (running, containers) = ComposeProjectInfo::parse_status(&row.status);
            ComposeProjectInfo {
                name: row.name,
                status: row.status,
                running,
                containers,
                config_files: row.config_files,
            }
        })
        .collect()
}

/// `docker ps` 的 `Labels` 里取 compose 项目名。
fn compose_project(labels: &str) -> Option<String> {
    labels.split(',').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        let value = value.trim();
        (key.trim() == "com.docker.compose.project" && !value.is_empty())
            .then(|| value.to_string())
    })
}

#[derive(Deserialize)]
struct NetworkRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Driver", default)]
    driver: String,
    #[serde(rename = "Scope", default)]
    scope: String,
}

/// `docker network ls --format '{{json .}}'`。
pub(crate) fn parse_network_ls(stdout: &str) -> Parsed<DockerNetworkInfo> {
    let mut rows = Vec::new();
    let mut bad_lines = 0;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<NetworkRow>(line) {
            Ok(row) => rows.push(DockerNetworkInfo {
                id: row.id,
                name: row.name,
                driver: row.driver,
                scope: row.scope,
            }),
            Err(_) => bad_lines += 1,
        }
    }
    Parsed { rows, bad_lines }
}

/// 一个容器所属的网络集合。
pub(crate) struct ContainerNetworks {
    pub(crate) name: String,
    pub(crate) networks: Vec<String>,
}

/// `docker inspect --format '{{.Name}}|{{.HostConfig.NetworkMode}}|{{range $k,$v := .NetworkSettings.Networks}}{{$k}},{{end}}'`。
///
/// 从容器侧汇总，`docker ps -a` 里的容器全部参与，停止的也算在内。所属网络是
/// `NetworkSettings.Networks` 的键与 `HostConfig.NetworkMode` 的并集（容器从未启动
/// 时后者仍有 `bridge`），排除 `default` 与 `container:<id>` 这类非网络名。
pub(crate) fn parse_container_networks(stdout: &str) -> Vec<ContainerNetworks> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.split('|');
        let name = parts.next().unwrap_or("").trim().trim_start_matches('/');
        if name.is_empty() {
            continue;
        }
        let mode = parts.next().unwrap_or("").trim();
        let list = parts.next().unwrap_or("").trim();
        let mut networks: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !mode.is_empty()
            && mode != "default"
            && !mode.starts_with("container:")
            && !networks.iter().any(|n| n == mode)
        {
            networks.push(mode.to_string());
        }
        if networks.is_empty() {
            continue;
        }
        out.push(ContainerNetworks {
            name: name.to_string(),
            networks,
        });
    }
    out
}

/// 镜像 ID 归一化：去掉 `sha256:`，取前 12 位小写十六进制。
pub(crate) fn normalize_image_id(raw: &str) -> String {
    let raw = raw.trim();
    let hex = raw.strip_prefix("sha256:").unwrap_or(raw);
    hex.chars().take(12).collect::<String>().to_ascii_lowercase()
}

/// `docker inspect --format '{{.Image}}'`：运行中容器引用的镜像 ID，每行一个。
///
/// 调用方只传运行中容器的 id，所以计数就是「被多少运行中容器使用」。非十六进制的
/// 行（例如混进来的报错）直接跳过，避免生成假 key。
pub(crate) fn parse_image_usage(stdout: &str) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    for line in stdout.lines() {
        let raw = line.trim();
        let hex = raw.strip_prefix("sha256:").unwrap_or(raw);
        if hex.len() < 12 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        *out.entry(normalize_image_id(raw)).or_insert(0) += 1;
    }
    out
}

/// `docker ps` 快照里 state == Running 的容器 id，保持 ps 顺序。
///
/// 「使用中」只算运行中的容器；与概览的「N 运行」同一判据。
pub(crate) fn running_container_ids(containers: &[DockerContainerInfo]) -> Vec<String> {
    containers
        .iter()
        .filter(|item| item.state == ContainerState::Running)
        .map(|item| item.id.clone())
        .collect()
}

/// 容器 inspect 输出 → 网络名 → 容器名（排序、去重）。
///
/// 含已停止容器：`docker network inspect` 的成员只含运行中的，停止后 endpooint
/// 消失，所以统一从容器侧汇总。空输入返回空 map（=「确实没有容器」）。
pub(crate) fn network_members_by_network(stdout: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for container in parse_container_networks(stdout) {
        for network in container.networks {
            map.entry(network).or_default().push(container.name.clone());
        }
    }
    for names in map.values_mut() {
        names.sort();
        names.dedup();
    }
    map
}

#[derive(Deserialize)]
struct SearchRow {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Description", default)]
    description: String,
    #[serde(rename = "StarCount", default)]
    star_count: String,
    #[serde(rename = "IsOfficial", default)]
    is_official: String,
}

/// `docker search --format '{{json .}}'`：走远端 daemon 的 registry 配置 / 加速源。
pub(crate) fn parse_docker_search(stdout: &str) -> Parsed<RegistryRepo> {
    let mut rows = Vec::new();
    let mut bad_lines = 0;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<SearchRow>(line) {
            Ok(row) => {
                let stars = row.star_count.trim().parse::<i64>().unwrap_or(0);
                let official = row.is_official.eq_ignore_ascii_case("true")
                    || row.is_official.eq_ignore_ascii_case("[ok]");
                rows.push(RegistryRepo {
                    name: row.name,
                    description: row.description,
                    pulls: 0,
                    stars,
                    display_pulls: String::new(),
                    official,
                });
            }
            Err(_) => bad_lines += 1,
        }
    }
    Parsed { rows, bad_lines }
}

fn short_created(raw: &str) -> String {
    raw.trim().chars().take(19).collect()
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

/// `docker version --format '{{.Server.Version}}'`：取首行，空 / 含空白 / 超长视为未知。
pub(crate) fn parse_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    (!line.is_empty() && line.len() <= 64 && !line.contains(char::is_whitespace))
        .then(|| line.to_string())
}

/// `Names` 可能是逗号分隔的多个别名，列表只显示第一个。
fn first_name(names: &str) -> &str {
    names.split(',').next().unwrap_or("").trim()
}

/// 只保留真实端口映射（`host:port->container/tcp`）。
///
/// `docker ps` 的 `Ports` 里还会混着只声明未发布的项（如 `80/tcp`），以及 host
/// 网络容器的空串；这些对用户没有信息量，列表里一律不展示。
fn mapped_ports(ports: &str) -> String {
    split_ports(ports).join(", ")
}

/// 把 docker 的 `Ports` 字段拆成「每条映射一行」。
///
/// docker 用 `,` 分隔多个映射；只保留已发布项（含 `->`），
/// 只声明未发布的 `80/tcp` 与空串都不返回。
pub(crate) fn split_ports(ports: &str) -> Vec<String> {
    ports
        .split(',')
        .map(str::trim)
        .filter(|item| item.contains("->"))
        .map(str::to_string)
        .collect()
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
    fn split_ports_returns_each_published_mapping() {
        assert_eq!(
            split_ports("0.0.0.0:3300->3300/tcp, :::3300->3300/tcp"),
            vec!["0.0.0.0:3300->3300/tcp", ":::3300->3300/tcp"]
        );
        assert_eq!(
            split_ports("0.0.0.0:8080->80/tcp, 80/tcp"),
            vec!["0.0.0.0:8080->80/tcp"]
        );
        assert!(split_ports("80/tcp").is_empty());
        assert!(split_ports("").is_empty());
    }

    #[test]
    fn ports_keep_only_published_mappings() {
        assert_eq!(
            mapped_ports("0.0.0.0:8080->80/tcp, 80/tcp"),
            "0.0.0.0:8080->80/tcp"
        );
        assert_eq!(
            mapped_ports("0.0.0.0:80->80/tcp, :::80->80/tcp"),
            "0.0.0.0:80->80/tcp, :::80->80/tcp"
        );
        assert_eq!(mapped_ports("80/tcp"), "");
        assert_eq!(mapped_ports(""), "");
    }

    #[test]
    fn parses_images_json_lines() {
        let stdout = concat!(
            r#"{"ID":"sha256:abc","Repository":"alpine","Tag":"3.22","Size":"8.14MB","CreatedAt":"2026-11-06 02:01:07 +0800 CST"}"#,
            "\n",
            r#"{"ID":"sha256:def","Repository":"<none>","Tag":"<none>","Size":"0B","CreatedAt":"2026-01-01 00:00:00 +0000 UTC"}"#,
            "\nnot-json\n"
        );
        let out = parse_images(stdout);
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.bad_lines, 1);
        assert_eq!(out.rows[0].display_name, "alpine:3.22");
        assert_eq!(out.rows[0].created, "2026-11-06 02:01:07");
        assert!(!out.rows[0].dangling);
        assert!(out.rows[1].dangling);
    }

    #[test]
    fn parses_compose_status_and_rows() {
        assert_eq!(ComposeProjectInfo::parse_status("running(2)"), (true, 2));
        assert_eq!(ComposeProjectInfo::parse_status("exited(0)"), (false, 0));
        assert_eq!(ComposeProjectInfo::parse_status("unknown"), (false, 0));

        // `--format json`：带缩进的数组。
        let out = parse_compose_ls(
            r#"[
  {"Name":"easytier","Status":"running(1)","ConfigFiles":"/opt/easytier/docker-compose.yml"},
  {"Name":"homebox","Status":"exited(0)","ConfigFiles":"/opt/homebox/docker-compose.yml"}
]"#,
        );
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.bad_lines, 0);
        assert!(out.rows[0].running);
        assert!(!out.rows[1].running);
        assert!(out.rows[1].config_files.ends_with("docker-compose.yml"));

        // 老版本 legacy `{{json .}}`：多个对象首尾相接、没有换行。
        let out = parse_compose_ls(
            r#"{"Name":"a","Status":"running(2)","ConfigFiles":"/a.yml"}{"Name":"b","Status":"running(1)","ConfigFiles":"/b.yml"}"#,
        );
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0].name, "a");
        assert_eq!(out.rows[0].containers, 2);

        assert!(parse_compose_ls("[]").rows.is_empty());
        assert!(parse_compose_ls("null").rows.is_empty());
        assert!(parse_compose_ls("").rows.is_empty());
        assert_eq!(parse_compose_ls("not-json").bad_lines, 1);
    }

    #[test]
    fn parses_networks_and_container_membership() {
        let out = parse_network_ls(concat!(
            r#"{"ID":"n1","Name":"bridge","Driver":"bridge","Scope":"local"}"#,
            "\nbad\n"
        ));
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.bad_lines, 1);
        assert_eq!(out.rows[0].driver, "bridge");

        let rows = parse_container_networks(concat!(
            "/web|bridge|backend,bridge,\n",
            "/old|default|bridge,\n",
            "/agent|container:abc||\n"
        ));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "web");
        assert_eq!(rows[0].networks, vec!["backend", "bridge"]);
        assert_eq!(rows[1].name, "old");
        assert_eq!(rows[1].networks, vec!["bridge"]);
    }

    #[test]
    fn counts_running_container_images() {
        // 用户三台运行中容器引用的镜像；已停止的 easytier（452191afc864）不在这里。
        let usage = parse_image_usage(concat!(
            "sha256:062964a69725aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            "sha256:d0aa3e6f1600bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
            "sha256:75dcd3f28c57cccccccccccccccccccccccccccccccccccccccccccccccc\n"
        ));
        assert_eq!(usage.len(), 3);
        assert_eq!(usage.get("062964a69725").copied(), Some(1));
        assert_eq!(usage.get("d0aa3e6f1600").copied(), Some(1));
        assert_eq!(usage.get("75dcd3f28c57").copied(), Some(1));
        assert!(!usage.contains_key("452191afc864"));

        // 同一个镜像跑了两个运行中容器。
        let twice = parse_image_usage("sha256:062964a69725\n062964a69725\n");
        assert_eq!(twice.get("062964a69725").copied(), Some(2));

        // 混进来的报错行不能变成假 key。
        assert!(parse_image_usage("Error: No such object: abc\n").is_empty());
        assert!(parse_image_usage("").is_empty());
    }

    #[test]
    fn running_ids_skip_stopped_and_paused() {
        let ps = concat!(
            r#"{"ID":"c1","Names":"web","Image":"nginx","State":"running","Status":"Up"}"#,
            "\n",
            r#"{"ID":"c2","Names":"old","Image":"alpine","State":"exited","Status":"Exited"}"#,
            "\n",
            r#"{"ID":"c3","Names":"paused","Image":"redis","State":"paused","Status":"Up (Paused)"}"#,
            "\n",
            r#"{"ID":"c4","Names":"db","Image":"postgres","State":"running","Status":"Up"}"#
        );
        let ids = running_container_ids(&parse_ps(ps).rows);
        assert_eq!(ids, vec!["c1".to_string(), "c4".to_string()]);
        assert!(running_container_ids(&[]).is_empty());
    }

    #[test]
    fn network_members_group_sort_and_dedup() {
        let map = network_members_by_network(concat!(
            "/web|bridge|backend,bridge,\n",
            "/web2|bridge|bridge,backend,\n",
            "/old|default|bridge,\n",
            "/agent|container:abc||\n"
        ));
        assert_eq!(map["backend"], vec!["web", "web2"]);
        assert_eq!(map["bridge"], vec!["old", "web", "web2"]);
        assert!(!map.contains_key("container:abc"));
        assert!(network_members_by_network("").is_empty());
    }

    #[test]
    fn normalizes_image_ids_for_matching() {
        assert_eq!(normalize_image_id("sha256:452191afc8641111"), "452191afc864");
        assert_eq!(normalize_image_id(" 062964A69725 "), "062964a69725");
    }

    #[test]
    fn ps_reads_compose_project_label() {
        let out = parse_ps(
            r#"{"ID":"c1","Names":"web","Image":"nginx","State":"running","Status":"Up","Labels":"com.docker.compose.project=easytier,com.docker.compose.service=web"}"#,
        );
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].compose_project.as_deref(), Some("easytier"));

        let out = parse_ps(
            r#"{"ID":"c2","Names":"solo","Image":"alpine","State":"exited","Status":"Exited","Labels":""}"#,
        );
        assert!(out.rows[0].compose_project.is_none());
    }

    #[test]
    fn parses_docker_search_rows() {
        let out = parse_docker_search(concat!(
            r#"{"Description":"Official build of Nginx.","IsAutomated":"false","IsOfficial":"true","Name":"nginx","StarCount":"21000"}"#,
            "\n",
            r#"{"Description":"community","IsAutomated":"false","IsOfficial":"false","Name":"someone/app","StarCount":"12"}"#,
            "\nnot-json\n"
        ));
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.bad_lines, 1);
        assert_eq!(out.rows[0].name, "nginx");
        assert!(out.rows[0].official);
        assert_eq!(out.rows[0].stars, 21000);
        assert!(!out.rows[1].official);
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

    #[test]
    fn version_accepts_a_single_token_and_rejects_noise() {
        assert_eq!(parse_version("27.1.1\n"), Some("27.1.1".to_string()));
        assert_eq!(parse_version("24.0.7"), Some("24.0.7".to_string()));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("  \n"), None);
        assert_eq!(parse_version("cannot connect to the daemon"), None);
    }
}
