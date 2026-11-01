//! 权限位元数据与集合运算。
//!
//! 12 个位只有这一份表：连接授权弹窗、AI 页的客户端卡、`get_server_info` 与审计
//! 全部从这里派生。表按「风险 → 功能区」排好序，UI 只按顺序渲染，不再自己分组。
//!
//! `Permissions::default()` 恒为空集 —— 它是"这个客户端还没被授予任何能力"，
//! 不是"出厂默认"。权限只活在会话（`ClientLease`）上，配置里没有全局权限位。

use serde_json::{Map, Value};

/// 权限模型的对外版本。1 = 旧的 7 位全局模型；2 = 12 位、按客户端授权。
pub(crate) const PERMISSIONS_REV: u32 = 2;

/// 两级风险。常规位可以默认出现；高风险位必须用户逐次勾选。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Risk {
    Normal,
    High,
}

impl Risk {
    pub(crate) fn label_zh(self) -> &'static str {
        match self {
            Self::Normal => "常规",
            Self::High => "高风险",
        }
    }

    pub(crate) fn label_en(self) -> &'static str {
        match self {
            Self::Normal => "Routine",
            Self::High => "High risk",
        }
    }

    pub(crate) fn note_zh(self) -> &'static str {
        match self {
            Self::Normal => "只读与基础会话操作；仍需在连接弹窗里逐项确认。",
            Self::High => "涉及任意执行、凭据、信任边界或不可逆写入；默认不勾选。",
        }
    }

    pub(crate) fn note_en(self) -> &'static str {
        match self {
            Self::Normal => "Read-only and basic session actions; still confirmed per connection.",
            Self::High => {
                "Arbitrary execution, credentials, trust boundaries or irreversible writes; off by default."
            }
        }
    }
}

/// 功能区。风险相同时按这个顺序聚合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    View,
    Session,
    Terminal,
    Config,
    Host,
    File,
}

impl Group {
    pub(crate) fn label_zh(self) -> &'static str {
        match self {
            Self::View => "查看",
            Self::Session => "会话",
            Self::Terminal => "终端与命令",
            Self::Config => "配置",
            Self::Host => "主机",
            Self::File => "文件",
        }
    }

    pub(crate) fn label_en(self) -> &'static str {
        match self {
            Self::View => "View",
            Self::Session => "Sessions",
            Self::Terminal => "Terminal & commands",
            Self::Config => "Settings",
            Self::Host => "Hosts",
            Self::File => "Files",
        }
    }
}

/// 12 个授权位。字段名与 `PERMISSION_BITS.key` 一一对应。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Permissions {
    pub(crate) read: bool,
    pub(crate) session_open_close: bool,
    pub(crate) settings_write: bool,
    pub(crate) host_key_trust: bool,
    pub(crate) terminal_input: bool,
    pub(crate) exec: bool,
    pub(crate) local_exec: bool,
    pub(crate) settings_high: bool,
    pub(crate) host_upsert: bool,
    pub(crate) host_delete: bool,
    pub(crate) file_download: bool,
    pub(crate) file_upload: bool,
}

/// 一位权限的展示与访问元数据。
pub(crate) struct PermissionBit {
    pub(crate) key: &'static str,
    /// 结构体字段名，测试据此确认 key 拼写与字段一致。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) field: &'static str,
    pub(crate) risk: Risk,
    pub(crate) group: Group,
    pub(crate) label_zh: &'static str,
    pub(crate) label_en: &'static str,
    /// 客户端卡/摘要用的短名。
    pub(crate) short_zh: &'static str,
    pub(crate) short_en: &'static str,
    pub(crate) desc_zh: &'static str,
    pub(crate) desc_en: &'static str,
    pub(crate) get: fn(&Permissions) -> bool,
    pub(crate) set: fn(&mut Permissions, bool),
}

