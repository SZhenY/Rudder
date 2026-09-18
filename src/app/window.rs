use super::*;

#[cfg(target_os = "linux")]
pub(super) fn set_window_icon(window: &AppWindow) {
    use i_slint_backend_winit::winit::window::Icon;
    const ICON_PNG: &[u8] = include_bytes!("../../assets/icon@512.png");
    let Ok(img) = image::load_from_memory(ICON_PNG) else {
        return;
    };
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    let Ok(icon) = Icon::from_rgba(rgba.into_raw(), w, h) else {
        return;
    };
    window
        .window()
        .with_winit_window(|ww| ww.set_window_icon(Some(icon)));
}

#[cfg(windows)]
pub(super) fn apply_window_chrome(window: &slint::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window.with_winit_window(|ww| {
        let Ok(handle) = ww.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = h.hwnd.get();

        #[link(name = "dwmapi")]
        unsafe extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: isize,
                attr: u32,
                pv: *const core::ffi::c_void,
                cb: u32,
            ) -> i32;
        }
        // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2 (Windows 11+).
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;
        unsafe {
            let pref: u32 = DWMWCP_ROUND;
            let corner_hr = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&pref as *const u32).cast(),
                4,
            );
            tracing::debug!("window chrome applied: hwnd={hwnd:#x} corner_hr={corner_hr:#x}");
        }
    });
}

#[cfg(not(windows))]
pub(super) fn apply_window_chrome(_window: &slint::Window) {}

/// `auto` 的探测结果 → 配置里要写的具体取值。
///
/// 抽成纯函数是为了能测：真正的探测要起子进程，但"结果怎么落库"这条契约不该靠人肉观察。
// Windows 与 Linux 的启动路径用得到它（外加测试）；别的平台别让它变成 dead_code。
#[cfg(any(windows, target_os = "linux", test))]
pub(super) const fn auto_renderer_for(probe_ok: bool) -> &'static str {
    if probe_ok {
        "wgpu"
    } else {
        "software"
    }
}

/// Windows / Linux 的"自动"：探测一次，把结论**固化进配置**，之后启动不再探测。
///
/// 探测要花约一秒（子进程真渲染一帧），所以只在用户选"自动"之后的**第一次启动**发生；
/// 写完配置后存的就是 `wgpu` / `software`，后续启动直接用它。想重新探测（比如换了机器
/// 或者装上了显卡驱动）就再选一次"自动"。
///
/// 显式设了 `SLINT_BACKEND` 时不动配置 —— 那个环境变量优先级最高，探测没有意义。
#[cfg(any(windows, target_os = "linux"))]
pub(super) fn resolve_auto_renderer_mode(
    mut config: crate::config::ConfigStore,
) -> crate::config::ConfigStore {
    if config.renderer_mode() != "auto" {
        return config;
    }
    if std::env::var_os("SLINT_BACKEND").is_some() {
        tracing::info!("auto renderer: SLINT_BACKEND is set, leaving the config untouched");
        return config;
    }

    let resolved = auto_renderer_for(gpu_renderer_probe_passes());
    config.set_renderer_mode(resolved.to_owned());
    config.save_logging();
    tracing::info!(
        resolved,
        "auto renderer: probe finished and the result is now stored in the config"
    );
    config
}

/// `auto` 的 GPU 探测：**用子进程真渲染一帧**，成功才算 GPU 可用。
///
/// 为什么不指望 Slint 自己的回退：`create_renderer` 只在**渲染器工厂返回 `Err`** 时才走
/// `try_create_window_with_fallback_renderer`，而 femtovg 是延迟创建 GL 上下文的
/// （`new_suspended`）—— 工厂在虚拟机里照样成功，真正的失败发生在**首帧**。于是回退链
/// 根本跑不到，"自动"的表现就是窗口打不开。这里用子进程实测，它无法作弊。
///
/// 探测进程用 `--probe-renderer=wgpu` 启动，而 `wgpu` 这条取值不会再探测，所以不会递归。
#[cfg(any(windows, target_os = "linux"))]
fn gpu_renderer_probe_passes() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let status = std::process::Command::new(exe)
        .arg("--probe-renderer=wgpu")
        // 探测的是 femtovg 本身，不受外部 SLINT_BACKEND 影响。
        .env("SLINT_BACKEND", "winit-femtovg-wgpu")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(status, Ok(s) if s.success())
}

