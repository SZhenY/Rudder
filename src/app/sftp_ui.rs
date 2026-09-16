use super::*;

pub(super) fn terminal_sftp_paths(w: &AppWindow) -> HashMap<String, String> {
    use slint::Model as _;
    let mut out = HashMap::new();
    let model = w.get_terminals();
    if let Some(terminals) = model.as_any().downcast_ref::<VecModel<TerminalState>>() {
        for i in 0..terminals.row_count() {
            if let Some(row) = terminals.row_data(i) {
                out.insert(row.id.to_string(), row.sftp_path.to_string());
            }
        }
    }
    out
}

pub(super) fn sorted_sftp_entries_from_model(
    model: &ModelRc<SftpEntry>,
    key: &str,
    dir: i32,
) -> ModelRc<SftpEntry> {
    let Some(vec_model) = model.as_any().downcast_ref::<VecModel<SftpEntry>>() else {
        return model.clone();
    };
    let mut entries = Vec::with_capacity(vec_model.row_count());
    for i in 0..vec_model.row_count() {
        if let Some(entry) = vec_model.row_data(i) {
            entries.push(entry);
        }
    }
    sort_sftp_entries(&mut entries, key, dir);
    ModelRc::from(std::rc::Rc::new(VecModel::from(entries)))
}

pub(super) fn sort_sftp_entries(entries: &mut [SftpEntry], key: &str, dir: i32) {
    use std::cmp::Ordering;

    let name_cmp = |a: &SftpEntry, b: &SftpEntry| natural_name_cmp(&a.name, &b.name);
    let default_cmp = |a: &SftpEntry, b: &SftpEntry| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => name_cmp(a, b),
    };

    if dir == 0 || key.is_empty() {
        entries.sort_by(default_cmp);
        return;
    }

    entries.sort_by(|a, b| {
        let group = match (a.is_dir, b.is_dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        };
        if group != Ordering::Equal {
            return group;
        }
        let ord = match key {
            "size" => a
                .size_bytes
                .partial_cmp(&b.size_bytes)
                .unwrap_or(Ordering::Equal)
                .then_with(|| default_cmp(a, b)),
            "modified" => a
                .modified_ts
                .partial_cmp(&b.modified_ts)
                .unwrap_or(Ordering::Equal)
                .then_with(|| default_cmp(a, b)),
            _ => name_cmp(a, b).then_with(|| default_cmp(a, b)),
        };
        if dir < 0 { ord.reverse() } else { ord }
    });
}

pub(super) fn natural_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    natural_ascii_cmp(&a.to_lowercase(), &b.to_lowercase()).then_with(|| a.cmp(b))
}

pub(super) fn natural_ascii_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut ai = 0;
    let mut bi = 0;
    while ai < ab.len() && bi < bb.len() {
        let ad = ab[ai].is_ascii_digit();
        let bd = bb[bi].is_ascii_digit();
        if ad && bd {
            let a_start = ai;
            let b_start = bi;
            while ai < ab.len() && ab[ai].is_ascii_digit() {
                ai += 1;
            }
            while bi < bb.len() && bb[bi].is_ascii_digit() {
                bi += 1;
            }

            let mut a_sig = a_start;
            let mut b_sig = b_start;
            while a_sig < ai && ab[a_sig] == b'0' {
                a_sig += 1;
            }
            while b_sig < bi && bb[b_sig] == b'0' {
                b_sig += 1;
            }

            let a_len = ai - a_sig;
            let b_len = bi - b_sig;
            let ord = a_len
                .cmp(&b_len)
                .then_with(|| ab[a_sig..ai].cmp(&bb[b_sig..bi]))
                .then_with(|| (ai - a_start).cmp(&(bi - b_start)));
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }

        let ord = ab[ai].cmp(&bb[bi]);
        if ord != Ordering::Equal {
            return ord;
        }
        ai += 1;
        bi += 1;
    }
    ab.len().cmp(&bb.len())
}

pub(super) fn collect_sftp_selected(
    terminals: &VecModel<TerminalState>,
    tab_id: &str,
) -> Vec<String> {
    let mut paths = Vec::new();
    for ti in 0..terminals.row_count() {
        let Some(row) = terminals.row_data(ti) else {
            continue;
        };
        if row.id.as_str() != tab_id {
            continue;
        }
        if let Some(em) = row
            .sftp_entries
            .as_any()
            .downcast_ref::<VecModel<SftpEntry>>()
        {
            for ei in 0..em.row_count() {
                if let Some(e) = em.row_data(ei)
                    && e.selected
                {
                    paths.push(e.full_path.to_string());
                }
            }
        }
        break;
    }
    paths
}

