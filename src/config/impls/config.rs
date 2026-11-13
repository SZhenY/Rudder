//! Session / application configuration.
//!
//! Persists a simple JSON file in the app's data directory. Resolution is
//! **portable-first** (#141): a `config/` folder next to the executable is
//! preferred so the whole app can ride along on a USB stick and never litters
//! the user profile. When the executable lives somewhere read-only (a
//! system-wide install under Program Files / `/usr`), it falls back to the
//! per-user OS config dir (e.g. `%APPDATA%/Rudder`, `~/.config/Rudder`),
//! which is also where every pre-0.4.15 version stored its data — so existing
//! installs keep working untouched. See [`data_dir`].
//!
//! ## Password encryption
//!
//! Passwords are **not** stored in plaintext.  On first launch a random
//! 256-bit key is written to `secret.key` in the same config directory
//! (mode `0600` on Unix).  Every non-empty password is then encrypted with
//! **ChaCha20-Poly1305** (a random 96-bit nonce per value) and stored as
//!
//! ```text
//! enc:v1:<base64url(nonce_12_bytes || ciphertext)>
//! ```
//!
//! Legacy plaintext passwords (from older installs) are left untouched in
//! memory and silently re-encrypted the next time the config is saved.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305,
    aead::{Aead, KeyInit},
};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use super::settings::{
    AppearanceSettings, LayoutSettings, McpClientGrant, McpSettings, SyncSettings,
    TerminalSettings, TransferSettings, UpdateSettings,
};
use uuid::Uuid;
use zeroize::Zeroize;

// ── Data directory resolution (portable-first, #141) ──────────────────────────
//
// All user data — sessions.json, secret.key, known_hosts, error.log — lives in
// ONE directory resolved here, and `errlog` / `known_hosts` route through it too.

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// `chmod 0600`，best-effort：复制 `secret.key` 时收紧权限，权限设置失败不该让
/// 复制本身失败。三处搬运 `secret.key` 的路径共用（原先各自内联一遍
/// `#[cfg(unix)]` + `use PermissionsExt`）。
#[cfg(unix)]
fn set_private_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

/// The single directory holding all user data (sessions, encryption key,
/// known_hosts, error.log). Resolved once and cached; any one-time migration
/// from the legacy per-user dir runs exactly once.
///
/// Portable-first: prefers a `config/` folder beside the executable, falling
/// back to the per-user OS config dir when the exe dir is read-only (#141).
pub fn data_dir() -> PathBuf {
    DATA_DIR.get_or_init(resolve_data_dir).clone()
}

/// Directory for diagnostic logs (`error.log`). Kept *separate* from the config
/// dir so logs don't clutter user data: portable-first → a `log/` folder beside
/// the executable (a sibling of `config/`), falling back to a `log/` subdir
/// under the per-user data dir when the exe dir is read-only (Program Files etc.)
/// (#log-dir).
/// macOS: always the per-user log dir — the app bundle /Applications is a
/// shared system location and never gets an exe-adjacent `log/` folder. All
/// user data stays under `~/Library/Application Support/dev.rudder.rudder`.
#[cfg(target_os = "macos")]
pub fn log_dir() -> PathBuf {
    let dir = data_dir().join("log");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// 当前 unix 秒。授权时间戳要跨进程重启存活，所以用挂钟而不是 `Instant`。
///
/// 跨平台：`upsert_mcp_client_grant` 与 `on_initialized` 都调它，不能挂 `cfg`。
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Windows / Linux: portable-first — `<exe_dir>/log`, sibling of the portable
/// config/ folder, falling back to a `log/` subdir under the per-user data dir
/// when the exe dir is read-only (Program Files etc.).
#[cfg(not(target_os = "macos"))]
pub fn log_dir() -> PathBuf {
    // Portable: <exe_dir>/log, sibling of the portable config/ folder.
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let log = parent.join("log");
        if fs::create_dir_all(&log).is_ok() && dir_is_writable(&log) {
            return log;
        }
    }
    // Read-only exe dir → put logs in their own subdir under the per-user data
    // dir (still not mixed in with sessions.json et al.).
    let dir = data_dir().join("log");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Pre-0.4.15 location: the per-user OS config dir
/// (`%APPDATA%/Rudder`, `~/.config/Rudder`, …).
fn legacy_data_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "rudder", "rudder").map(|d| d.config_dir().to_path_buf())
}

/// Portable location: a `config/` folder beside the executable.
fn portable_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("config"))
}

/// True only if we can actually create and write a file in `dir` — Program Files
/// and other system locations can reject writes even when the dir appears to
/// exist, so a real write probe is the reliable test.
/// (macOS always uses the per-user dir, so the probe is Windows/Linux only.)
#[cfg(not(target_os = "macos"))]
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".write_probe_{}", std::process::id()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 把目录权限收紧到 0700（unix；Windows 的 `%TEMP%` 本就是当前用户专属）。
fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// 这个目录是不是"我们自己的私有目录"：真的目录（不是符号链接）、0700、属主是我们。
#[cfg(unix)]
fn is_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        return false;
    }
    // std 没有 getuid，拿可执行文件的属主当"我们自己"。
    std::env::current_exe()
        .ok()
        .and_then(|exe| fs::metadata(exe).ok())
        .is_some_and(|m| m.uid() == meta.uid())
}

#[cfg(not(unix))]
fn is_private_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
}

/// 8 字节加密随机数的十六进制串 —— 临时目录名用，别留可预测的名字。
fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    // 拿不到系统熵就退化成时间戳 —— 这个值只用于临时目录名，不需要密码学强度，
    // 但不能留可预测的固定串。
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 在临时目录里建一个**本次运行专属**的私有目录（随机名、0700、创建即独占）。
///
/// 给自更新暂存这类"用完即弃、但不能被别人预占"的场景用。
pub(crate) fn create_private_temp_dir(prefix: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", random_suffix()));
    // `create_dir` 而不是 `create_dir_all`：名字已存在就报错，不会跟随符号链接。
    fs::create_dir(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    restrict_dir(&dir);
    Ok(dir)
}

/// 私有临时目录：配置数据的最后一招回落，也用作 zmodem 收文件的兜底目录。
///
/// 刻意**不**无条件用 `<temp>/rudder`：`/tmp` 是 1777（世界可写），共享机器上别的
/// 本地账户可以先放一个同名目录或符号链接，把 `secret.key` 与 `sessions.json` 引到
/// 任意位置。所以只在"目录由我们刚建出来"或"已存在的确实是我们的私有目录"时才用它，
/// 否则换一个不可预测的随机名 —— 宁可丢持久化，也不写进别人能摆布的位置。
pub(crate) fn private_temp_dir() -> PathBuf {
    let base = std::env::temp_dir();
    let candidate = base.join("rudder");
    if fs::create_dir(&candidate).is_ok() {
        restrict_dir(&candidate);
        return candidate;
    }
    if is_private_dir(&candidate) {
        return candidate; // 上次运行留下的、确实属于我们的目录
    }
    let dir = base.join(format!("rudder-{}", random_suffix()));
    let _ = fs::create_dir(&dir);
    restrict_dir(&dir);
    dir
}

/// Windows: the data dir is ALWAYS beside the executable — `<exe_dir>/config`
/// (logs in `<exe_dir>/log`, user fonts in `<exe_dir>/config/fonts`) —
/// whether installed or portable. The app is fully self-contained and never
/// writes to AppData. On the first launch in a portable location, existing
/// legacy data is copied over once (`migrate_legacy`).
///
/// The MSI installer creates `config` and `log` and grants `Users` write access
/// to them (`wix/main.wxs`), so a per-machine install in Program Files keeps its
/// data too. The temp dir remains only as a last resort for installs where that
/// grant is missing, so the app still starts on a read-only exe dir.
#[cfg(target_os = "windows")]
fn resolve_data_dir() -> PathBuf {
    if let Some(portable) = portable_data_dir()
        && fs::create_dir_all(&portable).is_ok()
        && dir_is_writable(&portable)
    {
        // MSI 装在 Program Files 时，旧版本写不进 `config`，配置被放到这里的
        // `%TEMP%\rudder`。安装包现在给 `config` 授权了，先把那份数据搬过来 ——
        // 不搬的话升级后会话与设置全是空的。比 AppData 那份更近，先跑它。
        if let Some(tmp) = temporary_data_dir() {
            migrate_temp_legacy(&tmp, &portable);
        }
        if let Some(legacy) = legacy_data_dir().filter(|l| l != &portable) {
            migrate_legacy(&legacy, &portable);
        }
        return portable;
    }

    // Read-only exe dir → temp dir, so the app still launches. Data is lost on
    // reboot, but never silently goes to AppData.
    private_temp_dir()
}

/// 旧版 MSI 的配置目录：`private_temp_dir()` 的首选名，仅存在时返回。
#[cfg(target_os = "windows")]
fn temporary_data_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("rudder");
    dir.is_dir().then_some(dir)
}

/// macOS: always the per-user OS config dir (`~/Library/Application
/// Support/dev.rudder.rudder`). The app bundle lives in a shared, usually
/// read-only location, so no `config/` folder is ever created beside the
/// executable — matching the user's preference to keep all data under the
/// official macOS location. A one-time migration pulls data from a previous
/// portable (exe-adjacent) config if the per-user dir is still empty.
#[cfg(target_os = "macos")]
fn resolve_data_dir() -> PathBuf {
    let dir = legacy_data_dir().unwrap_or_else(private_temp_dir);
    let _ = fs::create_dir_all(&dir);

    if let Some(portable) = portable_data_dir()
        && portable != dir
        && portable.exists()
        && !dir.join("sessions.json").exists()
    {
        // If the per-user dir holds a secret.key but no sessions.json, that
        // key may not decrypt the portable sessions (the two files are a
        // pair). Drop the orphan key so migrate_legacy pulls the portable
        // pair in as a unit, keeping sessions decryptable (#migration-consistency).
        if dir.join("secret.key").exists() && portable.join("sessions.json").exists() {
            let _ = fs::remove_file(dir.join("secret.key"));
        }
        migrate_legacy(&portable, &dir);
    }
    dir
}

