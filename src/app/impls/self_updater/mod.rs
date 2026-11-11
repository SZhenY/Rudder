//! 在线一键更新（GitHub Releases）。
//!
//! 流程：列 release → 按平台挑资产 → 下载（进度）→ 安装 → 重启。
//! 安装分两条路：
//! - 便携版（zip / tar.gz，以及 macOS 的 `.app`）：解压后原地替换，
//!   由用户点"重启"生效；
//! - Windows MSI 安装：不碰文件，把下载好的 `.msi` 交给 `msiexec`（UAC 提权），
//!   应用随即退出让出 exe 的文件锁。
//!
//! 与 `updater.rs` 的分工：后者只负责"有新版本"的检查与对话框；本模块负责
//! 真正的下载与替换。
//!
//! 为什么不用 `self_update` 的自动资产匹配：Rudder 的 release 资产按
//! `matrix.name` 命名（`rudder-<ver>-macos-aarch64.zip`），不含 target triple，
//! `asset_for(get_target())` 匹配不到——所以走手动流程。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

mod platform;
pub(crate) use platform::*;
mod discover;
pub(crate) use discover::*;
mod install;
pub(crate) use install::*;
mod notes;
pub(crate) use notes::*;

/// 这个候选是不是"**降级 / 切换**"（目标版本比本机低）而不是升级？
///
/// 只在「正式版」通道上提供切换：本机跑的是 `0.7.9-beta1` 这类测试版、而正式版最新是
/// `0.7.9` 时，用户**已经明确选了正式版通道**，那就该告诉他"可切换到旧版本"，
/// 而不是一句"已是最新版本"（用户 2026-09-21 的要求）。
///
/// `all` / `beta` 通道永不提供降级 —— 它们的语义就是"要更新的"；而且本机跑着比线上
/// 一切都新的自编译版本时（很常见），在那两个通道上弹降级只会莫名其妙。
pub(crate) fn is_channel_switch(channel: &str, current: super::Version, latest: super::Version) -> bool {
    channel == "stable" && latest < current
}

#[cfg(test)]
#[path = "tests.rs"] mod tests;
#[cfg(test)]
#[path = "channel_tests.rs"] mod channel_tests;
#[cfg(test)]
#[path = "notes_tests.rs"] mod notes_tests;
#[cfg(test)]
#[path = "changelog_tests.rs"] mod changelog_tests;
#[cfg(test)]
#[path = "switch_tests.rs"] mod switch_tests;

