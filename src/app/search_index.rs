//! 全局搜索索引（左侧边栏搜索框用）。
//!
//! 为什么过滤放在 Rust 侧：Slint 表达式里**没有 `contains`**，"标题是否包含
//! 关键词"写不出来，只能在 Rust 做完再把结果数组回写给界面。
//!
//! 一期覆盖两类目标：
//!   · **页面 / 设置分类** —— 分类关键词表沿用被删掉的设置页导航搜索那一套。
//!   · **主机** —— 名称 / 地址 / 用户 / 分组 / 备注。
//!
//! 二期可以给 `SearchHit` 再挂"设置项"粒度（把设置页改成数据驱动后，
//! 按分类过滤不够用，需要精确到项 + 滚动定位）。

use crate::ui::SearchHit;

/// 一条参与搜索的主机（由调用方从 `SessionInfo` 摘出来，避免这里依赖 UI 类型）。
pub(crate) struct HostEntry {
    pub name: slint::SharedString,
    pub host: slint::SharedString,
    pub user: slint::SharedString,
    pub group: slint::SharedString,
    pub note: slint::SharedString,
}

/// (中英标题, 关键词, page, cat)。`cat = -1` 表示这是页面而非设置分类。
const PAGE_HITS: &[(&str, &str, i32, i32)] = &[
    ("主机 Hosts", "host server 服务器 连接到 machine", 0, -1),
    ("终端 Terminal", "terminal 终端 会话 shell console", 1, -1),
    ("指令集 Snippets", "snippet 指令集 命令 片段", 2, -1),
    ("Docker", "docker 容器 镜像 container 部署", 3, -1),
    ("笔记随笔 Notes", "note 笔记 随笔 便签", 5, -1),
    ("AI 助手 Assistant", "ai 助手 模型", 6, -1),
    ("设置 Settings", "settings 设置 配置 preferences", 4, -1),
];

/// 设置页分类：`(标题, cat id, 关键词)`。cat id 与 `page_settings.slint` 的
/// `current-cat` 取值一致（0 基础 / 1 终端 / 2 文件 / 4 快捷键 / 6 云同步 / 5 关于）。
const CAT_HITS: &[(&str, i32, &str)] = &[
    ("基础", 0, "基础 general language 语言 主题 更新 功能入口 theme accent wallpaper 壁纸 缩放 scale 动画 animation 渲染器 renderer 更新渠道 channel"),
    ("终端", 1, "终端 terminal fonts 字体 cursor 光标 scrollback 滚回 osc52 高亮 highlight 规则 rule preset 预设"),
    ("文件", 2, "文件 files sftp 分区 partitions 传输 transfer eol json 下载 download 挂载 mount filter 过滤 折叠 collapse"),
    ("快捷键", 4, "快捷键 shortcuts keys 全局 global 终端快捷键"),
    ("云同步", 6, "云同步 cloud webdav 同步 sync 上传 upload 下载 download"),
    ("关于", 5, "关于 about 版本 version 日志 log 反馈 feedback libs"),
];

/// 大小写不敏感的子串匹配。空关键词视为不匹配。
fn contains_ci(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && hay.to_lowercase().contains(needle)
}

/// 跑一次搜索，结果按「页面 → 分类 → 主机」排序，且同名匹配优先。
pub(crate) fn run(query: &str, hosts: &[HostEntry]) -> Vec<SearchHit> {
    let q = query.trim();
    let ql = q.to_lowercase();
    // 空关键词 = **浏览全部**，不是"无结果"。
    //
    // 折叠态点搜索按钮要列出所有可跳转项（不展开侧栏、直接给列表），走的就是空查询。
    // 这里用"恒真谓词"顶替 `contains_ci` 的"空关键词不匹配"，而不是复制一份排序
    // 逻辑 —— 顺序仍是「页面 → 分类 → 主机」，同级按命中字段的优先级排。
    let browsing = ql.is_empty();
    let m = |hay: &str| browsing || contains_ci(hay, &ql);
    // (排序键, 命中权重) —— 权重小的排前面：标题命中 0，正文命中 1。
    let mut scored: Vec<(u8, u8, SearchHit)> = Vec::new();

    for (title, kw, page, cat) in PAGE_HITS {
        if *cat < 0 && (m(title) || m(kw)) {
            let in_title = m(title);
            scored.push((
                0,
                if in_title { 0 } else { 1 },
                SearchHit {
                    kind: 0,
                    index: *page,
                    page: *page,
                    title: (*title).into(),
                    subtitle: "页面".into(),
                },
            ));
        }
    }

    for (title, cat, kw) in CAT_HITS {
        let in_title = contains_ci(title, &ql);
        if in_title || contains_ci(kw, &ql) {
            scored.push((
                1,
                if in_title { 0 } else { 1 },
                SearchHit {
                    kind: 1,
                    index: *cat,
                    page: 4,
                    title: (*title).into(),
                    subtitle: "设置分类".into(),
                },
            ));
        }
    }

    for (i, h) in hosts.iter().enumerate() {
        // 名称 > 分组 > 地址 > 用户 > 备注（备注多为长句，放最后）
        let fields = [&h.name, &h.group, &h.host, &h.user, &h.note];
        let level = fields
            .iter()
            .position(|f| contains_ci(f, &ql))
            .map(|n| n as u8);
        if let Some(level) = level {
            let hit = SearchHit {
                kind: 2,
                index: i as i32,
                page: 0,
                title: h.name.clone(),
                subtitle: h.subtitle(),
            };
            scored.push((2, level, hit));
        }
    }

    scored.sort_by_key(|(group, level, _)| (*group, *level));
    scored.into_iter().map(|(_, _, h)| h).collect()
}

impl HostEntry {
    /// 副标题：优先"分组 · 用户@主机"，退到主机地址，都没有就用占位。
    fn subtitle(&self) -> slint::SharedString {
        if !self.user.is_empty() && !self.host.is_empty() {
            return if self.group.is_empty() {
                format!("{}@{}", self.user, self.host)
            } else {
                format!("{} · {}@{}", self.group, self.user, self.host)
            }
            .into();
        }
        if !self.host.is_empty() {
            return self.host.clone();
        }
        if !self.group.is_empty() {
            return self.group.clone();
        }
        "本机 / 尚未填写地址".into()
    }
}
