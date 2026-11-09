use super::*;

/// 从 `CHANGELOG.md` 里取出某个版本的段落（`## [0.7.9] - …` 到下一个 `## [` 之间）。
///
/// 为什么发布说明不只用 release 的 `body`：它是 GitHub 自动生成的（我们线上那几个 release
/// 实际上常常是空的），而 `CHANGELOG.md` 是**中英对照**手写的 —— 用户要看的就是那份。
/// 段落里带 Markdown，交给 [`release_notes`] 清洗。
///
/// `version` 不带 `v`（tag 去掉前缀后的形式）。
pub(crate) fn changelog_section(md: &str, version: &str) -> Option<String> {
    let want = format!("[{version}]");
    let mut started = false;
    let mut out: Vec<&str> = Vec::new();
    for line in md.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            if started {
                break; // 下一个版本段开始 → 本段结束
            }
            // `## [0.7.9] - 2026-09-19`；`[0.7.9]` 不会误配 `[0.7.9-beta1]`（差在中括号）。
            started = head.trim().starts_with(&want);
            continue;
        }
        if started {
            out.push(line);
        }
    }
    if !started || out.is_empty() {
        return None;
    }
    let text = release_notes(&out.join("\n"));
    if text.is_empty() { None } else { Some(text) }
}

/// 拉某个 tag 的 `CHANGELOG.md`（raw.githubusercontent）。失败返回 `None` ——
/// 调用方会退回 release 的 `body`，网络不好时不至于没说明可看。
///
/// 走与检查 / 下载同一个 Agent：这个域同样可能只有代理能通、同样可能被 TLS 中间人拦，
/// 用裸 `ureq::get` 会让「检查能出结果、发布说明却是空的」这种半失败很难解释。
pub(crate) fn fetch_changelog(tag: &str) -> Option<String> {
    let url =
        format!("https://raw.githubusercontent.com/{REPO_OWNER}/{REPO_NAME}/{tag}/CHANGELOG.md");
    let agent = match crate::app::update_net::current().client() {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!("update notes: {e}");
            return None;
        }
    };
    match agent
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
    {
        Ok(resp) => match read_body_capped(resp, MAX_CHANGELOG_BODY, "changelog") {
            // 与 releases 共用有上限读取，超限报错而非静默截断。
            Ok(text) => Some(text),
            Err(e) => {
                tracing::debug!("update notes: {e:#}");
                None
            }
        },
        Err(e) => {
            tracing::debug!(
                "update notes: fetching CHANGELOG failed: {}",
                crate::app::update_net::describe(&e)
            );
            None
        }
    }
}

/// 「自动更新」对话框里那块发布说明的内容：优先取 **CHANGELOG.md 里该版本的段落**
/// （中英对照、手写），取不到再退回 release 的 `body`。
pub(crate) fn notes_for(tag: &str, body: &str) -> String {
    let version = tag.trim().trim_start_matches('v');
    let notes = if let Some(md) = fetch_changelog(tag)
        && let Some(section) = changelog_section(&md, version)
    {
        section
    } else {
        release_notes(body)
    };
    // 给排查留一句话（debug 级，正式版默认 info 不打印）：这段说明来自哪儿、多长。
    tracing::debug!("update notes for {tag}: {} chars", notes.chars().count());
    notes
}
/// 发布说明的上限。对话框里那块是**可滚动**的，所以标准是"别把窗口撑坏"，而不是"少放点"：
/// 一个版本的 CHANGELOG 段落一般 2~3 千字符，都放得下；这里只拦异常离谱的输入。
/// 按**字符**（不是字节）算，中文说明不会被截成半个字。
pub(crate) const MAX_NOTES_CHARS: usize = 4000;

/// 把 release 的 `body` 清洗成能直接塞进普通 `Text` 的纯文本。
///
/// 为什么要洗：Slint 的 `Text` **不渲染 Markdown**，原样显示就是一堆 `##`、`**`、`- `
/// 符号。这里只做最保守的几步，不追求完整解析：
/// * HTML 注释（发布模板常留着 `<!-- ... -->`，可能跨行）整段丢掉；
/// * 行首 `#` / `>` 去掉，`- ` / `* ` 列表项换成 `• `；
/// * 行内 `**` / `__` / 反引号去掉；
/// * 连续空行压成一个，首尾空行去掉；
/// * 超长截断并加省略号。
pub(crate) fn release_notes(body: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_comment = false;
    let mut blanks = 0usize;

    for raw in body.lines() {
        let mut line = raw.trim().to_string();

        if in_comment {
            match line.find("-->") {
                Some(i) => {
                    line = line[i + 3..].trim().to_string();
                    in_comment = false;
                }
                None => continue,
            }
        }
        while let Some(start) = line.find("<!--") {
            match line[start + 4..].find("-->") {
                Some(rel) => {
                    let end = start + 4 + rel + 3;
                    line = format!("{}{}", &line[..start], &line[end..]);
                }
                None => {
                    line.truncate(start);
                    in_comment = true;
                    break;
                }
            }
        }

        let mut text = line.trim();
        text = text.trim_start_matches('#').trim();
        text = text.trim_start_matches('>').trim();
        let text = match text.strip_prefix("- ").or_else(|| text.strip_prefix("* ")) {
            Some(rest) => format!("• {}", rest.trim()),
            None => text.to_string(),
        };
        let text = text.replace("**", "").replace("__", "").replace('`', "");
        let text = text.trim_end().to_string();

        if text.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue; // 连续空行只留一个
            }
        } else {
            blanks = 0;
        }
        out.push(text);
    }

    // 首尾空行都清掉：对话框里那块框不该以空白开头或结尾（注释被整段删掉时，
    // 开头很容易留下一串空行）。
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let mut s = out.join("\n");
    if s.chars().count() > MAX_NOTES_CHARS {
        s = s.chars().take(MAX_NOTES_CHARS).collect::<String>();
        s.push('…');
    }
    s
}
