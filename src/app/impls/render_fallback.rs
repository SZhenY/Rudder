use super::*;


/// 兜底重拉时带的环境变量：它在这条进程里时不再兜底，避免反复重启。
pub(crate) const RENDER_FALLBACK_ENV: &str = "RUDDER_RENDER_FALLBACK";

/// 渲染失败后该不该降级到软件渲染。
///
/// 两个前提：当前不是软件渲染（否则问题不在 GPU），且不在兜底重拉出来的进程里
/// （否则会无限重启）。抽成纯函数是为了能测 —— 端到端要造一台渲染不起来的机器才能验。
pub(crate) fn should_fall_back_to_software(choice: &str, already_fell_back: bool) -> bool {
    choice != "software" && !already_fell_back
}

/// GPU 档起不来（首帧渲染失败 / 运行中设备丢失）时：档位降成软件渲染，再重新拉起自己。
///
/// 为什么不原地重试：Slint 的 platform 一旦 `set_platform` 就不能换，换渲染器只能重开进程。
///
/// 返回 `Some` = 不兜底或兜底本身失败，错误交给上层报出去；返回 `None` = 已经重拉了，
/// 本进程安静退出即可。
pub(crate) fn recover_render_failure(
    store: &Rc<RefCell<ConfigStore>>,
    err: slint::PlatformError,
) -> Option<anyhow::Error> {
    let choice = store.borrow().renderer_mode_choice().to_string();
    let already = std::env::var_os(RENDER_FALLBACK_ENV).is_some();
    if !should_fall_back_to_software(&choice, already) {
        return Some(anyhow::anyhow!("event loop exited with error: {err}"));
    }

    // 先落盘：即使下面重拉失败，下次启动也已经是软件渲染，不会卡在同一个坑里。
    {
        let mut s = store.borrow_mut();
        s.set_renderer_mode_choice("software");
        s.save_logging();
    }
    tracing::error!(
        error = %err,
        "renderer failed at runtime; falling back to software rendering"
    );

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Some(anyhow::anyhow!(
                "event loop exited with error: {err}; relaunch failed: {e}"
            ));
        }
    };
    match std::process::Command::new(exe)
        .env(RENDER_FALLBACK_ENV, "1")
        .spawn()
    {
        Ok(_) => None,
        Err(e) => Some(anyhow::anyhow!(
            "event loop exited with error: {err}; relaunch failed: {e}"
        )),
    }
}
#[cfg(test)]
mod renderer_fallback_tests {
    use super::should_fall_back_to_software;

    /// GPU 档起不来才降级；软件渲染也失败说明问题不在 GPU，别兜底。
    #[test]
    fn falls_back_only_from_a_gpu_tier() {
        assert!(should_fall_back_to_software("wgpu", false));
        assert!(should_fall_back_to_software("auto", false));
        assert!(!should_fall_back_to_software("software", false));
    }

    /// 兜底重拉出来的进程里再失败就不能再重启，否则会无限循环。
    #[test]
    fn never_falls_back_twice() {
        assert!(!should_fall_back_to_software("wgpu", true));
        assert!(!should_fall_back_to_software("software", true));
    }
}