/// 展示顺序：先常规、后高风险；同一风险内按功能区聚合。
pub(crate) const PERMISSION_BITS: &[PermissionBit] = &[
    PermissionBit {
        key: "read",
        field: "read",
        risk: Risk::Normal,
        group: Group::View,
        label_zh: "读取终端与配置",
        label_en: "Read terminals & settings",
        short_zh: "读取",
        short_en: "read",
        desc_zh: "看屏幕内容、标签页、设置与远端目录；屏幕上出现过的敏感信息也可能被读到",
        desc_en: "screens, tabs, settings and remote listings; may include secrets printed on screen",
        get: |p: &Permissions| p.read,
        set: |p: &mut Permissions, v: bool| p.read = v,
    },
    PermissionBit {
        key: "session_open_close",
        field: "session_open_close",
        risk: Risk::Normal,
        group: Group::Session,
        label_zh: "开关会话",
        label_en: "Open / close sessions",
        short_zh: "开关会话",
        short_en: "sessions",
        desc_zh: "新开或关闭 SSH 标签页",
        desc_en: "open or close SSH tabs",
        get: |p: &Permissions| p.session_open_close,
        set: |p: &mut Permissions, v: bool| p.session_open_close = v,
    },
    PermissionBit {
        key: "settings_write",
        field: "settings_write",
        risk: Risk::Normal,
        group: Group::Config,
        label_zh: "修改常规设置",
        label_en: "Change routine settings",
        short_zh: "改设置",
        short_en: "settings",
        desc_zh: "24 个常规项：字体、主题、布局、终端显示、更新频率等",
        desc_en: "24 routine keys: font, theme, layout, terminal display, update cadence",
        get: |p: &Permissions| p.settings_write,
        set: |p: &mut Permissions, v: bool| p.settings_write = v,
    },
    PermissionBit {
        key: "host_key_trust",
        field: "host_key_trust",
        risk: Risk::High,
        group: Group::Session,
        label_zh: "信任未知主机密钥",
        label_en: "Trust unknown host keys",
        short_zh: "信任密钥",
        short_en: "trust keys",
        desc_zh: "首次连接时静默把未知密钥写入 known_hosts；密钥变更仍会弹窗",
        desc_en: "silently write a first-seen key to known_hosts; changed keys still prompt",
        get: |p: &Permissions| p.host_key_trust,
        set: |p: &mut Permissions, v: bool| p.host_key_trust = v,
    },
    PermissionBit {
        key: "terminal_input",
        field: "terminal_input",
        risk: Risk::High,
        group: Group::Terminal,
        label_zh: "写入终端",
        label_en: "Type into terminals",
        short_zh: "写入",
        short_en: "typing",
        desc_zh: "等同你在键盘上打字；Local 标签页还会额外要求本机执行",
        desc_en: "same as typing on your keyboard; local tabs also need local execution",
        get: |p: &Permissions| p.terminal_input,
        set: |p: &mut Permissions, v: bool| p.terminal_input = v,
    },
    PermissionBit {
        key: "exec",
        field: "exec",
        risk: Risk::High,
        group: Group::Terminal,
        label_zh: "在会话上执行命令",
        label_en: "Run commands on sessions",
        short_zh: "执行",
        short_en: "exec",
        desc_zh: "SSH 会话上的一次性 exec 通道，以及结束远端进程；不进入终端窗口",
        desc_en: "one-shot exec channel on SSH sessions, and terminating remote processes; does not touch the terminal view",
        get: |p: &Permissions| p.exec,
        set: |p: &mut Permissions, v: bool| p.exec = v,
    },
    PermissionBit {
        key: "local_exec",
        field: "local_exec",
        risk: Risk::High,
        group: Group::Terminal,
        label_zh: "在本机执行命令",
        label_en: "Run commands on this machine",
        short_zh: "本机执行",
        short_en: "local exec",
        desc_zh: "在本机起 cmd / sh 子进程；Local 标签页上的输入也受它约束",
        desc_en: "spawn cmd / sh locally; input into local tabs is gated by it too",
        get: |p: &Permissions| p.local_exec,
        set: |p: &mut Permissions, v: bool| p.local_exec = v,
    },
    PermissionBit {
        key: "settings_high",
        field: "settings_high",
        risk: Risk::High,
        group: Group::Config,
        label_zh: "修改高风险设置",
        label_en: "Change high-risk settings",
        short_zh: "改高危设置",
        short_en: "high settings",
        desc_zh: "9 个高风险项：更新代理、WebDAV 目标、下载免确认、OSC52、WSL 启动项",
        desc_en: "9 keys: update proxy, WebDAV target, download prompt, OSC52, WSL profiles",
        get: |p: &Permissions| p.settings_high,
        set: |p: &mut Permissions, v: bool| p.settings_high = v,
    },
    PermissionBit {
        key: "host_upsert",
        field: "host_upsert",
        risk: Risk::High,
        group: Group::Host,
        label_zh: "新增 / 修改主机",
        label_en: "Create / edit hosts",
        short_zh: "改主机",
        short_en: "edit hosts",
        desc_zh: "新增或覆盖主机，可写入口令、私钥、代理与登录触发器",
        desc_en: "create or overwrite hosts, including passwords, keys, proxies and login triggers",
        get: |p: &Permissions| p.host_upsert,
        set: |p: &mut Permissions, v: bool| p.host_upsert = v,
    },
    PermissionBit {
        key: "host_delete",
        field: "host_delete",
        risk: Risk::High,
        group: Group::Host,
        label_zh: "删除主机",
        label_en: "Delete hosts",
        short_zh: "删主机",
        short_en: "delete hosts",
        desc_zh: "删除已保存的主机；可顺带关闭它的标签页",
        desc_en: "delete a saved host; may also close its open tabs",
        get: |p: &Permissions| p.host_delete,
        set: |p: &mut Permissions, v: bool| p.host_delete = v,
    },
    PermissionBit {
        key: "file_download",
        field: "file_download",
        risk: Risk::High,
        group: Group::File,
        label_zh: "下载远端文件",
        label_en: "Download remote files",
        short_zh: "下载",
        short_en: "download",
        desc_zh: "把远端文件下载到本机下载目录；同名文件不覆盖",
        desc_en: "download into the configured download folder; never overwrites",
        get: |p: &Permissions| p.file_download,
        set: |p: &mut Permissions, v: bool| p.file_download = v,
    },
    PermissionBit {
        key: "file_upload",
        field: "file_upload",
        risk: Risk::High,
        group: Group::File,
        label_zh: "上传、删除、改名远端文件",
        label_en: "Write to remote paths",
        short_zh: "写入",
        short_en: "write",
        desc_zh: "上传覆盖、删除、改名、改权限以及新建目录",
        desc_en: "upload, delete, rename, chmod and mkdir on remote paths",
        get: |p: &Permissions| p.file_upload,
        set: |p: &mut Permissions, v: bool| p.file_upload = v,
    },
];

