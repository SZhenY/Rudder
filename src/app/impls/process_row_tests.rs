use super::*;

#[test]
fn marks_owner_and_preserves_source_tab() {
    let input = vec![
        ProcInfo {
            pid: 10,
            user: "alice".into(),
            cpu: 1.0,
            mem: 2.0,
            command: "own".into(),
        },
        ProcInfo {
            pid: 11,
            user: "root".into(),
            cpu: 3.0,
            mem: 4.0,
            command: "other".into(),
        },
    ];
    let rows = proc_rows(&input, "alice", "term-a");
    assert!(rows[0].own_process);
    assert!(!rows[1].own_process);
    assert!(rows.iter().all(|row| row.tab_id.as_str() == "term-a"));
}

#[test]
fn privilege_rules_match_effective_login_user() {
    assert!(!process_needs_root("alice", "alice"));
    assert!(process_needs_root("alice", "root"));
    assert!(process_needs_root("alice", "bob"));
    assert!(!process_needs_root("root", "root"));
    assert!(!process_needs_root("root", "alice"));
}