/// Linux: keep the original behaviour — portable-first when the exe dir is
/// writable, otherwise the per-user OS config dir (AppData-style), with a temp
/// dir as the last resort.
#[cfg(target_os = "linux")]
fn resolve_data_dir() -> PathBuf {
    let legacy = legacy_data_dir();

    if let Some(portable) = portable_data_dir() {
        // Already a portable install → keep using it (nothing to migrate).
        if portable.exists() && dir_is_writable(&portable) {
            return portable;
        }
        // Otherwise try to claim the portable dir. This succeeds only where the
        // exe directory is writable (i.e. not a Program Files / system install),
        // which is exactly when portable mode makes sense.
        if fs::create_dir_all(&portable).is_ok() && dir_is_writable(&portable) {
            if let Some(ref legacy) = legacy {
                migrate_legacy(legacy, &portable);
            }
            return portable;
        }
    }

    // Fall back to the legacy per-user dir (also the pre-0.4.15 location). Last
    // resort: a temp dir, so the app still launches if neither is available.
    let dir = legacy.unwrap_or_else(private_temp_dir);
    let _ = fs::create_dir_all(&dir);
    dir
}

/// On the first launch that lands on the portable dir, copy user data over from
/// the legacy per-user dir so upgrading users keep their saved sessions. The
/// originals are left in place (copy, not move) as a safety net, and existing
/// destination files are never overwritten (#141).
fn migrate_legacy(legacy: &Path, portable: &Path) {
    if legacy == portable {
        return;
    }
    for name in [SESSIONS_FILE, SETTINGS_FILE, "secret.key", "known_hosts"] {
        let src = legacy.join(name);
        let dst = portable.join(name);
        if src.exists() && !dst.exists() {
            match fs::copy(&src, &dst) {
                Ok(_) => {
                    // Keep the key owner-only on Unix (copy preserves bytes, not
                    // necessarily the mode).
                    #[cfg(unix)]
                    if name == "secret.key" {
                        set_private_mode(&dst);
                    }
                    tracing::info!(
                        "migrated {name} to portable config dir {}",
                        portable.display()
                    );
                }
                Err(e) => tracing::warn!(
                    "data migration: failed to copy {} → {}: {e}",
                    src.display(),
                    dst.display()
                ),
            }
        }
    }
}

/// 把旧版 MSI 落在 `%TEMP%\rudder` 的配置搬进安装目录。
///
/// 只在目标目录**还没被用过**时搬（既没有 `sessions.json` 也没有 `secret.key`）：
/// 那份临时数据可能很旧，用户也可能刚把会话清空过，往一份在用的配置里掺旧会话
/// 比丢数据更糟。密钥与会话始终成对搬，不存在"新 key 解不开旧会话"的组合。
/// 其余文件按"目标不存在才复制"（`migrate_legacy` 的既有规则）。
#[cfg(any(windows, test))]
fn migrate_temp_legacy(tmp: &Path, portable: &Path) {
    if portable.join(SESSIONS_FILE).exists() || portable.join("secret.key").exists() {
        return;
    }
    migrate_legacy(tmp, portable);
}

fn sessions_file_has_connections(path: &Path) -> bool {
    let Ok(raw) = crate::files::read_string_capped(path, crate::files::MAX_CONFIG_BYTES, "session config") else {
        return false;
    };
    serde_json::from_str::<ConfigFile>(&raw)
        .map(|cfg| !cfg.sessions.is_empty())
        .unwrap_or(false)
}

fn restore_user_backup_if_needed(primary_dir: &Path, backup_dir: &Path) {
    if primary_dir == backup_dir {
        return;
    }
    let primary_sessions = primary_dir.join(SESSIONS_FILE);
    let backup_sessions = backup_dir.join(SESSIONS_FILE);
    if sessions_file_has_connections(&primary_sessions)
        || !sessions_file_has_connections(&backup_sessions)
    {
        return;
    }
    let _ = fs::create_dir_all(primary_dir);
    for name in [SESSIONS_FILE, SETTINGS_FILE, "secret.key", "known_hosts"] {
        let src = backup_dir.join(name);
        let dst = primary_dir.join(name);
        if src.exists() {
            match fs::copy(&src, &dst) {
                Ok(_) => {
                    #[cfg(unix)]
                    if name == "secret.key" {
                        set_private_mode(&dst);
                    }
                    tracing::info!(
                        "restored {name} from user config backup {}",
                        backup_dir.display()
                    );
                }
                Err(e) => tracing::warn!(
                    "failed to restore {} from {} to {}: {e}",
                    name,
                    src.display(),
                    dst.display()
                ),
            }
        }
    }
}

/// A secret string (e.g. a session password) whose heap buffer is zeroed when
/// it is dropped, so plaintext credentials don't survive in freed memory and
/// turn up in core dumps, a debugger, or `/proc/<pid>/mem`.  `Clone` makes an
/// independent copy that is likewise zeroed on its own drop, and `Debug` is
/// redacted so a password can never be logged by accident.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never reveal the contents in logs / debug output.
        f.write_str(if self.0.is_empty() {
            "Secret(\"\")"
        } else {
            "Secret(***)"
        })
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Secret(String::deserialize(d)?))
    }
}

/// Which transport a session uses.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    /// SSH shell + SFTP (the original and default behaviour).
    #[default]
    Ssh,
    /// Local serial port (COM3 / /dev/ttyUSB0) for switches, routers, MCUs (#14).
    Serial,
    /// Plain Telnet over TCP, for legacy network gear (#17).
    Telnet,
    /// Local shell process on this machine (PowerShell/CMD/WSL/$SHELL).
    Local,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::Ssh => "ssh",
            SessionKind::Serial => "serial",
            SessionKind::Telnet => "telnet",
            SessionKind::Local => "local",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "serial" => SessionKind::Serial,
            "telnet" => SessionKind::Telnet,
            "local" => SessionKind::Local,
            _ => SessionKind::Ssh,
        }
    }
}

fn default_baud() -> u32 {
    115_200
}
fn default_data_bits() -> u8 {
    8
}
fn default_stop_bits() -> u8 {
    1
}
fn default_parity() -> String {
    "none".to_string()
}

/// 「检查频率」的取值，**顺序即设置页下拉的顺序**（下标 = `update-freq-index`）。
/// 第一项是默认值（每次启动）。改这里就别忘 `app::updater::check_interval_secs` 与
/// `app::settings::update` 的标签数组 —— 两边都有测试盯着它们对得上。
pub const UPDATE_FREQUENCIES: [&str; 6] = [
    "startup",
    "daily",
    "weekly",
    "monthly",
    "semiannual",
    "yearly",
];

/// Bump when `migrate_defaults` gains a new one-time default-layout change.
/// `migrate_defaults` 在 `rev >= DEFAULTS_REV` 时早退，新增迁移必须同时把版本号 +1。
// rev 7 = 移除内嵌 Meatshell Mono 后，把仍指向它的字体配置改回出厂默认。
pub const DEFAULTS_REV: u32 = 7;

/// 回滚行数的**常规**上限（A 方案）。默认值仍是 5 000 行，所以这项只约束"手动填大数"。
pub const SCROLLBACK_MAX: usize = 100_000;
/// 打开「大回滚缓冲区」后的上限。
pub const SCROLLBACK_MAX_LARGE: usize = 1_000_000;

/// 配置目录里的两个数据文件（**同目录**，共享 `secret.key`）：
/// 会话与用户数据 / 设置。拆分前设置也写在 `SESSIONS_FILE` 里。
pub(crate) const SESSIONS_FILE: &str = "sessions.json";
pub(crate) const SETTINGS_FILE: &str = "settings.json";

const PREVIOUS_DEFAULT_WALLPAPER_TRANSPARENCY: f32 = 0.38;
const PREVIOUS_DEFAULT_WALLPAPER_OVERLAY: f32 = 1.0 - PREVIOUS_DEFAULT_WALLPAPER_TRANSPARENCY;
/// 上一个出厂默认：15%（overlay 0.85）。rev 5 迁移用它识别仍停留在旧默认的用户。
const PREVIOUS_DEFAULT_WALLPAPER_OVERLAY_015: f32 = 0.85;
const DEFAULT_WALLPAPER_TRANSPARENCY: f32 = 0.30;
pub(crate) const DEFAULT_WALLPAPER_OVERLAY: f32 = 1.0 - DEFAULT_WALLPAPER_TRANSPARENCY;

/// MCP 服务的出厂端口。与上游实现的 Streamable HTTP 默认值一致，
/// 避开 8080/3000 这类常被别的开发服务器占用的口。
pub(crate) const DEFAULT_MCP_PORT: u16 = 8765;

fn normalize_hex_color(value: &str) -> Option<String> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", digits.to_ascii_uppercase()))
}

/// 全新配置（文件不存在，或原文件损坏）。
///
/// 分域之后这里只剩"与具体设置无关"的两件事：
/// * 各个设置域的出厂默认由 `TerminalSettings::default()` 等**唯一出处**提供，
///   `ConfigFile::default()`（派生）会转发到它们；
/// * 这里只负责标记默认值迁移已完成。
///
/// 此前这个函数要手工罗列 20 多个字段，而漏掉任何一项都会退回派生 Default 的
/// 0 / false / 空串 —— 「还原本页默认」出现过的白纸化 bug 正是这么来的。
pub(crate) fn fresh_config() -> ConfigFile {
    ConfigFile {
        defaults_rev: DEFAULTS_REV,
        ..ConfigFile::default()
    }
}