/// 9 个高风险设置键：写它们要 `settings_high`，读它们要被 `get_settings` 脱敏。
/// 与 `crate::app::mcp_ui::settings_table()` 的一致性由那里的测试保证。
pub(crate) const HIGH_RISK_SETTINGS: &[&str] = &[
    "update.proxy_enabled",
    "update.proxy",
    "sync.webdav_url",
    "sync.webdav_username",
    "sync.webdav_remote_path",
    "sync.upload_enabled",
    "transfer.download_always_ask",
    "terminal.osc52",
    "wsl.profiles",
];

pub(crate) fn is_high_risk_setting(key: &str) -> bool {
    HIGH_RISK_SETTINGS.contains(&key)
}

/// 一位权限在「风险段 → 功能区」树里的位置。设置页与授权弹窗共用，
/// Slint 侧只读这些标记、不做分组判断。
pub(crate) struct BitRow {
    pub(crate) bit: &'static PermissionBit,
    /// 非空 = 该风险段的第一行（要打段标题）。
    pub(crate) risk: Option<Risk>,
    /// 非空 = 该组的第一行，且**只在组内多于 1 位时**才会给出 ——
    /// 单条组（常规段的查看/会话/配置、高风险段的会话/配置）不画组标题，
    /// 否则标题会比内容还密，正是弹窗排版显得杂乱的原因。
    /// 授权弹窗用这个：可折叠的组才有标题。
    pub(crate) group: Option<Group>,
    /// 同样的"组首行"标记，但**不管组内几条都给**（旧语义）。
    /// 设置页的权限列表用它：那里没有折叠，每个功能区都要标题。
    pub(crate) group_always: Option<Group>,
    /// 组 id（每行都有，同组同值），供「折叠某组」定位。
    pub(crate) section: &'static str,
    /// 组内位数。
    pub(crate) section_total: usize,
}

