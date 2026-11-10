use super::*;

pub(crate) fn all_quick_group_names(store: &ConfigStore) -> std::collections::HashSet<String> {
    let cmds = store.quick_commands();
    let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
    if cmds.iter().any(|c| c.group.trim().is_empty()) {
        set.insert("default".to_string());
    }
    for g in store.quick_groups() {
        set.insert(g.clone());
    }
    for c in cmds {
        let g = c.group.trim();
        if !g.is_empty() {
            set.insert(g.to_string());
        }
    }
    set
}

/// 把一条命令压成**单行预览**：折叠行内空白、行间插 `⏎`、超长截断 —— 用于列表里显示
/// （上游 9725617）。**只用于显示**：运行 / 复制 / 回填一律仍用原始多行字符串，heredoc
/// 之类的换行不能被显示逻辑改坏。
pub(crate) fn command_preview(command: &str) -> String {
    const MAX_CHARS: usize = 240;
    const SEP: &str = " ⏎ ";

    let mut out = String::new();
    for (i, line) in command.lines().enumerate() {
        if i > 0 {
            out.push_str(SEP);
        }
        // 行内空白折叠成单个空格，首尾去掉（缩进的 heredoc 不会带出一串空格）。
        out.push_str(&line.split_whitespace().collect::<Vec<_>>().join(" "));
        if out.chars().count() > MAX_CHARS {
            let head: String = out.chars().take(MAX_CHARS).collect();
            return format!("{}…", head.trim_end());
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod command_preview_tests {
    use super::*;

    #[test]
    fn single_line_is_unchanged() {
        assert_eq!(command_preview("ls -la"), "ls -la");
    }

    #[test]
    fn newlines_collapse_to_one_line_with_a_marker() {
        assert_eq!(command_preview("a\nb"), "a ⏎ b");
        assert_eq!(command_preview("  a   b  \n  c  "), "a b ⏎ c");
    }

    /// 240 字符截断（多一个省略号）—— 列表行是固定高度的，不能让它撑成多行。
    #[test]
    fn long_commands_are_truncated_to_one_visible_line() {
        let preview = command_preview(&"x".repeat(1000));
        assert!(preview.chars().count() <= 241, "{}", preview.chars().count());
        assert!(preview.ends_with('…'));

        let many_lines = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let preview = command_preview(&many_lines);
        assert!(preview.chars().count() <= 241, "{}", preview.chars().count());
    }
}

pub(super) fn quick_cmd_model(
    store: &ConfigStore,
    collapsed_groups: &std::collections::HashSet<String>,
) -> ModelRc<QuickCmd> {
    let cmds = store.quick_commands();

    let has_default = cmds.iter().any(|c| c.group.trim().is_empty());
    // Named groups = explicit quick-groups ∪ groups referenced by commands.
    let named: Vec<String> = store
        .quick_groups()
        .iter()
        .cloned()
        .chain(
            cmds.iter()
                .map(|c| c.group.trim().to_string())
                .filter(|g| !g.is_empty()),
        )
        .collect();
    let named = crate::config::dedup_sorted(named);

    let mut groups: Vec<String> = Vec::new();
    if has_default {
        groups.push("default".to_string());
    }
    groups.extend(named);

    let mut rows: Vec<QuickCmd> = Vec::new();
    for group in &groups {
        let is_collapsed = collapsed_groups.contains(group);
        let members: Vec<(usize, &crate::config::QuickCommand)> = cmds
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let g = c.group.trim();
                if group == "default" {
                    g.is_empty()
                } else {
                    g == group
                }
            })
            .collect();
        if members.is_empty() {
            // Header-only placeholder for an empty group (orig_index -1) so it can
            // still be renamed / deleted, matching empty session folders.
            rows.push(QuickCmd {
                name: "".into(),
                command: "".into(),
                command_preview: "".into(),
                group: group.clone().into(),
                group_header: group.clone().into(),
                collapsed: is_collapsed,
                orig_index: -1,
                send_enter: true,
            });
        } else {
            for (i, (orig_idx, c)) in members.iter().enumerate() {
                rows.push(QuickCmd {
                    name: c.name.clone().into(),
                    command: c.command.clone().into(),
                    command_preview: command_preview(c.command.as_str()).into(),
                    group: group.clone().into(),
                    group_header: if i == 0 {
                        group.clone().into()
                    } else {
                        "".into()
                    },
                    collapsed: is_collapsed,
                    orig_index: *orig_idx as i32,
                    send_enter: c.send_enter,
                });
            }
        }
    }
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(crate) fn reorder_quick_command(
    commands: &mut [crate::config::QuickCommand],
    index: usize,
    move_up: bool,
) -> bool {
    let Some(current) = commands.get(index) else {
        return false;
    };
    let group = current.group.trim().to_string();
    let target = if move_up {
        (0..index)
            .rev()
            .find(|&candidate| commands[candidate].group.trim() == group)
    } else {
        (index + 1..commands.len())
            .find(|&candidate| commands[candidate].group.trim() == group)
    };
    if let Some(target) = target {
        commands.swap(index, target);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod reorder_tests {
    use super::reorder_quick_command;
    use crate::config::QuickCommand;

    fn command(name: &str, group: &str) -> QuickCommand {
        QuickCommand {
            name: name.to_string(),
            command: name.to_string(),
            group: group.to_string(),
            send_enter: true,
        }
    }

    #[test]
    fn reorders_only_within_the_current_group() {
        let mut commands = vec![
            command("a", "ops"),
            command("x", "other"),
            command("b", "ops"),
        ];
        assert!(reorder_quick_command(&mut commands, 2, true));
        assert_eq!(
            commands.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "x", "a"]
        );
        assert!(!reorder_quick_command(&mut commands, 0, true));
    }
}