/// One-time push of the new default layout to *existing* users — but only for
/// each item they're still leaving at the old default, so deliberate choices are
/// never clobbered. Runs once (gated by `defaults_rev`); returns whether anything
/// changed so the caller can persist it. (#new-user-defaults)
fn migrate_defaults(cfg: &mut ConfigFile) -> bool {
    if cfg.defaults_rev >= DEFAULTS_REV {
        return false;
    }
    // Retired built-ins (tech/miku/ms) remap to dark regardless of rev: the
    // ids no longer exist in the picker, so any stored reference would render
    // nothing (#wallpaper-trim).
    if matches!(
        cfg.appearance.wallpaper.as_str(),
        "builtin:tech" | "builtin:miku" | "builtin:ms"
    ) {
        cfg.appearance.wallpaper = "builtin:dark".to_string();
    }
    // rev 1: miku / right-docked resources / wallpaper overlay.
    if cfg.defaults_rev < 1 {
        // Overlay still unset -> current default.
        if cfg.appearance.wallpaper_overlay <= 0.0 {
            cfg.appearance.wallpaper_overlay = DEFAULT_WALLPAPER_OVERLAY;
        }
    }
    // rev 2: settings show wallpaper transparency, while rev 1 accidentally
    // stored the default as panel alpha 0.38, so it displayed as ~62%.
    if cfg.defaults_rev < 2
        && (cfg.appearance.wallpaper_overlay - PREVIOUS_DEFAULT_WALLPAPER_TRANSPARENCY).abs()
            < 0.005
    {
        cfg.appearance.wallpaper_overlay = DEFAULT_WALLPAPER_OVERLAY;
    }
    // rev 3: reduce the default transparency from 38% to 15%. Only advance
    // users still on the previous default; preserve every custom slider value.
    if cfg.defaults_rev < 3
        && (cfg.appearance.wallpaper_overlay - PREVIOUS_DEFAULT_WALLPAPER_OVERLAY).abs() < 0.005
    {
        cfg.appearance.wallpaper_overlay = DEFAULT_WALLPAPER_OVERLAY;
    }
    // rev 5: 壁纸遮罩透明度出厂默认 15% → 30%。只迁移仍停留在旧默认（overlay
    // 0.85）的用户；自定义滑条值一律保留。
    if cfg.defaults_rev < 5
        && (cfg.appearance.wallpaper_overlay - PREVIOUS_DEFAULT_WALLPAPER_OVERLAY_015).abs() < 0.005
    {
        cfg.appearance.wallpaper_overlay = DEFAULT_WALLPAPER_OVERLAY;
    }
    // rev 6：回滚上限治理。老配置里可能存着 10 万以上的行数（当时上限是 100 万）——
    // 若不处理，新的常规上限会在下次写入时把它**静默截断**。读到这种配置就把
    // 「大回滚缓冲区」打开，保留用户原有的上限。
    // 注意这条**刻意不按 rev 门控**：它只在"存储值超出常规上限"时生效，而关闭开关
    // （`set_large_scrollback(false)`）本身会把现值收回上限，所以条件之后不会再成立。
    if cfg.terminal.scrollback_lines > SCROLLBACK_MAX {
        cfg.terminal.large_scrollback = true;
    }
    // rev 7：内嵌字体已移除，把仍指向它的存储值改回出厂默认；其它值不动。
    if cfg.defaults_rev < 7 {
        if cfg.terminal.font_family == "Meatshell Mono" {
            cfg.terminal.font_family = "JetBrains Mono".to_string();
        }
        if cfg.appearance.ui_font_family == "Meatshell Mono" {
            cfg.appearance.ui_font_family = String::new();
        }
    }
    cfg.defaults_rev = DEFAULTS_REV;
    true
}
fn default_sftp_width() -> f32 {
    380.0
}
fn default_sftp_height() -> f32 {
    220.0
}

fn default_quick_panel_width() -> f32 {
    260.0
}

fn default_quick_panel_height() -> f32 {
    220.0
}
fn default_flow() -> String {
    "none".to_string()
}
fn default_encoding() -> String {
    "UTF-8".to_string()
}

/// How a session authenticates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Password,
    #[serde(rename = "keyboard-interactive")]
    KeyboardInteractive,
    Key,
}

impl AuthMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::KeyboardInteractive => "keyboard-interactive",
            AuthMethod::Key => "key",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "keyboard-interactive" | "keyboard" | "interactive" => AuthMethod::KeyboardInteractive,
            "key" => AuthMethod::Key,
            _ => AuthMethod::Password,
        }
    }
}

/// A single saved SSH target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    #[serde(default)]
    pub password: Secret,
    #[serde(default)]
    pub private_key_path: String,
    #[serde(default)]
    pub private_key_inline: Secret,
    /// Optional outbound proxy, e.g. "socks5://127.0.0.1:1080" or
    /// "http://user:pass@host:8080". Empty = use $ALL_PROXY, else direct.
    ///
    /// 新建的会话不把凭据写在 URL 里：`proxy_user` / `proxy_pass` 各存一份，
    /// 密码落地前加密（[`Self::proxy_pass`]）。带 userinfo 的旧 URL 仍然认。
    #[serde(default)]
    pub proxy: String,
    /// 代理用户名；空 = 不需要认证，或凭据在 `proxy` 的 userinfo 里。
    #[serde(default)]
    pub proxy_user: String,
    /// 代理密码（落盘加密、永不下发界面 / MCP 回读）。
    #[serde(default)]
    pub proxy_pass: Secret,
    /// Optional SSH jump host (bastion): the id of another saved SSH session to
    /// tunnel this connection through, like OpenSSH's ProxyJump. Empty = direct.
    /// Single hop only; the jump session supplies its own host/user/auth (#211).
    #[serde(default)]
    pub jump_session_id: String,
    #[serde(default)]
    pub last_used: Option<String>,
    /// Optional folder/group name to organize sessions in the list (#41).
    /// Empty = ungrouped. Sessions are grouped by this in Quick Connect.
    #[serde(default)]
    pub group: String,

    // --- Transport ----------------------------------------------------------
    /// SSH (default), Serial, or Telnet. Absent in old config files → Ssh.
    #[serde(default)]
    pub kind: SessionKind,

    /// WSL distribution and startup directory for generated local sessions.
    /// The directory defaults to the selected distribution user's home (`~`).
    #[serde(default)]
    pub local_distribution: String,
    #[serde(default)]
    pub local_working_dir: String,

    // --- Serial-only fields (ignored unless kind == Serial) -----------------
    /// Serial device path, e.g. "COM3" (Windows) or "/dev/ttyUSB0" (Linux).
    #[serde(default)]
    pub serial_port: String,
    #[serde(default = "default_baud")]
    pub baud_rate: u32,
    #[serde(default = "default_data_bits")]
    pub data_bits: u8,
    #[serde(default = "default_stop_bits")]
    pub stop_bits: u8,
    /// "none" | "odd" | "even".
    #[serde(default = "default_parity")]
    pub parity: String,
    /// "none" | "hardware" | "software".
    #[serde(default = "default_flow")]
    pub flow_control: String,

    /// Character encoding used by the interactive terminal stream (#338).
    /// UTF-8 remains the default for existing and newly created sessions.
    #[serde(default = "default_encoding")]
    pub encoding: String,

    // --- SSH port forwarding / tunnels (#56) --------------------------------
    /// Tunnels established automatically when this SSH session connects.
    #[serde(default)]
    pub forwards: Vec<PortForward>,

    /// Expect/send rules evaluated against interactive terminal output (#212).
    #[serde(default)]
    pub triggers: Vec<SessionTrigger>,

    /// Skip the shell-integration setup (the cwd-follow PROMPT_COMMAND hook + the
    /// remote resource monitor). Those assume a POSIX shell; on a Windows server
    /// whose shell is pwsh/cmd the injected hook breaks the shell. Turn this on
    /// for such servers (#140).
    #[serde(default)]
    pub disable_shell_integration: bool,
    /// Free-form note for this session — somewhere to stash extra info (jump-host
    /// details, credentials hints, owner, etc.). Shown only in the edit dialog.
    /// (B站 suggestion)
    #[serde(default)]
    pub note: String,
}

/// One SSH tunnel (#56). `kind` is "local" (-L), "remote" (-R) or
/// "dynamic" (-D / SOCKS5). For local/remote, `host:host_port` is the target;
/// for dynamic it is ignored (the SOCKS client picks the destination).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PortForward {
    pub kind: String,
    /// Optional label to tell rules apart (#100). Empty = unnamed.
    #[serde(default)]
    pub name: String,
    /// Listener bind address (local side for L/D, remote side for R).
    /// Empty → 127.0.0.1.
    #[serde(default)]
    pub bind_addr: String,
    pub bind_port: u16,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub host_port: u16,
}

/// Automatically send a response when literal terminal output is observed
/// (#212) — e.g. answer a `Password:` prompt or accept a host-key confirmation.
/// `expect` is matched literally against the terminal stream (not a regex), so
/// it stays predictable for operators writing rules by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTrigger {
    pub expect: String,
    /// Stored encrypted like the session password; never logged.
    #[serde(default)]
    pub response: Secret,
    /// Append `\r` after the response so the remote shell accepts it.
    #[serde(default = "default_true")]
    pub append_enter: bool,
    /// False means the rule is consumed after its first match.
    #[serde(default)]
    pub repeat: bool,
}

// Hand-written rather than derived: `Default` must agree with the serde
// defaults, otherwise a rule built in Rust would silently lose its trailing
// Enter while the same rule loaded from disk keeps it.
impl Default for SessionTrigger {
    fn default() -> Self {
        Self {
            expect: String::new(),
            response: Secret::default(),
            append_enter: true,
            repeat: false,
        }
    }
}

impl Session {
    pub fn new_empty() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: String::new(),
            host: String::new(),
            port: 22,
            user: "root".into(),
            auth: AuthMethod::Password,
            password: Secret::default(),
            private_key_path: String::new(),
            private_key_inline: Secret::default(),
            proxy: String::new(),
            proxy_user: String::new(),
            proxy_pass: Secret::default(),
            jump_session_id: String::new(),
            last_used: None,
            group: String::new(),
            kind: SessionKind::Ssh,
            local_distribution: String::new(),
            local_working_dir: String::new(),
            serial_port: String::new(),
            baud_rate: default_baud(),
            data_bits: default_data_bits(),
            stop_bits: default_stop_bits(),
            parity: default_parity(),
            flow_control: default_flow(),
            encoding: default_encoding(),
            forwards: Vec::new(),
            triggers: Vec::new(),
            disable_shell_integration: false,
            note: String::new(),
        }
    }
}

/// A saved quick command (#55): a named snippet the user clicks to send to the
/// active terminal.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuickCommand {
    pub name: String,
    pub command: String,
    /// Optional group/folder name. Empty = the implicit "default" group (#55).
    #[serde(default)]
    pub group: String,
    /// Whether clicking the chip sends + executes (appends Return). `false` only
    /// drops the command into the input box to tweak first. Defaults to `true` so
    /// existing quick commands keep running on click. (B站 suggestion)
    #[serde(default = "default_true")]
    pub send_enter: bool,
}

/// One user-defined client-side terminal highlighting rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputHighlightRule {
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_line: bool,
    /// Stable palette id: red | yellow | green | cyan | magenta | gray.
    #[serde(default)]
    pub color: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn normalize_highlight_color(color: &str) -> &'static str {
    match color {
        "yellow" => "yellow",
        "green" => "green",
        "cyan" => "cyan",
        "magenta" => "magenta",
        "gray" => "gray",
        _ => "red",
    }
}