impl BitRow {
    /// 组内多于 1 位才值得折叠。
    pub(crate) fn collapsible(&self) -> bool {
        self.section_total > 1
    }

    /// 默认展开：常规段全展开；高风险段的可折叠组收起（高危项默认不铺在眼前）；
    /// 单条组恒展开 —— 它没有标题可点，收起就永远看不见了。
    pub(crate) fn default_expanded(&self) -> bool {
        !(self.collapsible() && matches!(self.bit.risk, Risk::High))
    }
}

/// `(风险段, 功能区)` → 稳定组 id。穷举 match：新增功能区时编译器会强制补齐。
fn section_id(risk: Risk, group: Group) -> &'static str {
    match (risk, group) {
        (Risk::Normal, Group::View) => "routine.view",
        (Risk::Normal, Group::Session) => "routine.session",
        (Risk::Normal, Group::Config) => "routine.config",
        (Risk::Normal, Group::Terminal) => "routine.terminal",
        (Risk::Normal, Group::Host) => "routine.host",
        (Risk::Normal, Group::File) => "routine.file",
        (Risk::High, Group::View) => "high.view",
        (Risk::High, Group::Session) => "high.session",
        (Risk::High, Group::Config) => "high.config",
        (Risk::High, Group::Terminal) => "high.terminal",
        (Risk::High, Group::Host) => "high.host",
        (Risk::High, Group::File) => "high.file",
    }
}

/// 逐位给出分组信息：`risk` 非空 = 段首行；`group` 非空 = 可折叠组的组首行。
pub(crate) fn bit_rows() -> Vec<BitRow> {
    let mut out = Vec::with_capacity(PERMISSION_BITS.len());
    let mut prev_risk: Option<Risk> = None;
    let mut prev_group: Option<Group> = None;
    for bit in PERMISSION_BITS {
        let total = PERMISSION_BITS
            .iter()
            .filter(|other| other.risk == bit.risk && other.group == bit.group)
            .count();
        let risk = (prev_risk != Some(bit.risk)).then_some(bit.risk);
        let is_group_head = risk.is_some() || prev_group != Some(bit.group);
        let group_always = is_group_head.then_some(bit.group);
        // 弹窗只给"可折叠的组"画标题；设置页用 group_always。
        let group = if total > 1 { group_always } else { None };
        out.push(BitRow {
            bit,
            risk,
            group,
            group_always,
            section: section_id(bit.risk, bit.group),
            section_total: total,
        });
        prev_risk = Some(bit.risk);
        prev_group = Some(bit.group);
    }
    out
}

impl Permissions {
    /// 未知权限名一律拒绝（fail closed）。
    pub(crate) fn allows(&self, key: &str) -> bool {
        self.get(key).unwrap_or(false)
    }

    pub(crate) fn get(&self, key: &str) -> Option<bool> {
        PERMISSION_BITS
            .iter()
            .find(|bit| bit.key == key)
            .map(|bit| (bit.get)(self))
    }

