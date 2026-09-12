#[path = "impls/config.rs"]
mod imp;
#[path = "impls/finalshell.rs"]
mod finalshell;
/// 分域设置模型：每个设置页一组，出厂默认在各域的 `Default` 里（唯一出处）。
#[path = "struct/settings.rs"]
pub(crate) mod settings;

pub(crate) use imp::*;