/// A user-managed WSL launch entry (#310 follow-up, upstream a64097e). An empty
/// list keeps the implicit default WSL entry for backwards compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WslProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub distribution: String,
    #[serde(default = "default_wsl_home")]
    pub directory: String,
}

fn default_wsl_home() -> String {
    "~".to_string()
}

/// On-disk layout. Keep additive to ease forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// User-managed WSL launch entries. An empty list keeps the implicit
    /// default WSL entry for backwards compatibility.
    #[serde(default)]
    pub wsl_profiles: Vec<WslProfile>,
    /// Explicit session groups/folders (#41), including empty ones so a folder
    /// can exist before any session is moved into it. "default" is implicit and
    /// not stored here.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Quick Connect folders that were collapsed when the UI was last used.
    /// `None` is a legacy/new config and starts with every folder collapsed;
    /// `Some([])` means the user explicitly expanded every folder.
    #[serde(default)]
    pub collapsed_session_groups: Option<Vec<String>>,
    /// Saved quick commands (#55).
    #[serde(default)]
    pub quick_commands: Vec<QuickCommand>,
    /// Explicit quick-command group names — mirrors `groups` for sessions so that
    /// empty quick-command groups survive and can be renamed/deleted (#55).
    #[serde(default)]
    pub quick_groups: Vec<String>,
    /// Recent commands sent from the command box, oldest first, capped (#55).
    #[serde(default)]
    pub command_history: Vec<String>,
    /// One-time default-layout migration marker (#new-user-defaults). 0 = config
    /// predates the migration. `migrate_defaults` bumps it to `DEFAULTS_REV` after
    /// pushing the new look (default wallpaper / right-docked
    /// resource panel / wallpaper overlay) to users still sitting on old defaults.
    #[serde(default)]
    pub defaults_rev: u32,

    /// ── 分域设置 ────────────────────────────────────────────────────────
    /// 每个域一个结构体，出厂默认在它自己的 `Default` 里（唯一出处）。
    /// `flatten` 让 on-disk 仍然是平铺 key-value —— 老配置零迁移。
    #[serde(flatten)]
    pub terminal: TerminalSettings,
    #[serde(flatten)]
    pub appearance: AppearanceSettings,
    #[serde(flatten)]
    pub layout: LayoutSettings,
    #[serde(flatten)]
    pub transfer: TransferSettings,
    #[serde(flatten)]
    pub sync: SyncSettings,
    #[serde(flatten)]
    pub update: UpdateSettings,
    #[serde(flatten)]
    pub mcp: McpSettings,
}

/// 落盘形态之一：会话与用户数据 → `sessions.json`。
///
/// 拆分前设置与会话混在同一个文件里，于是每动一个开关都要把含加密口令的全部
/// 会话重新序列化、重新加密再整体重写一遍（拖动滑块时是每帧一次）。拆开之后
/// 这个文件只在会话 / 分组 / 命令历史这类数据变化时才写。
///
/// ⚠️ 给 `ConfigFile` 加字段时必须同步到这里或 `SettingsFile`，否则它永远不会
/// 落盘 —— `the_two_disk_files_cover_every_config_field` 会拦住这种遗漏。
///
/// 读取旧文件也走这里：合并前的 `sessions.json` 里那些设置键被 serde 忽略，
/// 因此老配置无需迁移即可读入（只是设置部分改由 `settings.json` 提供）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionsFile {
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub wsl_profiles: Vec<WslProfile>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub collapsed_session_groups: Option<Vec<String>>,
    #[serde(default)]
    pub quick_commands: Vec<QuickCommand>,
    #[serde(default)]
    pub quick_groups: Vec<String>,
    #[serde(default)]
    pub command_history: Vec<String>,
}

/// 落盘形态之二：设置（6 个分域 + 迁移标记）→ `settings.json`。
///
/// on-disk 仍是平铺 key-value（各域 `flatten`），与拆分前完全一致，所以
/// `settings.json` 也能直接承载一份"合并前"的旧配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsFile {
    #[serde(default)]
    pub defaults_rev: u32,
    #[serde(flatten)]
    pub terminal: TerminalSettings,
    #[serde(flatten)]
    pub appearance: AppearanceSettings,
    #[serde(flatten)]
    pub layout: LayoutSettings,
    #[serde(flatten)]
    pub transfer: TransferSettings,
    #[serde(flatten)]
    pub sync: SyncSettings,
    #[serde(flatten)]
    pub update: UpdateSettings,
    #[serde(flatten)]
    pub mcp: McpSettings,
}

impl SessionsFile {
    /// 从内存态取会话侧的一份拷贝（落盘前调用，随后就地加密）。
    fn from_cache(cache: &ConfigFile) -> Self {
        Self {
            sessions: cache.sessions.clone(),
            wsl_profiles: cache.wsl_profiles.clone(),
            groups: cache.groups.clone(),
            collapsed_session_groups: cache.collapsed_session_groups.clone(),
            quick_commands: cache.quick_commands.clone(),
            quick_groups: cache.quick_groups.clone(),
            command_history: cache.command_history.clone(),
        }
    }
}

impl SettingsFile {
    fn from_cache(cache: &ConfigFile) -> Self {
        Self {
            defaults_rev: cache.defaults_rev,
            terminal: cache.terminal.clone(),
            appearance: cache.appearance.clone(),
            layout: cache.layout.clone(),
            transfer: cache.transfer.clone(),
            sync: cache.sync.clone(),
            update: cache.update.clone(),
            mcp: cache.mcp.clone(),
        }
    }

    /// 设置以 `settings.json` 为准：合并前的旧 `sessions.json` 里也带着一份设置，
    /// 只有在没有 `settings.json` 时才用那份（见 `ConfigStore::load`）。
    fn apply_to(self, cfg: &mut ConfigFile) {
        cfg.defaults_rev = self.defaults_rev;
        cfg.terminal = self.terminal;
        cfg.appearance = self.appearance;
        cfg.layout = self.layout;
        cfg.transfer = self.transfer;
        cfg.sync = self.sync;
        cfg.update = self.update;
        cfg.mcp = self.mcp;
    }
}

/// Portable export file (issue #46): sessions with everything in plaintext
/// **except** the password, which is encrypted with a fixed key baked into the
/// binary so the file opens on *any* machine running Rudder.
///
/// Security note: a built-in key in open-source code is **obfuscation, not real
/// security** — anyone with the source can derive it. It only stops a casual
/// over-the-shoulder read of the file, same level as FinalShell's export.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportFile {
    /// Format marker / version so the schema can evolve later.
    rudder_export: u32,
    sessions: Vec<Session>,
}

pub struct ConfigStore {
    /// `sessions.json`：会话与用户数据。
    path: PathBuf,
    /// `settings.json`：设置。与 `path` **同目录**（密钥与备份都按目录组织）。
    settings_path: PathBuf,
    backup_dir: Option<PathBuf>,
    cache: ConfigFile,
    /// 有改动尚未落盘（防抖写盘：见 `save_debounced` / `flush`）。
    pending: bool,
    /// ChaCha20-Poly1305 key loaded from (or freshly generated into)
    /// `secret.key` in the same directory as `sessions.json`.
    key: [u8; 32],
}

