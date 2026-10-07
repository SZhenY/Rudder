//! 左侧边栏全局搜索的接线：过滤回调 + 结果跳转 + 主机卡片高亮。
//!
//! 过滤本身在 `search_index`（Slint 没有 `contains`）。这里只做三件事：
//!   1. 关键词 → 结果数组（回写 `search-hits`）；
//!   2. 选中结果 → 切页；
//!   3. 主机结果 → 对应卡片打 2.4 秒高亮，到点自动淡出。

use std::time::Duration;
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::app::search_index::{self, HostEntry};
use crate::ui::{AppWindow, SearchHit};

/// 高亮停留时长：2.4 秒足够目光找到，又不至于长期花哨。
const HIGHLIGHT_MS: u64 = 2400;

/// 浏览全部（空查询）时最多列多少条 —— 结果浮层没有滚动容器，再多就顶出屏幕了。
const BROWSE_LIMIT: usize = 12;

/// 接线一次。`AppWindow` 侧已声明回调，这里挂实现。
pub(crate) fn wire(window: &AppWindow) {
    let weak = window.as_weak();
    let r = window;
    r.on_search_changed(move |query: slint::SharedString| {
        let Some(w) = weak.upgrade() else { return };
        let hits = filter(&w, query.as_str());
        let has_query = !query.trim().is_empty();
        w.set_search_query(query);
        w.set_search_hits(ModelRc::from(Rc::new(VecModel::from(hits))));
        // 结果变了游标回第一项，否则会停在越界下标上（结果变少时）。
        w.set_search_cursor(0);
        // 只有**真的输入了东西**才弹浮层。点一下搜索框就凭空冒出一整列可跳转项
        // 既突兀，又会顺着打开浮层抢走输入框焦点（用户反馈）。
        w.set_search_open(has_query);
    });

    let weak = window.as_weak();
    r.on_open_search(move || {
        let Some(w) = weak.upgrade() else { return };
        // 打开前重跑一次过滤：可能上次输入后结果已过期（例如刚导入新主机）。
        let hits = filter(&w, w.get_search_query().as_str());
        w.set_search_hits(ModelRc::from(Rc::new(VecModel::from(hits))));
        w.set_search_cursor(0);
        // ⚠ 只聚焦，**不**弹浮层：查询为空时弹一整列"可跳转项"不像是被搜索的
        // 结果（用户反馈"单击搜索框后会自动出现"）。浮层由打字触发。
        w.set_search_open(!w.get_search_query().trim().is_empty());
    });

    let weak = window.as_weak();
    r.on_search_key(move |delta: i32, kind: slint::SharedString| {
        let Some(w) = weak.upgrade() else { return };
        if kind == "esc" {
            w.set_search_open(false);
            return;
        }
        let n = w.get_search_hits().row_count() as i32;
        if n == 0 || delta == 0 {
            return;
        }
        // ↑(-1) 到底回末尾，↓(+1) 到头回 0（循环）。
        let cur = w.get_search_cursor();
        let next = if delta < 0 {
            if cur <= 0 { n - 1 } else { cur - 1 }
        } else if cur + 1 >= n {
            0
        } else {
            cur + 1
        };
        w.set_search_cursor(next);
    });

    let weak = window.as_weak();
    r.on_choose_hit(move |index: i32| {
        let Some(w) = weak.upgrade() else { return };
        let hits = w.get_search_hits();
        let i = index.max(0) as usize;
        if i >= hits.row_count() {
            return;
        }
        let Some(hit) = hits.row_data(i) else { return };
        w.set_search_open(false);
        // 清空输入：下次点开是干净的一格。
        w.set_search_query("".into());
        w.set_search_hits(ModelRc::from(Rc::new(VecModel::<SearchHit>::default())));
        w.set_shell_page(hit.page);
        if hit.kind == 2 {
            highlight_host(&w, hit.index);
        }
        if hit.kind == 2 {
            highlight_host(&w, hit.index);
        }
    });
}

/// 从 `sessions` 摘出可搜索字段再跑索引。
///
/// **空查询 = 浏览全部**（折叠态点搜索按钮就是这条路）：`search_index::run` 里的
/// 恒真谓词会列出所有页面 / 设置分类 / 主机。
///
/// 浏览态要限长：全部条目可能有几十条，而结果浮层**没有滚动容器**，一口气铺开
/// 会超出屏幕。只取前 `BROWSE_LIMIT` 条 —— 要找后面的项，用户继续输入关键词即可
/// （有关键字时走的是真过滤，不受这个上限影响）。
fn filter(window: &AppWindow, query: &str) -> Vec<SearchHit> {
    let browsing = query.trim().is_empty();
    let sessions = window.get_sessions();
    let hosts: Vec<HostEntry> = sessions
        .iter()
        .map(|s| HostEntry {
            name: s.name.clone(),
            host: s.host.clone(),
            user: s.user.clone(),
            group: s.group.clone(),
            note: s.note.clone(),
        })
        .collect();
    let mut hits = search_index::run(query, &hosts);
    if browsing && hits.len() > BROWSE_LIMIT {
        hits.truncate(BROWSE_LIMIT);
    }
    hits
}

/// 给第 index 个主机卡片打高亮，2.4 秒后自动清除。
fn highlight_host(window: &AppWindow, index: i32) {
    let sessions = window.get_sessions();
    let i = index.max(0) as usize;
    if i >= sessions.row_count() {
        return;
    }
    let Some(s) = sessions.row_data(i) else { return };
    let id = s.id.clone();
    let weak = window.as_weak();
    // 先立刻点亮（重新搜同一台主机时也能再次看到）。
    window.set_host_highlight(id);
    slint::Timer::single_shot(Duration::from_millis(HIGHLIGHT_MS), move || {
        if let Some(w) = weak.upgrade() {
            w.set_host_highlight("".into());
        }
    });
}
