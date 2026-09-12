//! 分域设置模型：每个设置页一组，**出厂默认就在各自的 `Default` 里**。
//!
//! 为什么这样分
//! ------------
//! 此前所有设置平铺在 `ConfigFile` 的 66 个字段里，默认值却有三处来源：
//! serde 的 `#[serde(default = "...")]`（JSON 缺字段时）、派生的 `Default`（全 0/
//! 空/false）、以及 getter 里的哨兵兜底（`0 → 13`）。三者不一致，"还原本页默认"
//! 读错了其中一处就还原成一张白纸 —— 这是实际发生过的 bug。
//!
//! 现在每个域一个结构体，用 `#[serde(default)]`（**容器级**：缺字段时用
//! `Struct::default()`）+ 手工 `Default` 实现，使三处来源合一：
//!
//! ```text
//! serde 缺字段  ─┐
//! 派生 Default  ─┼─→ 各域结构体的 impl Default   ← 唯一出处
//! fresh_config  ─┘
//! ```
//!
//! 兼容性
//! ------
//! 挂回 `ConfigFile` 时用 `#[serde(flatten)]`，**on-disk 仍是平铺 key-value**，
//! 老 `sessions.json` 可原样读取，无需迁移。

use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_WALLPAPER_OVERLAY, OutputHighlightRule, Secret};

/// 终端页：字体 / 光标 / 回滚 / 高亮 / 粘贴行为。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// 终端字体族。
    pub font_family: String,
    /// 字号（px）。
    pub font_size: u32,
    /// 强制正文使用粗体字面（#262）。
    pub terminal_bold: bool,
    /// 回滚行数上限。
    pub scrollback_lines: usize,
    /// 粘贴/输入时把 LF 转成 CRLF。
    pub convert_eol: bool,
    /// 允许远端程序通过 OSC 52 写剪贴板。
    pub osc52_clipboard: bool,
    /// 光标形状：block / bar / underline。
    pub terminal_cursor_style: String,
    /// 光标颜色 `#RRGGBB`；空串 = 跟随主题。
    pub terminal_cursor_color: String,
    /// 存储取反：缺失/老配置保持"纯文本高亮开启"。
    pub output_highlight_disabled: bool,
    /// 存储取反：缺失/老配置保持"JSON 格式化开启"。
    pub json_format_disabled: bool,
    /// 内置规则集：builtin / log / devops。
    pub output_highlight_preset: String,
    /// 用户自定义规则（先于内置预设生效）。
    pub output_highlight_rules: Vec<OutputHighlightRule>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_family: "JetBrains Mono".to_string(),
            font_size: 13,
            terminal_bold: false,
            scrollback_lines: 5000,
            convert_eol: true,
            osc52_clipboard: true,
            terminal_cursor_style: "bar".to_string(),
            terminal_cursor_color: "#FFFFFF".to_string(),
            output_highlight_disabled: false,
            json_format_disabled: false,
            output_highlight_preset: "builtin".to_string(),
            output_highlight_rules: Vec::new(),
        }
    }
}

/// 外观页：界面字体 / 壁纸 / 渲染后端 / 缩放 / 资源面板过滤。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// 界面语言："zh" / "en"；空 = zh。
    pub language: String,
    /// 主题偏好："system" / "dark" / "light"；空 = system。
    pub theme_pref: String,
    /// 平台渲染后端；空 = 平台默认（macOS → femtovg）。
    pub renderer_mode: String,
    /// 界面字体族；空 = 按平台自动探测 CJK 字体。
    pub ui_font_family: String,
    /// 全局界面缩放百分比（0 = 100）。
    pub ui_scale: u32,
    /// 设置面板字号百分比（0 = 100）。
    pub panel_font: u32,
    /// 沉浸式壁纸 id：""=无 / builtin:light / builtin:dark / 自定义文件路径。
    pub wallpaper: String,
    /// 壁纸磨砂层不透明度（1 - 透明度）；0 = 用默认。
    pub wallpaper_overlay: f32,
    /// 资源面板隐藏 EFI / 交换分区等伪文件系统。
    pub hide_special_partitions: bool,
    /// 资源面板挂载点白名单；空 = 全部显示。
    pub mount_filter: String,
    /// 存储取反：缺失/老配置保持动画开启。
    pub animations_disabled: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            language: String::new(),
            theme_pref: String::new(),
            renderer_mode: String::new(),
            ui_font_family: String::new(),
            ui_scale: 100,
            panel_font: 100,
            wallpaper: "builtin:dark".to_string(),
            wallpaper_overlay: DEFAULT_WALLPAPER_OVERLAY,
            hide_special_partitions: true,
            mount_filter: String::new(),
            animations_disabled: false,
        }
    }
}