/// Remove duplicate entries in place, keeping the *last* (most recent)
/// occurrence of each and preserving relative order (#113). The list is capped
/// at 200, so the quadratic scan is trivial.
fn dedup_keep_last(items: &mut Vec<String>) {
    let mut i = 0;
    while i < items.len() {
        if items[i + 1..].contains(&items[i]) {
            items.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Display-only session groups that must never be persisted as user folders.
/// `default` maps to an empty group; `system` is owned by built-in local shells.
/// Case-insensitively sort a list of names and drop duplicates (used for
/// session groups / quick-command groups in three modules).
pub(crate) fn dedup_sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort_by_key(|g| g.to_lowercase());
    names.dedup();
    names
}

pub(crate) fn is_reserved_session_group(name: &str) -> bool {
    name.eq_ignore_ascii_case("default") || name.eq_ignore_ascii_case("system")
}

/// Repair configurations created before #324, when the Move-to menu exposed
/// the built-in `system` group as a destination for saved server sessions.
fn normalize_reserved_session_groups(cfg: &mut ConfigFile) -> bool {
    let old_group_count = cfg.groups.len();
    cfg.groups
        .retain(|group| !is_reserved_session_group(group.trim()));
    let mut changed = cfg.groups.len() != old_group_count;
    for session in &mut cfg.sessions {
        if is_reserved_session_group(session.group.trim()) {
            session.group.clear();
            changed = true;
        }
    }
    changed
}

impl ConfigStore {
    /// The prefix that marks an encrypted password blob in sessions.json.
    const ENC_PREFIX: &'static str = "enc:v1:";

    /// Marks a password encrypted with the **portable export key** (issue #46).
    const EXPORT_PREFIX: &'static str = "enc:exp:v1:";

    /// Fixed 32-byte key for portable exports. Baked into the binary so an
    /// exported file decrypts on any machine. Obfuscation only — see `ExportFile`.
    const EXPORT_KEY: [u8; 32] = *b"rudder.export.portable.key.00.01";

    // ── Encryption helpers ────────────────────────────────────────────────

    /// ChaCha20-Poly1305 加密 → `"<prefix><base64url(nonce_12_bytes || ciphertext)>"`。
    ///
    /// 会话密钥（[`Self::encrypt`]）与便携导出密钥（[`Self::encrypt_export`]）的
    /// 实现逐字相同，只差 key / 前缀 / 错误文案三个参数。
    fn encrypt_with(key: &[u8; 32], prefix: &str, label: &str, plaintext: &str) -> Result<String> {
        let cipher = ChaCha20Poly1305::new(key.into());
        // aead 0.6 的 `generate_nonce` 要求 infallible 的 CryptoRngCore，而我们只要系统熵源：
        // 直接取 12 字节，失败必须报错（绝不能退化成固定 nonce）。
        let mut nonce_bytes = [0u8; 12];
        getrandom::fill(&mut nonce_bytes)
            .map_err(|e| anyhow::anyhow!("{label} nonce generation failed: {e}"))?;
        let nonce = chacha20poly1305::aead::Nonce::<ChaCha20Poly1305>::from(nonce_bytes);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("{label} encrypt error: {e}"))?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ciphertext);
        Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(&blob)))
    }

    /// [`Self::encrypt_with`] 的逆操作。返回 `None` 表示这段字符串不是加密载荷
    /// （老版本的明文值、空串、或者被篡改/损坏的 blob）。
    fn decrypt_with(key: &[u8; 32], prefix: &str, s: &str) -> Option<String> {
        let b64 = s.strip_prefix(prefix)?;
        let blob = URL_SAFE_NO_PAD.decode(b64).ok()?;
        if blob.len() < 12 {
            return None;
        }
        let (nonce_bytes, ciphertext) = blob.split_at(12);
        let cipher = ChaCha20Poly1305::new(key.into());
        // `Array::from_slice` 在 hybrid-array 0.4 已废弃，拷进定长数组再构造。
        let mut nonce_arr = [0u8; 12];
        nonce_arr.copy_from_slice(nonce_bytes);
        let nonce = chacha20poly1305::aead::Nonce::<ChaCha20Poly1305>::from(nonce_arr);
        let plain = cipher.decrypt(&nonce, ciphertext).ok()?;
        String::from_utf8(plain).ok()
    }

    /// Encrypt `plaintext` with ChaCha20-Poly1305 and return
    /// `"enc:v1:<base64url(nonce_12_bytes || ciphertext)>"`.
    fn encrypt(key: &[u8; 32], plaintext: &str) -> Result<String> {
        Self::encrypt_with(key, Self::ENC_PREFIX, "password", plaintext)
    }

    /// Try to decrypt a value produced by [`Self::encrypt`].
    /// Returns `None` if the string is not an encrypted blob (e.g. a legacy
    /// plaintext value, an empty string, or a tampered/corrupt blob).
    fn try_decrypt(key: &[u8; 32], s: &str) -> Option<String> {
        Self::decrypt_with(key, Self::ENC_PREFIX, s)
    }

    // ── Key file management ───────────────────────────────────────────────

    /// Load the 32-byte key from `<config_dir>/secret.key`, or generate and
    /// persist a fresh one.  On Unix the key file is created with mode `0600`
    /// so other local accounts cannot read it.  On Windows files in `%APPDATA%`
    /// are already restricted to the owning user by default ACLs.
    fn load_or_create_key(config_dir: &Path) -> Result<[u8; 32]> {
        let key_path = config_dir.join("secret.key");

        if key_path.exists() {
            let bytes = fs::read(&key_path)
                .with_context(|| format!("failed to read {}", key_path.display()))?;
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
            // Do NOT overwrite it in place. This file is the only thing that can
            // decrypt the stored passwords, so a wrong-sized blob is still worth
            // keeping: if the truncation was accidental (partial write, sync
            // conflict) the user has something to recover from, whereas
            // replacing it destroys that chance permanently and silently breaks
            // every saved password.
            let backup = key_path.with_extension("key.bad");
            let _ = fs::remove_file(&backup);
            match fs::rename(&key_path, &backup) {
                Ok(()) => tracing::error!(
                    "secret.key is {} bytes (expected 32) — kept as {}; \
                     stored passwords can no longer be decrypted and must be re-entered",
                    bytes.len(),
                    backup.display()
                ),
                Err(e) => tracing::error!(
                    "secret.key is {} bytes (expected 32) and could not be moved aside ({e}); \
                     stored passwords can no longer be decrypted and must be re-entered",
                    bytes.len()
                ),
            }
        }

        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("OS RNG failed: {e}"))?;
        // 创建时就 0600（老写法是先 `fs::write` 再 `chmod`，中间那段窗口里别的本地
        // 账户就能读走密钥），并且 fsync —— 密钥丢了等于所有已存口令都解不开。
        Self::write_private(&key_path, &key, true)?;
        tracing::info!("generated new encryption key at {}", key_path.display());
        Ok(key)
    }

    // ── Public API ────────────────────────────────────────────────────────

    /// Load (or initialise) the config file. On any parse error we back up the
    /// broken file and start fresh — losing saved sessions is better than
    /// crashing at launch.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        let settings_path = Self::settings_path()?;
        let config_dir = path
            .parent()
            .context("config path has no parent directory")?
            .to_path_buf();

        fs::create_dir_all(&config_dir)
            .with_context(|| format!("failed to create config dir {}", config_dir.display()))?;

        let backup_dir = legacy_data_dir().filter(|dir| dir != &config_dir);
        if let Some(ref backup) = backup_dir {
            restore_user_backup_if_needed(&config_dir, backup);
        }

        let key = Self::load_or_create_key(&config_dir)?;

        // 设置以 `settings.json` 为准。它不存在有两种可能：全新安装，或**合并前
        // 的老配置**（那时设置也写在 `sessions.json` 里，由下面的会话解析兜底读出）。
        // 两种情况都要在本次载入后把 `settings.json` 写出来，之后两文件各自独立。
        let split_pending = !settings_path.exists();
        let settings_file = if split_pending {
            None
        } else {
            Self::read_settings_file(&settings_path)
        };

        let mut migrated = false;
        let mut cache = if path.exists() {
            let raw = crate::files::read_string_capped(
                &path,
                crate::files::MAX_CONFIG_BYTES,
                "session config",
            )?;
            match serde_json::from_str::<ConfigFile>(&raw) {
                Ok(cfg) => cfg,
                Err(err) => {
                    let backup = path.with_extension("json.broken");
                    let _ = fs::rename(&path, &backup);
                    tracing::warn!(
                        "config file was corrupt ({err}); backed up to {}",
                        backup.display()
                    );
                    fresh_config()
                }
            }
        } else {
            fresh_config()
        };
        if let Some(settings) = settings_file {
            settings.apply_to(&mut cache);
        }
        // 解密必须在合并设置**之后**：`webdav_password` 属于 sync 域，它可能来自
        // `settings.json` 而不是 `sessions.json`。
        Self::decrypt_into(&mut cache, &key);
        // Clean up any duplicate history accumulated before #113,
        // keeping the last (most recent) occurrence of each command.
        dedup_keep_last(&mut cache.command_history);
        // `system` and `default` are display-only group names. Older
        // builds allowed moving saved servers into `system`, creating
        // a duplicate empty-menu folder (#324).
        migrated |= normalize_reserved_session_groups(&mut cache);
        // One-time push of the new default layout to existing users
        // (only for items they never changed). (#new-user-defaults)
        migrated |= migrate_defaults(&mut cache);

        let store = Self {
            path,
            settings_path,
            backup_dir,
            cache,
            pending: false,
            key,
        };
        // Persist the migration so it runs exactly once, so a later opt-out is
        // not reverted next launch.
        // `split_pending` 同理：老配置在这里被拆成两个文件，只此一次。
        if (migrated || split_pending)
            && let Err(e) = store.save()
        {
            tracing::warn!("failed to persist config migration: {e:#}");
        }
        Ok(store)
    }

    /// 读 `settings.json`。坏文件与 `sessions.json` 一视同仁：改名留档，回落默认。
    fn read_settings_file(path: &Path) -> Option<SettingsFile> {
        let raw = match crate::files::read_string_capped(
            path,
            crate::files::MAX_CONFIG_BYTES,
            "settings file",
        ) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!("failed to read {}: {e}", path.display());
                return None;
            }
        };
        match serde_json::from_str::<SettingsFile>(&raw) {
            Ok(settings) => Some(settings),
            Err(err) => {
                let backup = path.with_extension("json.broken");
                let _ = fs::rename(path, &backup);
                tracing::warn!(
                    "settings file was corrupt ({err}); backed up to {}",
                    backup.display()
                );
                None
            }
        }
    }

    /// Decrypt every at-rest secret in place; leave legacy plaintext values
    /// untouched (they will be encrypted on next save). A value that still looks
    /// like a blob *after* a failed decrypt means `secret.key` no longer matches
    /// the file — count those so the failure is not silent.
    fn decrypt_into(cfg: &mut ConfigFile, key: &[u8; 32]) {
        let mut undecryptable = 0usize;
        for session in &mut cfg.sessions {
            if let Some(plain) = Self::try_decrypt(key, session.password.as_str()) {
                session.password = Secret::new(plain);
            } else if session.password.as_str().starts_with(Self::ENC_PREFIX) {
                undecryptable += 1;
            }
            if let Some(plain) = Self::try_decrypt(key, session.private_key_inline.as_str()) {
                session.private_key_inline = Secret::new(plain);
            }
            if let Some(plain) = Self::try_decrypt(key, session.proxy_pass.as_str()) {
                session.proxy_pass = Secret::new(plain);
            } else if session.proxy_pass.as_str().starts_with(Self::ENC_PREFIX) {
                undecryptable += 1;
            }
            // Trigger responses share the password's on-disk format;
            // legacy plaintext values stay as-is and are encrypted by
            // the next `save()`.
            for trigger in &mut session.triggers {
                if let Some(plain) = Self::try_decrypt(key, trigger.response.as_str()) {
                    trigger.response = Secret::new(plain);
                }
            }
        }
        if let Some(plain) = Self::try_decrypt(key, cfg.sync.webdav_password.as_str()) {
            cfg.sync.webdav_password = Secret::new(plain);
        }
        if let Some(plain) = Self::try_decrypt(key, cfg.mcp.mcp_token.as_str()) {
            cfg.mcp.mcp_token = Secret::new(plain);
        }
        // 命令历史：旧文件里的明文原样保留（下次 save 时加密），解不开的密文直接丢掉 ——
        // 留着只是一条谁也读不出来的乱码，还会被当成命令补全出来。
        cfg.command_history
            .retain_mut(|cmd| match Self::try_decrypt(key, cmd) {
                Some(plain) => {
                    *cmd = plain;
                    true
                }
                None => !cmd.starts_with(Self::ENC_PREFIX),
            });
        if undecryptable > 0 {
            // Nothing can recover these — `save()` skips values that
            // already carry the prefix, so the stale blobs would
            // otherwise be passed to the server as the password.
            tracing::error!(
                "{undecryptable} stored password(s) could not be decrypted — \
                 secret.key was replaced or corrupt; re-enter them in the session settings"
            );
        }
    }

    fn config_path() -> Result<PathBuf> {
        Ok(data_dir().join(SESSIONS_FILE))
    }

    fn settings_path() -> Result<PathBuf> {
        Ok(data_dir().join(SETTINGS_FILE))
    }

    pub fn sessions(&self) -> &[Session] {
        &self.cache.sessions
    }

    /// Drag-to-reorder a saved session (`dir < 0` = up). The stored Vec order
    /// is the display order within a group (same convention as quick commands).
    /// Same-group hops swap neighbours; when there is no same-group neighbour
    /// the hop crosses the group boundary instead: the session moves into the
    /// nearest visible display group along `dir`, landing at that group's
    /// boundary (first member moving down, last moving up). Returns whether
    /// anything changed.
    pub fn reorder_session(&mut self, id: &str, dir: isize) -> bool {
        // Display group of a session: ungrouped (and reserved names) render
        // under "default"; everything else under its own group. Mirrors
        // build_session_rows in src/app/session_models.rs.
        fn display_group(session: &Session) -> String {
            if session.group.is_empty() || is_reserved_session_group(session.group.trim()) {
                "default".to_string()
            } else {
                session.group.clone()
            }
        }

        let Some(idx) = self.cache.sessions.iter().position(|s| s.id == id) else {
            return false;
        };
        let group = display_group(&self.cache.sessions[idx]);

        // Same-group neighbour in stored (= display) order → plain swap.
        let same_group_target = {
            let sessions = &self.cache.sessions;
            if dir < 0 {
                (0..idx)
                    .rev()
                    .find(|&i| display_group(&sessions[i]) == group)
            } else {
                (idx + 1..sessions.len()).find(|&i| display_group(&sessions[i]) == group)
            }
        };
        if let Some(target) = same_group_target {
            self.cache.sessions.swap(idx, target);
            return true;
        }

        // Cross-group hop. Display order: "default" first (only when ungrouped
        // sessions exist), then named groups — explicit folders ∪ sessions'
        // groups — alphabetically. Collapsed groups are skipped: their rows
        // are hidden, so a card must never land inside one.
        let Some(collapsed_groups) = self.cache.collapsed_session_groups.clone() else {
            // Mirrors build_session_rows: no collapse list = everything
            // collapsed, so no visible target group can exist.
            return false;
        };
        let is_collapsed = |name: &str| collapsed_groups.iter().any(|g| g == name);

        let mut display: Vec<String> = Vec::new();
        if self
            .cache
            .sessions
            .iter()
            .any(|s| display_group(s) == "default")
        {
            display.push("default".to_string());
        }
        let named: Vec<String> = self
            .cache
            .groups
            .iter()
            .filter(|g| !is_reserved_session_group(g.trim()))
            .cloned()
            .chain(
                self.cache
                    .sessions
                    .iter()
                    .map(display_group)
                    .filter(|g| g != "default"),
            )
            .collect();
        display.extend(crate::config::dedup_sorted(named));

        let Some(pos) = display.iter().position(|g| g == &group) else {
            return false;
        };
        let target_group = if dir < 0 {
            (0..pos).rev().find(|&i| !is_collapsed(&display[i]))
        } else {
            (pos + 1..display.len()).find(|&i| !is_collapsed(&display[i]))
        }
        .map(|i| display[i].clone());
        let Some(target_group) = target_group else {
            return false;
        };

        let source_group = self.cache.sessions[idx].group.clone();
        let mut moved = self.cache.sessions.remove(idx);
        moved.group = if target_group == "default" {
            String::new()
        } else {
            target_group.clone()
        };
        let insert_at = {
            let members: Vec<usize> = self
                .cache
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| display_group(s) == target_group)
                .map(|(i, _)| i)
                .collect();
            if members.is_empty() {
                idx.min(self.cache.sessions.len())
            } else if dir < 0 {
                members[members.len() - 1] + 1
            } else {
                members[0]
            }
        };
        self.cache.sessions.insert(insert_at, moved);

        // A named source group that just lost its last member would vanish
        // from the list (changing the row count mid-drag, which drops the
        // dragging row's pointer grab); keep it as an empty folder.
        if group != "default"
            && !self.cache.sessions.iter().any(|s| s.group == source_group)
            && !self.cache.groups.iter().any(|g| g == &source_group)
        {
            self.add_group(source_group);
        }
        true
    }

    pub fn upsert(&mut self, mut session: Session) {
        if is_reserved_session_group(session.group.trim()) {
            session.group.clear();
        }
        if let Some(existing) = self.cache.sessions.iter_mut().find(|s| s.id == session.id) {
            *existing = session;
        } else {
            self.cache.sessions.push(session);
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.cache.sessions.retain(|s| s.id != id);
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.cache.sessions.iter().find(|s| s.id == id)
    }

    pub fn download_dir(&self) -> &str {
        &self.cache.transfer.download_dir
    }

    pub fn set_download_dir(&mut self, dir: String) {
        self.cache.transfer.download_dir = dir;
    }

    /// UI language code ("zh" default / "en").
    pub fn language(&self) -> &str {
        if self.cache.appearance.language.is_empty() {
            "zh"
        } else {
            &self.cache.appearance.language
        }
    }

    pub fn set_language(&mut self, lang: String) {
        self.cache.appearance.language = lang;
    }

    /// Theme preference: "system" (default) | "dark" | "light".
    ///
    /// 未知取值（含老配置里的空串与大小写变体）一律按 "system"。
    pub fn theme_pref(&self) -> &str {
        match self.cache.appearance.theme_pref.as_str() {
            "dark" | "light" => &self.cache.appearance.theme_pref,
            _ => "system",
        }
    }

    /// 只接受三个取值，其余（含大小写变体）落到 "system" —— 免得界面下拉框回落到一个
    /// 既不是"跟随系统"也认不出来的空档。
    pub fn set_theme_pref(&mut self, pref: String) {
        self.cache.appearance.theme_pref = match pref.as_str() {
            "dark" | "light" => pref,
            _ => "system".to_string(),
        };
    }

    /// 主题色："" = 出厂默认 / 预设 id / "#RRGGBB"。
    pub fn accent(&self) -> &str {
        &self.cache.appearance.accent
    }

    pub fn set_accent(&mut self, accent: String) {
        self.cache.appearance.accent = accent;
    }

    /// 设置页用的**三档语义值**（`auto` / `wgpu` / `software`），不分平台。
    ///
    /// 各平台的 [`Self::renderer_mode`] 返回的是"喂给 Slint 的后端名"，三平台并不一致
    /// （macOS 把非软件档一律归一化成 `femtovg-wgpu`）。设置页是按下标高亮 / 精确字符串
    /// 匹配的，拿后端名去比语义档位会一个都匹配不上 —— 表现为"切换没反应、也没有高亮"。
    /// 所以播种与写入都走这个统一语义值。
    pub fn renderer_mode_choice(&self) -> &str {
        match self.cache.appearance.renderer_mode.as_str() {
            "software" => "software",
            // 旧值与后端名都归到 GPU 档：OpenGL 版 `gpu`、已下架的 `skia*`，
            // 以及 macOS 写进配置的 `femtovg-wgpu`（它就是 GPU 档本身）。
            "wgpu" | "gpu" | "skia" | "skia-vulkan" | "femtovg" | "femtovg-wgpu" => "wgpu",
            // 空串（出厂默认）与一切认不出的值都回"自动"，由启动探测决定。
            _ => "auto",
        }
    }

    /// 把设置页传来的三档语义值写进配置。
    ///
    /// 这里**直写 cache、不走** [`Self::set_renderer_mode`]：后者是分平台的，macOS 那版
    /// 只认 `software`、把其余一切（含 `auto`）都压成 `femtovg-wgpu` —— 那样"自动"这一档
    /// 在 macOS 上永远存不下来，下拉框会自己跳回 GPU。
    pub fn set_renderer_mode_choice(&mut self, choice: &str) {
        self.cache.appearance.renderer_mode = match choice {
            "software" => "software".into(),
            "wgpu" => "wgpu".into(),
            _ => "auto".into(),
        };
    }

    /// Renderer preference for the current platform.
    /// 矩阵：软件 / FemtoVG(wgpu→Metal)。界面上的名字是"自动 / GPU / 软件"三档。
    /// `auto` 由启动时的探测固化成后两者之一（与 Windows / Linux 同一套），所以这里
    /// 不需要返回 `auto` —— `_` 兜底到 GPU 档，只防配置被手改坏。
    #[cfg(target_os = "macos")]
    pub fn renderer_mode(&self) -> &str {
        match self.cache.appearance.renderer_mode.as_str() {
            "software" => "software",
            _ => "femtovg-wgpu",
        }
    }

    /// **默认 `auto`**：首次启动探测一次（这台机器有没有真 GPU、femtovg-wgpu 能不能真渲染
    /// 出一帧），有 GPU 就存 `wgpu`、没有就存 `software`，之后启动直接读配置。
    ///
    /// #224 那批高 DPI / 虚拟机兼容问题由这条探测兜住 —— 不再靠"把默认值压成软件渲染"。
    /// 不能指望 Slint 自己的回退：它对 femtovg 的延迟上下文创建无能为力（虚拟机里就是
    /// "窗口打不开"），所以探测必须自己起子进程实测。
    /// 矩阵：自动（默认）/ 软件 / FemtoVG(wgpu→D3D12)。
    /// 旧值 `gpu`（OpenGL）已退役 → 升到 `wgpu`；瞎值 → `auto`（重新探测一次）。
    #[cfg(target_os = "windows")]
    pub fn renderer_mode(&self) -> &str {
        match self.cache.appearance.renderer_mode.as_str() {
            "auto" => "auto",
            "wgpu" => "wgpu",
            "software" => "software",
            "gpu" => "wgpu",
            _ => "auto",
        }
    }

    /// Linux previously used Slint's automatic renderer selection and had no
    /// settings entry. Keep that behaviour for existing configurations.
    /// 矩阵：自动（默认）/ 软件 / FemtoVG(wgpu→Vulkan)。`auto` 由启动时的探测固化成
    /// 后两者之一（与 Windows 同一套）。旧值 `gpu`（OpenGL）与支线早期的 `skia-vulkan`
    /// 都已退役 → 升到 `wgpu`。
    #[cfg(target_os = "linux")]
    pub fn renderer_mode(&self) -> &str {
        match self.cache.appearance.renderer_mode.as_str() {
            "software" => "software",
            "wgpu" => "wgpu",
            "gpu" => "wgpu",
            "skia-vulkan" => "wgpu",
            _ => "auto",
        }
    }

    #[cfg(target_os = "macos")]
    pub fn set_renderer_mode(&mut self, mode: String) {
        self.cache.appearance.renderer_mode = match mode.as_str() {
            "software" => "software".into(),
            _ => "femtovg-wgpu".into(),
        };
    }

    #[cfg(target_os = "windows")]
    pub fn set_renderer_mode(&mut self, mode: String) {
        self.cache.appearance.renderer_mode = match mode.as_str() {
            "auto" => "auto".into(),
            "wgpu" => "wgpu".into(),
            "software" => "software".into(),
            "gpu" => "wgpu".into(),
            _ => "auto".into(),
        };
    }

    #[cfg(target_os = "linux")]
    pub fn set_renderer_mode(&mut self, mode: String) {
        self.cache.appearance.renderer_mode = match mode.as_str() {
            "software" => "software".into(),
            "wgpu" => "wgpu".into(),
            "gpu" => "wgpu".into(),
            "skia-vulkan" => "wgpu".into(),
            _ => "auto".into(),
        };
    }

    /// Terminal font family ("" = built-in default).
    pub fn font_family(&self) -> &str {
        &self.cache.terminal.font_family
    }

    pub fn set_font_family(&mut self, family: String) {
        self.cache.terminal.font_family = family;
    }

    /// UI font family (empty = auto-detect at startup).
    pub fn ui_font_family(&self) -> &str {
        if self.cache.appearance.ui_font_family.is_empty() {
            ""
        } else {
            &self.cache.appearance.ui_font_family
        }
    }

    pub fn set_ui_font_family(&mut self, family: String) {
        self.cache.appearance.ui_font_family = family;
    }

    /// Terminal font size in px (falls back to 13 when unset).
    pub fn font_size(&self) -> u32 {
        // 读路径同样夹取：配置是用户可手改的，`"fontSize": 9999` 会直接进
        // `Theme.term-font-size` 并按它排版（写路径一直夹在 8..=32，只有 setter 被测试覆盖）。
        let size = if self.cache.terminal.font_size == 0 {
            13
        } else {
            self.cache.terminal.font_size
        };
        size.clamp(8, 32)
    }

    pub fn set_font_size(&mut self, size: u32) {
        self.cache.terminal.font_size = size.clamp(8, 32);
    }

    /// Force regular terminal text to render with a bold face (#262).
    pub fn terminal_bold(&self) -> bool {
        self.cache.terminal.terminal_bold
    }

    pub fn set_terminal_bold(&mut self, bold: bool) {
        self.cache.terminal.terminal_bold = bold;
    }

    pub fn scrollback_lines(&self) -> usize {
        // 读路径同样夹取：配置文件是用户可手改的，`"scrollbackLines": 999999999` 会一路
        // 传到 alacritty 的 `TermConfig.scrolling_history`，让回滚缓冲按那个上限去扩张
        // （写路径一直有夹取，读路径此前没有 —— 只有 setter 测试，掩盖了这个不对称）。
        let max = if self.cache.terminal.large_scrollback {
            SCROLLBACK_MAX_LARGE
        } else {
            SCROLLBACK_MAX
        };
        self.cache.terminal.scrollback_lines.clamp(100, max)
    }

    pub fn set_scrollback_lines(&mut self, lines: usize) {
        // 上界取决于「大回滚缓冲区」开关（A3）：默认 10 万行，开关打开后 100 万行。
        let max = if self.cache.terminal.large_scrollback {
            SCROLLBACK_MAX_LARGE
        } else {
            SCROLLBACK_MAX
        };
        self.cache.terminal.scrollback_lines = lines.clamp(100, max);
    }

    /// 大回滚缓冲区开关（A4）。
    pub fn large_scrollback(&self) -> bool {
        self.cache.terminal.large_scrollback
    }
    /// 切换大回滚缓冲区：**开启不动现值**；关闭时把现值收回常规上限，
    /// 否则就会留下一个"上限 10 万但存着 50 万"的不一致状态。
    pub fn set_large_scrollback(&mut self, on: bool) {
        self.cache.terminal.large_scrollback = on;
        if !on && self.cache.terminal.scrollback_lines > SCROLLBACK_MAX {
            self.cache.terminal.scrollback_lines = SCROLLBACK_MAX;
        }
    }

    /// Convert LF to CRLF in pasted/typed text.
    pub fn convert_eol(&self) -> bool {
        self.cache.terminal.convert_eol
    }
    pub fn set_convert_eol(&mut self, v: bool) {
        self.cache.terminal.convert_eol = v;
    }
    /// Allow OSC 52 clipboard writes from remote programs.
    pub fn osc52_clipboard(&self) -> bool {
        self.cache.terminal.osc52_clipboard
    }

    pub fn webdav_cert_pin(&self) -> String {
        self.cache.sync.webdav_cert_pin.clone()
    }

    pub fn set_osc52_clipboard(&mut self, v: bool) {
        self.cache.terminal.osc52_clipboard = v;
    }
    /// Hide special partitions in the resource panel.
    pub fn hide_special_partitions(&self) -> bool {
        self.cache.appearance.hide_special_partitions
    }
    pub fn set_hide_special_partitions(&mut self, v: bool) {
        self.cache.appearance.hide_special_partitions = v;
    }
    /// Aggressive memory reclaim; read once at startup and applied on the next launch.
    pub fn aggressive_memory_reclaim(&self) -> bool {
        self.cache.appearance.aggressive_memory_reclaim
    }
    pub fn set_aggressive_memory_reclaim(&mut self, v: bool) {
        self.cache.appearance.aggressive_memory_reclaim = v;
    }
    /// All sidebar / panel animations enabled (Interface › Animations).
    pub fn animations_enabled(&self) -> bool {
        !self.cache.appearance.animations_disabled
    }
    pub fn set_animations_enabled(&mut self, v: bool) {
        self.cache.appearance.animations_disabled = !v;
    }
    /// Mount-point filter for the resource panel.
    pub fn mount_filter(&self) -> &str {
        &self.cache.appearance.mount_filter
    }
    pub fn set_mount_filter(&mut self, v: String) {
        self.cache.appearance.mount_filter = v;
    }

    /// Selected terminal insertion cursor shape. Legacy and invalid values use
    /// the existing block cursor so upgrades preserve the current appearance.
    pub fn terminal_cursor_style(&self) -> &str {
        match self.cache.terminal.terminal_cursor_style.as_str() {
            "block" => "block",
            "underline" => "underline",
            // 未知值回到出厂默认（竖线），不再硬编码成另一种形状 ——
            // 否则"默认值"与"非法值兜底"会是两个不同的答案。
            _ => "bar",
        }
    }

    pub fn set_terminal_cursor_style(&mut self, style: String) {
        self.cache.terminal.terminal_cursor_style = match style.as_str() {
            "block" => "block".into(),
            "underline" => "underline".into(),
            // 非法值回落出厂默认（竖线），与 getter 的兜底保持一致。
            _ => "bar".into(),
        };
    }

    /// 光标颜色；**空串 = 跟随主题**（深浅切换时按当前档位重新解析）。
    ///
    /// ⚠️ `#FFFFFF` 是"跟随主题"落地之前写死的出厂默认值：那个版本不管深浅档都给白光标，
    /// 浅色主题下几乎看不见。旧配置按**跟随主题**处理 —— 它是默认值，不是"用户显式挑了白"。
    pub fn terminal_cursor_color(&self) -> &str {
        let stored = self.cache.terminal.terminal_cursor_color.as_str();
        let legacy_white = stored == "#FFFFFF";
        if legacy_white || normalize_hex_color(stored).is_none() {
            ""
        } else {
            stored
        }
    }

    pub fn set_terminal_cursor_color(&mut self, color: &str) -> bool {
        // 空串不是一个颜色，而是"跟随主题"这个选项本身。
        let trimmed = color.trim();
        if trimmed.is_empty() {
            self.cache.terminal.terminal_cursor_color = String::new();
            return true;
        }
        let Some(normalized) = normalize_hex_color(trimmed) else {
            return false;
        };
        self.cache.terminal.terminal_cursor_color = normalized;
        true
    }

    /// Whether client-side highlighting of otherwise unstyled output is active.
    pub fn output_highlight_enabled(&self) -> bool {
        !self.cache.terminal.output_highlight_disabled
    }

    pub fn set_output_highlight_enabled(&mut self, enabled: bool) {
        self.cache.terminal.output_highlight_disabled = !enabled;
    }

    /// Whether complete JSON lines are pretty-printed and syntax-coloured
    /// (#338). Stored inverted so formatting is on by default.
    pub fn json_format_output(&self) -> bool {
        !self.cache.terminal.json_format_disabled
    }

    pub fn set_json_format_output(&mut self, enabled: bool) {
        self.cache.terminal.json_format_disabled = !enabled;
    }

    /// Selected built-in rule set. The default is the Rudder built-in
    /// (tailspin-style) rule set; unknown values fall back to it as well.
    pub fn output_highlight_preset(&self) -> &str {
        match self.cache.terminal.output_highlight_preset.as_str() {
            "devops" => "devops",
            "log" => "log",
            _ => "builtin",
        }
    }

    pub fn set_output_highlight_preset(&mut self, preset: String) {
        self.cache.terminal.output_highlight_preset = match preset.as_str() {
            "devops" => "devops".to_string(),
            "log" => "log".to_string(),
            _ => "builtin".to_string(),
        };
    }

    pub fn output_highlight_rules(&self) -> &[OutputHighlightRule] {
        &self.cache.terminal.output_highlight_rules
    }

    pub fn add_output_highlight_rule(&mut self, mut rule: OutputHighlightRule) {
        rule.pattern = rule.pattern.trim().to_string();
        rule.color = normalize_highlight_color(&rule.color).to_string();
        self.cache.terminal.output_highlight_rules.push(rule);
    }

    pub fn remove_output_highlight_rule(&mut self, index: usize) {
        if index < self.cache.terminal.output_highlight_rules.len() {
            self.cache.terminal.output_highlight_rules.remove(index);
        }
    }

    pub fn set_output_highlight_rule_enabled(&mut self, index: usize, enabled: bool) {
        if let Some(rule) = self.cache.terminal.output_highlight_rules.get_mut(index) {
            rule.enabled = enabled;
        }
    }

    /// 整表替换（MCP 的 `terminal.output_highlight` 用）。逐条 `add` 做不到"删掉
    /// 列表里没有的那几条"，而界面的增删接口是按下标操作的。
    pub fn set_output_highlight_rules(&mut self, rules: Vec<OutputHighlightRule>) {
        self.cache.terminal.output_highlight_rules = rules
            .into_iter()
            .map(|mut rule| {
                rule.pattern = rule.pattern.trim().to_string();
                rule.color = normalize_highlight_color(&rule.color).to_string();
                rule
            })
            .collect();
    }

    /// Global UI scale in percent (#100). Defaults to 100.
    pub fn ui_scale(&self) -> u32 {
        if self.cache.appearance.ui_scale == 0 {
            100
        } else {
            self.cache.appearance.ui_scale
        }
    }

    pub fn set_ui_scale(&mut self, percent: u32) {
        self.cache.appearance.ui_scale = percent.clamp(80, 200);
    }

    /// Immersive wallpaper id ("" = none).
    pub fn wallpaper(&self) -> &str {
        &self.cache.appearance.wallpaper
    }

    pub fn set_wallpaper(&mut self, id: impl Into<String>) {
        self.cache.appearance.wallpaper = id.into();
    }

    /// Whether the SFTP panel follows the terminal's cd (default true).
    pub fn sftp_follow_cd(&self) -> bool {
        !self.cache.transfer.sftp_no_follow_cd
    }

    pub fn set_sftp_follow_cd(&mut self, follow: bool) {
        self.cache.transfer.sftp_no_follow_cd = !follow;
    }

    /// Saved quick commands (#55).
    pub fn quick_commands(&self) -> &[QuickCommand] {
        &self.cache.quick_commands
    }

    pub fn set_quick_commands(&mut self, cmds: Vec<QuickCommand>) {
        self.cache.quick_commands = cmds;
    }

    /// User-managed WSL launch entries (#310 follow-up).
    pub fn wsl_profiles(&self) -> &[WslProfile] {
        &self.cache.wsl_profiles
    }

    pub fn add_wsl_profile(&mut self, name: String, distribution: String, directory: String) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.cache.wsl_profiles.push(WslProfile {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            distribution: distribution.trim().to_string(),
            directory: match directory.trim() {
                "" => "~".to_string(),
                value => value.to_string(),
            },
        });
    }

    pub fn remove_wsl_profile(&mut self, id: &str) {
        self.cache.wsl_profiles.retain(|profile| profile.id != id);
    }

    /// 整表替换（MCP 的 `wsl.profiles` 用）。名字与目录在这里统一归一化，
    /// 免得 MCP 与界面两条路径写出不一样的形状。
    pub fn set_wsl_profiles(&mut self, profiles: Vec<WslProfile>) {
        self.cache.wsl_profiles = profiles
            .into_iter()
            .filter_map(|mut profile| {
                profile.name = profile.name.trim().to_string();
                if profile.name.is_empty() {
                    return None;
                }
                profile.distribution = profile.distribution.trim().to_string();
                profile.directory = match profile.directory.trim() {
                    "" => "~".to_string(),
                    value => value.to_string(),
                };
                Some(profile)
            })
            .collect();
    }

    pub fn quick_panel_open(&self) -> bool {
        self.cache.layout.quick_panel_open
    }

    pub fn quick_commands_as_sidebar(&self) -> bool {
        self.cache.layout.quick_commands_as_sidebar
    }

    pub fn set_quick_commands_as_sidebar(&mut self, enabled: bool) {
        self.cache.layout.quick_commands_as_sidebar = enabled;
        if !enabled {
            self.cache.layout.quick_panel_open = false;
        }
    }

    pub fn set_quick_panel_open(&mut self, open: bool) {
        self.cache.layout.quick_panel_open = open;
    }

    pub fn quick_panel_collapsed(&self) -> bool {
        self.cache.layout.quick_panel_collapsed
    }

    pub fn set_quick_panel_collapsed(&mut self, collapsed: bool) {
        self.cache.layout.quick_panel_collapsed = collapsed;
    }

    pub fn quick_panel_width(&self) -> f32 {
        let width = self.cache.layout.quick_panel_width;
        if width <= 0.0 {
            default_quick_panel_width()
        } else {
            width
        }
    }

    pub fn set_quick_panel_width(&mut self, width: f32) {
        self.cache.layout.quick_panel_width = width;
    }

    pub fn quick_panel_height(&self) -> f32 {
        let height = self.cache.layout.quick_panel_height;
        if height <= 0.0 {
            default_quick_panel_height()
        } else {
            height
        }
    }

    pub fn set_quick_panel_height(&mut self, height: f32) {
        self.cache.layout.quick_panel_height = height;
    }

    /// Explicit quick-command groups (#55) — parallels [`groups`](Self::groups).
    pub fn quick_groups(&self) -> &[String] {
        &self.cache.quick_groups
    }

    /// Create an empty quick-command group. Ignores blank, "default", duplicates.
    pub fn add_quick_group(&mut self, name: String) {
        let n = name.trim().to_string();
        if n.is_empty() || n.eq_ignore_ascii_case("default") {
            return;
        }
        if !self.cache.quick_groups.iter().any(|g| g == &n) {
            self.cache.quick_groups.push(n);
        }
    }

    /// Delete a quick-command group; any command still in it falls back to
    /// ungrouped (the UI only offers delete on empty groups, but clear defensively).
    pub fn remove_quick_group(&mut self, name: &str) {
        self.cache.quick_groups.retain(|g| g != name);
        for c in &mut self.cache.quick_commands {
            if c.group == name {
                c.group.clear();
            }
        }
    }

    /// Rename a quick-command group, moving its commands along. No-op for
    /// blank / "default".
    pub fn rename_quick_group(&mut self, old: &str, new: String) {
        let n = new.trim().to_string();
        if n.is_empty() || n.eq_ignore_ascii_case("default") || n == old {
            return;
        }
        for g in &mut self.cache.quick_groups {
            if g == old {
                *g = n.clone();
            }
        }
        for c in &mut self.cache.quick_commands {
            if c.group == old {
                c.group = n.clone();
            }
        }
        self.cache.quick_groups.sort();
        self.cache.quick_groups.dedup();
    }

    /// Update one quick command in place by index (#55).
    pub fn update_quick_command(&mut self, index: usize, cmd: QuickCommand) {
        if let Some(slot) = self.cache.quick_commands.get_mut(index) {
            *slot = cmd;
        }
    }

    /// Recent command-box history, oldest first (#55).
    pub fn command_history(&self) -> &[String] {
        &self.cache.command_history
    }

    /// Append a command to the history: skips blanks, de-duplicates globally so
    /// each command appears once, and re-appends at the end so the most-recently
    /// used command is always last. Capped so it can't grow without bound (#113).
    pub fn push_command_history(&mut self, cmd: String) {
        if cmd.trim().is_empty() {
            return;
        }
        // Drop any earlier occurrence, then push → no duplicates and "last used"
        // moves to the end (bash `HISTCONTROL=erasedups` semantics).
        self.cache.command_history.retain(|c| c != &cmd);
        const CAP: usize = 200;
        self.cache.command_history.push(cmd);
        let len = self.cache.command_history.len();
        if len > CAP {
            self.cache.command_history.drain(0..len - CAP);
        }
    }

    /// Remove a single command-history entry by storage index (#96).
    pub fn remove_command_history(&mut self, index: usize) {
        if index < self.cache.command_history.len() {
            self.cache.command_history.remove(index);
        }
    }

    /// Collapse the left navigation on startup (default false) (#78).
    pub fn collapse_sidebar_default(&self) -> bool {
        self.cache.layout.collapse_sidebar_default
    }

    pub fn set_collapse_sidebar_default(&mut self, v: bool) {
        self.cache.layout.collapse_sidebar_default = v;
    }

    /// 上次的导航折叠态；None 表示回退到 `collapse_sidebar_default`。
    pub fn sidebar_collapsed(&self) -> Option<bool> {
        self.cache.layout.sidebar_collapsed
    }
    pub fn set_sidebar_collapsed(&mut self, v: bool) {
        self.cache.layout.sidebar_collapsed = Some(v);
    }
    /// Whether the startup new-version check is enabled (#184).
    pub fn update_check_enabled(&self) -> bool {
        !self.cache.update.update_check_disabled
    }
    pub fn set_update_check_enabled(&mut self, enabled: bool) {
        self.cache.update.update_check_disabled = !enabled;
    }

    /// 更新通道：`stable` / `beta` / `all`。未知值（含空串）一律回落 `stable`，
    /// 于是老配置与手改坏的值都表现为"只提示正式版"。
    pub fn update_channel(&self) -> &str {
        match self.cache.update.update_channel.as_str() {
            "beta" => "beta",
            "all" => "all",
            _ => "stable",
        }
    }

    pub fn set_update_channel(&mut self, channel: String) {
        self.cache.update.update_channel = match channel.as_str() {
            "beta" => "beta".into(),
            "all" => "all".into(),
            _ => "stable".into(),
        };
    }

    /// 检查频率：`startup`（每次启动，默认）/ `daily` / `weekly` / `monthly` /
    /// `semiannual`（每半年）/ `yearly`。未知值一律回落 `startup`。
    ///
    /// 取值列在 [`UPDATE_FREQUENCIES`] —— 它是**唯一出处**：设置页的下拉标签按同一个顺序
    /// 排列，节流区间见 `app::updater::check_interval_secs`。
    pub fn update_frequency(&self) -> &str {
        match self.cache.update.update_check_frequency.as_str() {
            v if UPDATE_FREQUENCIES.contains(&v) => v,
            _ => UPDATE_FREQUENCIES[0],
        }
    }

    pub fn set_update_frequency(&mut self, frequency: String) {
        self.cache.update.update_check_frequency = match frequency.as_str() {
            v if UPDATE_FREQUENCIES.contains(&v) => v.into(),
            _ => UPDATE_FREQUENCIES[0].into(),
        };
    }

    /// 更新流量是否走代理（默认关闭 = 直连）。
    pub fn update_proxy_enabled(&self) -> bool {
        self.cache.update.update_proxy_enabled
    }

    pub fn set_update_proxy_enabled(&mut self, v: bool) {
        self.cache.update.update_proxy_enabled = v;
    }

    /// 更新代理地址（空 = 跟随系统）。只作用于更新检查 / 下载。
    pub fn update_proxy(&self) -> &str {
        self.cache.update.update_proxy.trim()
    }

    pub fn set_update_proxy(&mut self, v: String) {
        self.cache.update.update_proxy = v.trim().to_string();
    }

    pub fn update_last_check(&self) -> i64 {
        self.cache.update.update_last_check_unix
    }

    pub fn set_update_last_check(&mut self, unix_secs: i64) {
        self.cache.update.update_last_check_unix = unix_secs.max(0);
    }



}

#[path = "store_mcp.rs"] mod mcp;
#[path = "store_groups.rs"] mod groups;
#[path = "store_export.rs"] mod export;

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests_aggregate;