    /// 未知 key 返回 false。
    pub(crate) fn set(&mut self, key: &str, value: bool) -> bool {
        match PERMISSION_BITS.iter().find(|bit| bit.key == key) {
            Some(bit) => {
                (bit.set)(self, value);
                true
            }
            None => false,
        }
    }

    pub(crate) fn from_keys(keys: &[&str]) -> Self {
        let mut perms = Self::default();
        for key in keys {
            let _ = perms.set(key, true);
        }
        perms
    }

    /// 已置位的 key，按表顺序（错误负载与审计需要稳定顺序）。
    pub(crate) fn keys(&self) -> Vec<&'static str> {
        PERMISSION_BITS
            .iter()
            .filter(|bit| (bit.get)(self))
            .map(|bit| bit.key)
            .collect()
    }

    pub(crate) fn minus(&self, other: &Self) -> Self {
        let mut out = *self;
        for bit in PERMISSION_BITS {
            (bit.set)(&mut out, (bit.get)(self) && !(bit.get)(other));
        }
        out
    }

    pub(crate) fn subset_of(&self, upper: &Self) -> bool {
        PERMISSION_BITS
            .iter()
            .all(|bit| !(bit.get)(self) || (bit.get)(upper))
    }

    pub(crate) fn covered_by(&self, upper: &Self) -> bool {
        self.subset_of(upper)
    }

    pub(crate) fn describe(&self) -> Value {
        let mut map = Map::new();
        for bit in PERMISSION_BITS {
            map.insert(bit.key.to_string(), Value::Bool((bit.get)(self)));
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_on() -> Permissions {
        let mut perms = Permissions::default();
        for bit in PERMISSION_BITS {
            assert!(perms.set(bit.key, true), "{} 未被 set 识别", bit.key);
        }
        perms
    }

    #[test]
    fn permission_bits_are_unique_and_complete() {
        assert_eq!(PERMISSION_BITS.len(), 12);
        let mut keys: Vec<&str> = PERMISSION_BITS.iter().map(|bit| bit.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), PERMISSION_BITS.len(), "权限 key 有重复");
        for bit in PERMISSION_BITS {
            assert_eq!(bit.key, bit.field, "key 与字段名不一致");
            assert_eq!(bit.key, bit.field);
        }
        let normal = PERMISSION_BITS
            .iter()
            .filter(|bit| bit.risk == Risk::Normal)
            .count();
        let high = PERMISSION_BITS
            .iter()
            .filter(|bit| bit.risk == Risk::High)
            .count();
        assert_eq!((normal, high), (3, 9));
    }

    #[test]
    fn bit_rows_heads_follow_risk_then_collapsible_group() {
        let rows = bit_rows();
        assert_eq!(rows.len(), PERMISSION_BITS.len());
        assert_eq!(rows[0].risk, Some(Risk::Normal), "第一行必须是段标题");
        let risk_heads: Vec<Risk> = rows.iter().filter_map(|row| row.risk).collect();
        assert_eq!(risk_heads, vec![Risk::Normal, Risk::High]);

        // 组标题只出现在"组内多于 1 位"的组首行，且同一组只打一次。
        let mut seen: Vec<&str> = Vec::new();
        for row in &rows {
            if let Some(group) = row.group {
                assert!(row.section_total > 1, "{group:?} 是单条组，不该打标题");
                assert!(!seen.contains(&row.section), "{} 重复打标题", row.section);
                seen.push(row.section);
            }
            // 反过来：可折叠组的首行必须有标题。
            if row.section_total > 1 && !seen.contains(&row.section) {
                panic!("{} 是可折叠组却没有组标题", row.section);
            }
        }
        // 2 个段标题 + 3 个可折叠组（高风险段的终端与命令、主机、文件）。
        assert_eq!(seen.len(), 3, "可折叠组数量变了：{seen:?}");

        // 设置页要的是"每个功能区都有标题"的旧语义：常规 3 组 + 高风险 5 组。
        let always: Vec<&str> = rows
            .iter()
            .filter(|row| row.group_always.is_some())
            .map(|row| row.section)
            .collect();
        assert_eq!(always.len(), 8, "设置页的分组标题数变了：{always:?}");
    }

    /// 组 id 连续、每组的 total 与实际行数一致、总和 = 12。
    #[test]
    fn bit_rows_sections_are_contiguous_and_total() {
        let rows = bit_rows();
        let mut sum = 0;
        let mut index = 0;
        while index < rows.len() {
            let section = rows[index].section;
            let total = rows[index].section_total;
            let run = rows[index..]
                .iter()
                .take_while(|row| row.section == section)
                .count();
            assert_eq!(run, total, "{section} 的 section_total 与相邻行数不符");
            sum += total;
            index += run;
        }
        assert_eq!(sum, PERMISSION_BITS.len());
    }

    /// 默认折叠态：常规段全展开、高风险段的可折叠组收起、单条组恒展开。
    #[test]
    fn bit_rows_default_expansion() {
        for row in bit_rows() {
            if row.section_total <= 1 {
                assert!(row.default_expanded(), "单条组必须展开：{}", row.section);
            } else if row.bit.risk == Risk::High {
                assert!(
                    !row.default_expanded(),
                    "高风险组默认应收起：{}",
                    row.section
                );
            } else {
                assert!(row.default_expanded(), "常规组默认应展开：{}", row.section);
            }
        }
    }

    /// 悬停气泡靠 desc 兜底，12 位的中英文描述都不能为空。
    #[test]
    fn every_bit_has_a_description() {
        for bit in PERMISSION_BITS {
            assert!(!bit.desc_zh.trim().is_empty(), "{} 缺中文描述", bit.key);
            assert!(!bit.desc_en.trim().is_empty(), "{} 缺英文描述", bit.key);
        }
    }

    #[test]
    fn default_is_empty() {
        let perms = Permissions::default();
        assert!(perms.keys().is_empty());
        for bit in PERMISSION_BITS {
            assert!(!perms.allows(bit.key), "{} 默认不能开", bit.key);
        }
        assert!(!perms.allows("not_a_permission"));
    }

    #[test]
    fn from_keys_and_keys_roundtrip_in_table_order() {
        let perms = Permissions::from_keys(&["local_exec", "read", "exec"]);
        assert_eq!(perms.keys(), vec!["read", "exec", "local_exec"]);
        let mut expected = Permissions::default();
        expected.set("read", true);
        expected.set("exec", true);
        expected.set("local_exec", true);
        assert_eq!(perms, expected);
    }

    #[test]
    fn minus_and_covered_by() {
        let upper = all_on();
        let mut lower = all_on();
        assert!(lower.covered_by(&upper));
        assert_eq!(lower.minus(&upper), Permissions::default());

        lower.set("exec", false);
        assert!(lower.covered_by(&upper));
        assert_eq!(upper.minus(&lower).keys(), vec!["exec"]);

        let leaked = Permissions::from_keys(&["exec"]);
        assert!(!leaked.covered_by(&lower));
        assert!(Permissions::default().covered_by(&lower));
    }

    #[test]
    fn describe_matches_bits() {
        let described = Permissions::default().describe();
        let map = described.as_object().expect("describe 必须返回对象");
        assert_eq!(map.len(), PERMISSION_BITS.len());
        for bit in PERMISSION_BITS {
            assert_eq!(map.get(bit.key), Some(&Value::Bool(false)), "{}", bit.key);
        }
    }

    #[test]
    fn high_risk_settings_are_classified() {
        assert_eq!(HIGH_RISK_SETTINGS.len(), 9);
        assert!(is_high_risk_setting("update.proxy"));
        assert!(!is_high_risk_setting("terminal.font_size"));
    }
}