pub(super) fn clear_sftp_selection(terminals: &VecModel<TerminalState>, tab_id: &str) {
    for ti in 0..terminals.row_count() {
        let Some(row) = terminals.row_data(ti) else {
            continue;
        };
        if row.id.as_str() != tab_id {
            continue;
        }
        if let Some(em) = row
            .sftp_entries
            .as_any()
            .downcast_ref::<VecModel<SftpEntry>>()
        {
            for ei in 0..em.row_count() {
                if let Some(mut e) = em.row_data(ei)
                    && e.selected
                {
                    e.selected = false;
                    em.set_row_data(ei, e);
                }
            }
        }
        let mut r = row.clone();
        r.sftp_selected_count = 0;
        terminals.set_row_data(ti, r);
        break;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn entry(name: &str, is_dir: bool, size: f32) -> SftpEntry {
        SftpEntry {
            name: name.into(),
            full_path: format!("/{name}").into(),
            is_dir,
            size: String::new().into(),
            size_bytes: size,
            modified: String::new().into(),
            modified_ts: 0.0,
            mode: 0o644,
            selected: false,
        }
    }

    fn names(rows: &[SftpEntry]) -> Vec<String> {
        rows.iter().map(|e| e.name.to_string()).collect()
    }

    /// 数字段按**数值**比较：`file10` 排在 `file2` 之后（最经典的用户可见排序 bug）。
    #[test]
    fn natural_ascii_cmp_orders_digit_runs_numerically() {
        assert_eq!(natural_ascii_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_ascii_cmp("file10", "file2"), Ordering::Greater);
        assert_eq!(natural_ascii_cmp("v1.2", "v1.10"), Ordering::Less);
    }

    /// 数值相同时**前导零多的排后面** —— 有个稳定的 tiebreak，列表才不会在
    /// `a01`/`a1` 之间来回抖。
    #[test]
    fn natural_ascii_cmp_tie_breaks_on_leading_zeros() {
        assert_eq!(natural_ascii_cmp("a01", "a1"), Ordering::Greater);
        assert_eq!(natural_ascii_cmp("a1", "a01"), Ordering::Less);
        assert_eq!(natural_ascii_cmp("a1", "a1"), Ordering::Equal);
    }

    #[test]
    fn natural_ascii_cmp_handles_prefixes_and_empty() {
        assert_eq!(natural_ascii_cmp("a1b", "a1b2"), Ordering::Less, "前缀短的小");
        assert_eq!(natural_ascii_cmp("", "a"), Ordering::Less);
        assert_eq!(natural_ascii_cmp("", ""), Ordering::Equal);
    }

    /// 名称比较忽略大小写，**同小写时回退原串**（保证全序、结果可复现）。
    #[test]
    fn natural_name_cmp_is_case_insensitive_then_ordinal() {
        assert_eq!(natural_name_cmp("B", "a"), Ordering::Greater);
        assert_eq!(
            natural_name_cmp("File1", "file1"),
            Ordering::Less,
            "小写相同 → 按原串（大写在前）"
        );
    }

    /// **目录永远在最前**，即使按大小倒序 —— 反向排序不能把目录翻到文件下面去
    /// （分组比较在反转之前就返回了）。
    #[test]
    fn sort_keeps_directories_first_in_every_mode() {
        let mut rows = vec![
            entry("b.txt", false, 100.0),
            entry("adir", true, 0.0),
            entry("a.txt", false, 5.0),
        ];
        sort_sftp_entries(&mut rows, "size", 1);
        assert_eq!(names(&rows), vec!["adir", "a.txt", "b.txt"], "按大小升序");

        sort_sftp_entries(&mut rows, "size", -1);
        assert_eq!(
            names(&rows),
            vec!["adir", "b.txt", "a.txt"],
            "按大小降序，但目录仍在最前"
        );
    }

    #[test]
    fn sort_falls_back_to_natural_names() {
        let mut rows = vec![entry("file10", false, 1.0), entry("file2", false, 1.0)];
        sort_sftp_entries(&mut rows, "", 0);
        assert_eq!(names(&rows), vec!["file2", "file10"], "key 为空 = 名称序");
        sort_sftp_entries(&mut rows, "whatever", 1);
        assert_eq!(names(&rows), vec!["file2", "file10"], "未知 key 也走名称序");
    }

    /// 值相同时用名称兜底 —— 否则 `sort_by` 的结果依赖输入顺序，列表会莫名抖动。
    #[test]
    fn sort_tie_breaks_on_name() {
        let mut rows = vec![
            entry("z.txt", false, 42.0),
            entry("a.txt", false, 42.0),
        ];
        sort_sftp_entries(&mut rows, "size", 1);
        assert_eq!(names(&rows), vec!["a.txt", "z.txt"]);
    }
}
