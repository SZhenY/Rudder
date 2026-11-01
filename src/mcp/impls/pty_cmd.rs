//! PTY 通道的命令包裹与输出切片 —— 纯函数，便于单测。
//!
//! 为什么需要它：交互式 PTY 没有 exec 通道那种"一次调用一份结果"的天然边界。
//! 终端快照（`TextSnapshot.text`）是整屏 / 整段滚动区合并后的纯文本，既没有行号
//! 偏移也没有命令分隔。要取"这条命令自己的输出"，只能自己插一对 marker 再从快照里切。
//!
//! marker 形如 `<RS>RUDDER_MCP_1a2b3c4d BEGIN` / `... END 0`：`<RS>` 是控制字符
//! 0x1e（Record Separator），在终端里占位极窄，几乎不会自然出现；token 里带序号，
//! 保证同一条命令的输出不会和上一次的 marker 撞上。

/// marker 行前缀：RS（0x1e）。
pub(crate) const MARKER: char = '\u{1e}';

/// 终端里那条 shell 的家族 —— 决定 marker 的组装语法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellFamily {
    /// POSIX shell（SSH 远端、非 Windows 的本地标签）。
    Posix,
    /// Windows PowerShell（本地标签的默认 shell）。
    PowerShell,
    /// Windows cmd.exe（用户显式选了 cmd 时）。
    Cmd,
}

/// 一次调用的唯一 token。序号来自 `McpState::next_epoch()`（跨租约单调）。
pub(crate) fn new_token(seq: u64) -> String {
    format!("RUDDER_MCP_{seq:08x}")
}

/// 把用户命令包成"带起止 marker 的一行"，交给 PTY 执行。
///
/// 退出码靠 `$?` / `$LASTEXITCODE` / `%ERRORLEVEL%` 在**同一条命令串**里取到 ——
/// 所以必须用 `{ …; }` 这种同 shell 分组，不能上 `$(…)`（那会开子 shell，拿到的是
/// 子 shell 的退出码）。
pub(crate) fn wrap(shell: ShellFamily, token: &str, command: &str) -> String {
    match shell {
        ShellFamily::Posix => format!(
            "printf '\\036{token} BEGIN\\n'; {{ {command}; }}; printf '\\036{token} END %s\\n' $?"
        ),
        // PowerShell 的 `$LASTEXITCODE` 只对原生命令有效；cmdlet 走 `$?`（True/False），
        // 这里按数字约定优先，取不到时 END 行没有数字，切片会得到 `code: None`。
        ShellFamily::PowerShell => format!(
            "Write-Output ([char]0x1e + '{token} BEGIN'); {command}; Write-Output ([char]0x1e + '{token} END ' + $LASTEXITCODE)"
        ),
        ShellFamily::Cmd => format!("echo {token} BEGIN & {command} & echo {token} END %ERRORLEVEL%"),
    }
}

/// 从快照里切出来的一条命令结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Extracted {
    /// BEGIN 与 END 之间的原始行（保留缩进与空行）。
    pub(crate) text: String,
    /// END 行上的数字；PowerShell 非原生命令等情况拿不到。
    pub(crate) code: Option<i32>,
    /// BEGIN 与 END 都找到了。
    pub(crate) found: bool,
}

/// 在快照文本里定位 `token` 的 BEGIN / END，返回中间的内容与退出码。
///
/// 定位规则是"**整行**等于 `<token> BEGIN`"（去掉前导 RS 与首尾空白后），这样
/// PTY 回显的那行 `printf '\036<token> BEGIN\n'; …` 会被自然跳过 —— 它开头是
/// `printf`，整行并不等于 marker。
pub(crate) fn extract(token: &str, snapshot: &str) -> Extracted {
    let begin = format!("{token} BEGIN");
    let end = format!("{token} END");

    let lines: Vec<&str> = snapshot.lines().collect();
    let mut begin_at: Option<usize> = None;
    let mut end_at: Option<usize> = None;
    let mut code: Option<i32> = None;

    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim_start_matches(MARKER).trim();
        if begin_at.is_none() {
            if line == begin {
                begin_at = Some(idx);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(&end) {
            code = rest.trim().parse::<i32>().ok();
            end_at = Some(idx);
            break;
        }
    }

    let (text, found) = match (begin_at, end_at) {
        (Some(begin), Some(end)) if end > begin => (
            lines[begin + 1..end].join("\n"),
            true,
        ),
        _ => (String::new(), false),
    };
    Extracted { text, code, found }
}

#[cfg(test)]
mod tests {
    use super::{Extracted, ShellFamily, extract, new_token, wrap};

    #[test]
    fn posix_wrap_keeps_exit_code_in_the_same_shell() {
        let line = wrap(ShellFamily::Posix, "RUDDER_MCP_00000001", "uname -a");
        assert!(line.starts_with("printf '\\036RUDDER_MCP_00000001 BEGIN\\n'; "));
        // 必须用 `{ …; }` 而不是 `$(…)`，否则 `$?` 取到的是子 shell 的退出码。
        assert!(line.contains("{ uname -a; }"));
        assert!(line.ends_with("printf '\\036RUDDER_MCP_00000001 END %s\\n' $?"));
    }

    #[test]
    fn powershell_and_cmd_have_their_own_grammar() {
        let ps = wrap(ShellFamily::PowerShell, "T", "Get-Date");
        assert!(ps.contains("[char]0x1e"));
        assert!(ps.contains("$LASTEXITCODE"));
        let cmd = wrap(ShellFamily::Cmd, "T", "dir");
        assert!(cmd.contains("%ERRORLEVEL%"));
        assert!(cmd.contains(" & "));
    }

    /// 真实快照里会同时出现「命令回显行」和「printf 吐出的 marker 行」，
    /// 切片必须只认后者，否则会把整条命令串当成输出。
    #[test]
    fn extract_skips_the_echoed_command_line() {
        let token = new_token(0x1a2b);
        let snapshot = format!(
            "$ printf '\\036{token} BEGIN\\n'; {{ ls; }}; printf '\\036{token} END %s\\n' $?\n\
             \u{1e}{token} BEGIN\n\
             Cargo.toml\n\
             src\n\
             \u{1e}{token} END 0\n\
             $ "
        );
        assert_eq!(
            extract(&token, &snapshot),
            Extracted {
                text: "Cargo.toml\nsrc".to_string(),
                code: Some(0),
                found: true,
            }
        );
    }

    /// marker 剥不干净（终端没渲染 RS）时也要能切 —— 定位靠 token，不靠控制字符。
    #[test]
    fn extract_works_without_the_rs_prefix() {
        let token = new_token(7);
        let snapshot = format!("{token} BEGIN\nhi\n{token} END 3\n");
        let out = extract(&token, &snapshot);
        assert!(out.found);
        assert_eq!(out.text, "hi");
        assert_eq!(out.code, Some(3));
    }

    /// 命令把 shell 弄崩了 / 被用户输入打断时，marker 可能永远不出现。
    #[test]
    fn missing_marker_reports_not_found() {
        let out = extract("RUDDER_MCP_dead", "just some output\n$ ");
        assert!(!out.found);
        assert_eq!(out.code, None);
        assert_eq!(out.text, "");
    }

    /// END 行没有数字（PowerShell 的 cmdlet 调用）时退出码为 None，但内容仍然有效。
    #[test]
    fn end_line_without_a_number_still_yields_text() {
        let token = new_token(9);
        let snapshot = format!("{token} BEGIN\nok\n{token} END \n");
        let out = extract(&token, &snapshot);
        assert!(out.found);
        assert_eq!(out.code, None);
        assert_eq!(out.text, "ok");
    }
}