/// 布局页：面板停靠 / 折叠 / 尺寸 / 标签页形态。
///
/// 这一组大多是**派生状态**：`sidebar_dock` / `welcome_as_sidebar` 一变，
/// 停靠冲突消解、各面板几何量、窗格树都要跟着重算（见 `apply_layout_prefs`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    pub collapse_sidebar_default: bool,
    pub collapse_sftp_default: bool,
    pub quick_commands_as_sidebar: bool,
    pub welcome_as_sidebar: bool,
    pub hide_cmd_bar: bool,
    pub zen_mode: bool,
    /// 上次的资源侧栏折叠态；None = 回退到 `collapse_sidebar_default`。
    pub sidebar_collapsed: Option<bool>,
    pub sidebar_width: f32,
    pub sidebar_height: f32,
    /// 资源面板停靠边：left / right / top / bottom。
    pub sidebar_dock: String,
    pub sftp_panel_width: f32,
    pub sftp_panel_height: f32,
    pub sftp_dock: String,
    pub quick_panel_open: bool,
    pub quick_panel_collapsed: bool,
    pub quick_panel_width: f32,
    pub quick_panel_height: f32,
    pub quick_panel_dock: String,
    pub welcome_sidebar_width: f32,
    pub welcome_sidebar_dock: String,
    /// None = 用户尚未显式折叠/展开过欢迎侧栏。
    pub welcome_collapsed: Option<bool>,
    /// 窗口尺寸（逻辑像素；0 = 未设置 → 用内置默认）。
    pub window_width: f32,
    pub window_height: f32,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            collapse_sidebar_default: false,
            collapse_sftp_default: false,
            quick_commands_as_sidebar: false,
            welcome_as_sidebar: false,
            hide_cmd_bar: false,
            zen_mode: false,
            sidebar_collapsed: None,
            sidebar_width: 220.0,
            sidebar_height: 240.0,
            sidebar_dock: "right".to_string(),
            sftp_panel_width: 380.0,
            sftp_panel_height: 220.0,
            sftp_dock: String::new(),
            quick_panel_open: false,
            quick_panel_collapsed: false,
            quick_panel_width: 260.0,
            quick_panel_height: 220.0,
            quick_panel_dock: String::new(),
            welcome_sidebar_width: 240.0,
            welcome_sidebar_dock: String::new(),
            welcome_collapsed: None,
            window_width: 0.0,
            window_height: 0.0,
        }
    }
}

/// 传输页：SFTP 跟随 / 下载位置。
///
/// 本域所有字段的出厂默认就是各自类型的 `Default`（false / 空串），因此直接
/// 派生 —— 与 Terminal/Appearance/Layout 那三个需要写非零默认值的域不同。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TransferSettings {
    /// 存储取反（"不跟随"），使缺失/老配置默认跟随 cd。
    pub sftp_no_follow_cd: bool,
    /// 每次下载都询问保存位置，而不是用预设目录。
    pub download_always_ask: bool,
    /// 预设下载目录；空 = 每次都问。
    pub download_dir: String,
}


/// 同步页：WebDAV 配置备份（含加密口令）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SyncSettings {
    /// 会话同步开启时，是否把 SFTP 上传也镜像到其它在线会话。
    pub sync_upload: bool,
    pub webdav_enabled: bool,
    pub webdav_url: String,
    pub webdav_username: String,
    /// 与登录口令同样加密存储。
    pub webdav_password: Secret,
    pub webdav_remote_path: String,
    pub webdav_accept_invalid_certs: bool,
    /// 可选的证书 SHA-256 指纹（小写 hex）；配合"信任自签名"收紧校验。
    pub webdav_cert_pin: String,
}


/// 新版本提示页。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpdateSettings {
    /// 存储取反：缺失/老配置保持启动检查开启。
    pub update_check_disabled: bool,
}

