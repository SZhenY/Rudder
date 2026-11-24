//! Docker 领域层：纯命令构造 / 输出解析 / 失败分类 / 主机键。
//!
//! 不依赖 Slint、`AppWindow`、`SessionHandle`；UI 控制器与 MCP 工具都从这里取
//! 同一份实现，避免两套命令字符串和两套解析。

#[path = "struct/model.rs"]
pub(crate) mod model;
#[path = "struct/failure.rs"]
pub(crate) mod failure;

#[path = "impls/command.rs"]
pub(crate) mod command;
#[path = "impls/parse.rs"]
pub(crate) mod parse;
#[path = "impls/classify.rs"]
pub(crate) mod classify;

pub(crate) use failure::DockerFailure;
pub(crate) use model::{
    ComposeProjectInfo, ContainerState, DockerContainerInfo, DockerHostKey, DockerImageInfo,
    DockerNetworkInfo, HostStatus, RegistryRepo,
};
