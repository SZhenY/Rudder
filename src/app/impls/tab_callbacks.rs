use super::*;

pub(super) fn wire_tab_callbacks(window: &AppWindow, ctx: &AppContext) {
    wire_tab_navigation(window, ctx);
    wire_tab_lifecycle(window, ctx);
    wire_tab_panes(window, ctx);
}

/// 把 `from` 位置的标签移动 `dir` 格（-1 左 / +1 右）；真的移动了才返回 true。
///
/// 抽成纯函数的原因：`from`/`dir` 直接来自 UI 拖拽事件（可能是任意整数），
/// 而 `Vec::remove` 一旦越界，release 构建（`panic = "abort"`）下是**整个客户端退出**。
/// 夹取与"到边界就什么都不做"必须能被单测钉住。
pub(crate) fn move_tab_in_place(tabs: &mut Vec<String>, from: i32, dir: i32) -> bool {
    let n = tabs.len() as i32;
    if n <= 1 {
        return false;
    }
    let from = from.clamp(0, n - 1);
    let to = (from + dir).clamp(0, n - 1);
    if from == to {
        return false;
    }
    let item = tabs.remove(from as usize);
    tabs.insert(to as usize, item);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn move_tab_in_place_swaps_with_the_neighbour() {
        let mut t = tabs(&["a", "b", "c"]);
        assert!(move_tab_in_place(&mut t, 1, -1));
        assert_eq!(t, tabs(&["b", "a", "c"]), "左移一格");
        assert!(move_tab_in_place(&mut t, 0, 1));
        assert_eq!(t, tabs(&["a", "b", "c"]), "再右移回来");
    }

    /// 到边界就什么都不做并返回 false —— 调用方据此跳过重排 UI。
    #[test]
    fn move_tab_in_place_reports_no_op_at_the_edges() {
        let mut t = tabs(&["a", "b", "c"]);
        assert!(!move_tab_in_place(&mut t, 0, -1), "最左还往左");
        assert!(!move_tab_in_place(&mut t, 2, 1), "最右还往右");
        assert!(!move_tab_in_place(&mut t, 1, 0), "dir = 0");
        assert_eq!(t, tabs(&["a", "b", "c"]), "一次都不该动");
    }

    /// 单个 / 空列表直接拒绝；越界索引必须夹住（否则 `Vec::remove` panic -> 进程退出）。
    #[test]
    fn move_tab_in_place_clamps_hostile_indices() {
        let mut one = tabs(&["only"]);
        assert!(!move_tab_in_place(&mut one, 0, 1));
        assert!(!move_tab_in_place(&mut one, 99, -1), "越界也不能 panic");
        let mut empty: Vec<String> = Vec::new();
        assert!(!move_tab_in_place(&mut empty, 0, 1));

        let mut t = tabs(&["a", "b", "c"]);
        assert!(move_tab_in_place(&mut t, 99, -1), "越界的 from 夹到末尾后再移动");
        assert_eq!(t, tabs(&["a", "c", "b"]));
    }
}
