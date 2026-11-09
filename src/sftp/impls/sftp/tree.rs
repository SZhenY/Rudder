use super::*;

/// Rebuild the flat tree node list from the current cache and push it to the UI.
pub(crate) fn emit_tree(
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    tree_expanded: &std::collections::HashSet<String>,
    events: &UnboundedSender<SessionEvent>,
) {
    let mut nodes = Vec::new();
    build_tree_nodes("/", 0, tree_expanded, tree_dirs, &mut nodes);
    let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
}

/// Re-fetch a directory's sub-directories into the tree cache, but only if that
/// directory is already known to the tree (root or previously expanded) — so a
/// mutation under a collapsed/unknown branch doesn't graft unrelated nodes in.
/// This is how create/delete/rename keep the left tree in sync without a
/// reconnect (#189).
pub(crate) async fn sync_tree_dir(
    sftp: &SftpSession,
    dir: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
) {
    if tree_dirs.contains_key(dir) {
        let dirs = list_dirs_only_impl(sftp, dir).await.unwrap_or_default();
        tree_dirs.insert(dir.to_string(), dirs);
    }
}