/// 渲染探测进程：用指定渲染器起一个**屏幕外**的小窗口，渲染一帧后正常退出。
///
/// 退出码即结论：0 = 这个渲染器在这台机器上能用；非 0（含 panic / abort）= 不能用。
/// 取值：Windows / Linux 用 `wgpu` / `software`，macOS 用 `femtovg-wgpu` / `skia` / `software`。
pub(super) fn run_renderer_probe(mode: &str) -> anyhow::Result<()> {
    use slint::ComponentHandle as _;

    #[cfg(windows)]
    setup_windows_platform(mode);
    #[cfg(target_os = "linux")]
    setup_linux_platform(mode);
    #[cfg(target_os = "macos")]
    setup_macos_platform(mode);

    let probe = crate::ui::RendererProbe::new()?;
    // 屏幕外，但**是可见窗口** —— 隐藏窗口不会触发渲染，而"能不能渲染"正是要测的东西。
    // 位置在 `run()` 之前就设好，所以第一帧已经画在屏幕外，不会闪。
    probe
        .window()
        .set_position(slint::PhysicalPosition::new(-32_000, -32_000));

    let quit = slint::Timer::default();
    quit.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(900),
        || {
            let _ = slint::quit_event_loop();
        },
    );
    probe.run()?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn setup_windows_platform(renderer_mode: &str) {
    use i_slint_backend_winit::winit::platform::windows::WindowAttributesExtWindows;

    let mut builder = i_slint_backend_winit::Backend::builder();
    let configured_renderer = match renderer_mode {
        "software" => Some("software".to_owned()),
        "wgpu" => Some("femtovg-wgpu".to_owned()),
        // 旧配置里的 `gpu`（OpenGL 版 FemtoVG）已从矩阵退役：升到 wgpu 档，
        // 别让用户停在一条设置页不再提供、以后也不会再维护的路径上。
        "gpu" => Some("femtovg-wgpu".to_owned()),
        // 正常路径上 `app::run` 已经把 `auto` 探测并固化成了具体值（见
        // `resolve_auto_renderer_mode`），这里只是兜底：万一还有 `auto` 走到这一步，
        // 照样探测一次，别回到"交给 Slint 自动选择"（那正是打不开窗口的老路）。
        "auto" => {
            if std::env::var_os("SLINT_BACKEND").is_some() {
                tracing::info!("auto renderer: SLINT_BACKEND is set, skipping the GPU probe");
                None
            } else if gpu_renderer_probe_passes() {
                tracing::warn!("auto renderer: probed but not persisted, using wgpu");
                Some("femtovg-wgpu".to_owned())
            } else {
                tracing::warn!("auto renderer: probe failed, using software");
                Some("software".to_owned())
            }
        }
        _ => Some("software".to_owned()),
    };
    // Any explicit environment value wins, including plain "winit" (automatic
    // renderer selection). This keeps the existing diagnostic escape hatch.
    let env_backend = std::env::var("SLINT_BACKEND").ok();
    let renderer = match env_backend.as_deref() {
        Some(backend) => backend
            .strip_prefix("winit-")
            .filter(|renderer| !renderer.is_empty())
            .map(str::to_owned),
        None => configured_renderer,
    };
    if let Some(renderer) = renderer.as_ref() {
        builder = builder.with_renderer_name(renderer.clone());
    }
    tracing::info!(
        renderer_mode,
        renderer = renderer.as_deref().unwrap_or("auto"),
        source = if env_backend.is_some() {
            "SLINT_BACKEND"
        } else {
            "settings"
        },
        "initializing Windows renderer"
    );
    let backend = builder
        .with_window_attributes_hook(|attrs| {
            attrs.with_transparent(false).with_undecorated_shadow(false)
        })
        .build();

    match backend {
        Ok(backend) => {
            if slint::platform::set_platform(Box::new(backend)).is_err() {
                tracing::warn!("Windows winit backend was already initialized");
            }
        }
        Err(err) => tracing::warn!("failed to initialize Windows winit backend: {err}"),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn setup_linux_platform(renderer_mode: &str) {
    if let Some(env_backend) = std::env::var_os("SLINT_BACKEND") {
        tracing::info!(
            renderer_mode,
            renderer = %env_backend.to_string_lossy(),
            source = "SLINT_BACKEND",
            "initializing Linux renderer"
        );
        return;
    }

    // 矩阵：软件 / FemtoVG(wgpu→Vulkan)。`auto` 由启动时的探测
    // （`resolve_auto_renderer_mode`，与 Windows 同一套）固化成这两者之一，
    // 不再交给 Slint 自己的自动选择 —— 那条链会先挑 Skia，不是我们要的矩阵。
    // 旧配置的 `gpu`（OpenGL 版 FemtoVG）与支线早期的 `skia-vulkan` 都退役 → 升到 wgpu 档。
    let renderer = match renderer_mode {
        "software" => "software",
        "wgpu" => "femtovg-wgpu",
        "gpu" => "femtovg-wgpu",
        "skia-vulkan" => "femtovg-wgpu",
        _ => {
            tracing::info!(
                renderer_mode,
                renderer = "auto",
                source = "settings",
                "initializing Linux renderer"
            );
            return;
        }
    };

    tracing::info!(
        renderer_mode,
        renderer,
        source = "settings",
        "initializing Linux renderer"
    );
    let backend_builder =
        i_slint_backend_winit::Backend::builder().with_renderer_name(renderer.to_owned());
    match backend_builder.build() {
        Ok(backend) => {
            if slint::platform::set_platform(Box::new(backend)).is_err() {
                tracing::warn!("Linux winit backend was already initialized");
            }
        }
        Err(err) => tracing::warn!("failed to initialize Linux winit backend: {err}"),
    }
}

pub(super) fn clamp_window_size_to_monitor(
    window: &slint::Window,
    preferred: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    use i_slint_backend_winit::winit::dpi::{LogicalPosition, LogicalSize};

    window.with_winit_window(|ww| {
        #[cfg(target_os = "linux")]
        {
            use i_slint_backend_winit::winit::platform::wayland::WindowExtWayland;

            // Wayland compositors own the final surface size. A
            // request_inner_size call is only advisory and KWin may configure a
            // different size, leaving Slint's rendered and input geometries out
            // of sync (#286). Let the compositor choose the startup size.
            if ww.xdg_toplevel().is_some() {
                return None;
            }
        }

        let scale = ww.scale_factor().max(0.01);
        // Before `Window::run()` makes the native window visible, winit often
        // has no current monitor yet. Falling back to the primary monitor lets
        // the persisted size actually apply during startup (#278).
        let monitor = ww.current_monitor().or_else(|| ww.primary_monitor())?;
        let monitor_size = monitor.size();
        let monitor_pos = monitor.position();
        let max_w = (monitor_size.width as f64 / scale - 16.0).max(1.0) as f32;
        let max_h = (monitor_size.height as f64 / scale - 16.0).max(1.0) as f32;
        let min_w = 960.0_f32.min(max_w);
        let min_h = 600.0_f32.min(max_h);
        let current = ww.inner_size();
        let current_w = (current.width as f64 / scale) as f32;
        let current_h = (current.height as f64 / scale) as f32;
        let (want_w, want_h) = preferred.unwrap_or((current_w, current_h));
        let target_w = want_w.clamp(min_w, max_w);
        let target_h = want_h.clamp(min_h, max_h);

        if (target_w - current_w).abs() > 0.5
            || (target_h - current_h).abs() > 0.5
            || preferred.is_some()
        {
            let _ = ww.request_inner_size(LogicalSize::new(target_w as f64, target_h as f64));
        }

        if (target_w - want_w).abs() > 0.5 || (target_h - want_h).abs() > 0.5 {
            let mon_w = monitor_size.width as f64 / scale;
            let mon_h = monitor_size.height as f64 / scale;
            let mon_x = monitor_pos.x as f64 / scale;
            let mon_y = monitor_pos.y as f64 / scale;
            ww.set_outer_position(LogicalPosition::new(
                mon_x + (mon_w - target_w as f64).max(0.0) / 2.0,
                mon_y + (mon_h - target_h as f64).max(0.0) / 2.0,
            ));
        }

        Some((target_w, target_h))
    })?
}

#[cfg(target_os = "linux")]
pub(super) fn is_wayland_window(window: &slint::Window) -> bool {
    use i_slint_backend_winit::winit::platform::wayland::WindowExtWayland;

    window
        .with_winit_window(|ww| ww.xdg_toplevel().is_some())
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn is_wayland_window(_window: &slint::Window) -> bool {
    false
}

pub(super) fn maximized_geometry_needs_repair(
    window_width: u32,
    window_height: u32,
    monitor_width: u32,
    monitor_height: u32,
) -> bool {
    window_width.saturating_mul(4) < monitor_width.saturating_mul(3)
        || window_height.saturating_mul(4) < monitor_height.saturating_mul(3)
}

pub(super) fn refresh_revealed_main_window(weak: slint::Weak<AppWindow>) {
    let Some(win) = weak.upgrade() else { return };
    let repair = win
        .window()
        .with_winit_window(|ww| {
            ww.request_redraw();
            if !cfg!(windows) || !ww.is_maximized() {
                return false;
            }
            let Some(monitor) = ww.current_monitor() else {
                return false;
            };
            let outer = ww.outer_size();
            let screen = monitor.size();
            let stale = maximized_geometry_needs_repair(
                outer.width,
                outer.height,
                screen.width,
                screen.height,
            );
            if stale {
                tracing::warn!(
                    "repairing stale maximized geometry: window={}x{} monitor={}x{} scale={}",
                    outer.width,
                    outer.height,
                    screen.width,
                    screen.height,
                    ww.scale_factor(),
                );
                ww.set_maximized(false);
            }
            stale
        })
        .unwrap_or(false);

    let weak2 = weak.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
        if let Some(win) = weak2.upgrade() {
            win.window().with_winit_window(|ww| {
                if repair {
                    ww.set_maximized(true);
                }
                ww.request_redraw();
            });
        }
    });
}

#[cfg(target_os = "linux")]
pub(super) fn schedule_slint_pointer_ungrab<T>(weak: slint::Weak<T>)
where
    T: slint::ComponentHandle + 'static,
{
    // Linux window managers/compositors may consume the release event after a
    // system move/resize starts. If Slint keeps its press grab, the whole app
    // can remain stuck in move/resize cursor mode. A few deferred synthetic
    // releases cover Cinnamon/Mutter/KWin timing differences.
    for delay_ms in [0_u64, 16, 80, 200] {
        let weak2 = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(delay_ms), move || {
            if let Some(w) = weak2.upgrade() {
                let win = w.window();
                win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(-1.0, -1.0),
                    button: slint::platform::PointerEventButton::Left,
                });
                win.dispatch_event(slint::platform::WindowEvent::PointerExited);
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn schedule_slint_pointer_ungrab<T>(_weak: slint::Weak<T>)
where
    T: slint::ComponentHandle + 'static,
{
}

#[cfg(target_os = "macos")]
pub(super) fn setup_macos_platform(renderer_mode: &str) {
    use i_slint_backend_winit::winit::platform::macos::WindowAttributesExtMacOS;

    let mut builder = i_slint_backend_winit::Backend::builder();
    // An explicit environment value wins, including plain "winit" (Slint's
    // automatic choice). Otherwise use the renderer selected in Settings.
    let env_backend = std::env::var("SLINT_BACKEND").ok();
    let renderer = match env_backend.as_deref() {
        Some(backend) => backend
            .strip_prefix("winit-")
            .filter(|renderer| !renderer.is_empty())
            .map(str::to_owned),
        // 矩阵：软件 / FemtoVG(wgpu→Metal，默认) / Skia(wgpu→Metal)。
        // 旧配置里的 `femtovg`（OpenGL）已在 `ConfigStore::renderer_mode` 里归一化到
        // `femtovg-wgpu`；这里的兜底只防配置被手改坏。
        None => Some(
            match renderer_mode {
                "skia" => "skia",
                "software" => "software",
                _ => "femtovg-wgpu",
            }
            .to_owned(),
        ),
    };
    if let Some(renderer) = renderer.as_ref() {
        builder = builder.with_renderer_name(renderer.clone());
    }
    tracing::info!(
        renderer_mode,
        renderer = renderer.as_deref().unwrap_or("auto"),
        source = if env_backend.is_some() {
            "SLINT_BACKEND"
        } else {
            "settings"
        },
        "initializing macOS renderer"
    );
    builder = builder.with_window_attributes_hook(|attrs| {
        attrs
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
    });
    match builder.build() {
        Ok(backend) => {
            if slint::platform::set_platform(Box::new(backend)).is_err() {
                tracing::warn!("winit backend already set; immersive macOS titlebar disabled");
            }
        }
        Err(e) => {
            tracing::warn!("winit backend build failed ({e}); immersive macOS titlebar disabled")
        }
    }
}

#[cfg(test)]
mod mixed_dpi_window_tests {
    use super::maximized_geometry_needs_repair;

    #[test]
    fn repairs_large_maximized_geometry_mismatch() {
        assert!(maximized_geometry_needs_repair(604, 1384, 1080, 1501));
        assert!(maximized_geometry_needs_repair(1920, 1000, 3840, 2160));
    }

    #[test]
    fn accepts_taskbar_sized_maximized_work_area() {
        assert!(!maximized_geometry_needs_repair(1920, 1040, 1920, 1080));
        assert!(!maximized_geometry_needs_repair(2560, 1400, 2560, 1440));
    }
}

#[cfg(test)]
mod auto_renderer_tests {
    use super::auto_renderer_for;

    /// 探测通过 → 配置写 `wgpu`；不通过 → 写 `software`（这正是用户选的"自动"语义：
    /// 探测一次、结论落进配置文件，之后启动不再探测）。
    #[test]
    fn probe_result_maps_to_a_concrete_config_value() {
        assert_eq!(auto_renderer_for(true), "wgpu");
        assert_eq!(auto_renderer_for(false), "software");
    }
}

#[cfg(test)]
mod titlebar_drag_tests {
    /// 标题栏拖动必须**先上膛、拖过阈值才交给系统**（`armed-drag` + 6px）。
    ///
    /// 为什么盯着这条：按下即 drag 会让系统接管指针，第一次点击的 up 不再派发，
    /// `double-clicked` 永远派发不到 —— 双击最大化就没了。反过来让双击区压在上面，
    /// 拖动区又收不到按下（fix1/fix2 丢拖动，fix3/fix4 丢双击，两种顺序各丢一个功能）。
    /// Slint 1.18 的 `WindowMoveArea` 走的是"抓走指针"那条路，所以这里用它替代不了。
    #[test]
    fn titlebar_drag_is_armed_before_handing_over_to_the_system() {
        let src = include_str!("../../ui/components/window_shell.slint");
        assert!(
            src.contains("armed-drag"),
            "标题栏拖动必须保留 armed-drag（否则双击最大化失效）"
        );
        assert!(
            src.contains("> 6px"),
            "拖动必须在位移超过 6px 后才交给系统（阈值被删就意味着按下即拖）"
        );
        // 注释里会提到 `WindowMoveArea`（解释为什么不用它），所以只认**元素声明**。
        assert!(
            !src.contains("WindowMoveArea {"),
            "WindowMoveArea 会抓走指针、顶掉双击最大化，别在标题栏里用它"
        );
    }
}
