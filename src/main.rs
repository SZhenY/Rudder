// Entry point. Wires the Slint UI to the config store, system sampler and
// SSH session manager.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod fonts;
mod i18n;
mod layout;
mod logging;
mod resource;
mod session;
mod sftp;
mod ssh;
mod terminal;
mod tunnel;
mod ui;
mod wallpaper;
mod webdav;

/// Install a panic hook that records crashes into `error.log` (beside the
/// config) and stderr. With `panic = "abort"` in release, a background-task
/// panic would otherwise silently kill the GUI with no trace (#panic-hook).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic".to_string()
        };
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_default();
        let msg = format!("panic: {payload}\n  at {location}");
        eprintln!("{msg}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::config::log_dir().join("error.log"))
        {
            use std::io::Write;
            let _ = writeln!(f, "{msg}");
        }
    }));
}

fn main() -> anyhow::Result<()> {
    install_panic_hook();

    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("rudder {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // 渲染探测进程：用指定渲染器起一个屏幕外的小窗口、渲染一帧就退出，退出码即结论。
    // 由 `auto` 渲染模式在 Windows 上启动（见 `src/app/window.rs`）；`gpu` 这条取值不会
    // 再次探测，所以不存在递归。
    if let Some(mode) = std::env::args()
        .find_map(|arg| arg.strip_prefix("--probe-renderer=").map(str::to_owned))
    {
        return app::run_renderer_probe(&mode);
    }

    // Renderer 不在这里强制：三平台矩阵是「GPU / 软件」两档，取值与「自动」探测见
    // src/app/window.rs（macOS 默认 GPU = FemtoVG on wgpu → Metal；Windows / Linux 首次
    // 启动探测一次后把结论固化进配置）。上面 `--probe-renderer=` 那个分支是唯一的例外。
    //
    // History: 0.4.10 force-set SLINT_BACKEND=winit-skia to work around femtovg's
    // CoreText font lookup failing on macOS 26 / Tahoe (all text vanished, #108).
    // That fix shipped without on-device verification and turned out to *break* a
    // different set of Macs (Apple Silicon M5 / 26.5): Skia couldn't resolve the
    // "PingFang SC" UI font and all text vanished there instead (#129). Icons
    // survived in both cases because Material Icons is an embedded font.
    //
    // Skia 渲染器后来整体下架（体积与构建成本换不来它带来的收益，且它在 Windows 上有历史
    // 链接坑 #224），于是「两个渲染器各救一半机器」那个取舍不复存在：GPU 档只有
    // femtovg-wgpu。某台机器上渲染异常时的逃生口是设置里切「软件」，或
    // `SLINT_BACKEND=winit-software ./rudder` 启动（环境变量优先于配置里的 renderer_mode）。

    init_tracing();

    // ── IME policy ───────────────────────────────────────────────────────────
    // NOTE: We deliberately DO **NOT** call `ImmDisableIME` here.
    //
    // An earlier version disabled the IME for the whole Slint event-loop thread
    // to work around a vim `:q!` glitch (Chinese IMEs intercept letter keys and,
    // on a Shift press, discard the in-flight pinyin).  But disabling the IME
    // also makes 中文输入 completely impossible — there is no composition window
    // at all, which is exactly the "无法输入任何中文" bug.
    //
    // Chinese input now flows through the hidden `ime-input` TextInput in
    // terminal_view.slint: composition happens there, and committed text is
    // forwarded to the PTY via the `edited` callback.  The vim/Shift side-effects
    // are handled instead by the C0-marker + 3-layer Backspace filters in
    // `app::on_send_key`, so we no longer need (and must not use) ImmDisableIME.

    app::run()
}

/// Set up tracing: stderr (honours RUST_LOG, default info) **plus** a capped
/// `error.log` file at WARN and above so users can send diagnostics — e.g. a
/// bastion disconnect reason — without setting RUST_LOG (#86).
fn init_tracing() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    // Third-party noise routed through `log` → tracing: ICU4X data-error warnings
    // (icu_provider dependency) and fontdb's "malformed font" warning for fonts it
    // can't parse but harmlessly skips (e.g. Windows' mstmc.ttf). Silence on every
    // layer; keep fontdb at `error` so genuine failures still surface.
    fn quiet_noise(mut f: EnvFilter) -> EnvFilter {
        for d in [
            "icu_provider=off",
            "icu_segmenter=off",
            "icu_normalizer=off",
            "fontdb=error",
        ] {
            if let Ok(dir) = d.parse() {
                f = f.add_directive(dir);
            }
        }
        f
    }

    let env_filter =
        quiet_noise(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(env_filter);

    // One file, capped at 50 MiB, auto-overwriting when full (5 MiB was too
    // small to diagnose anything useful).
    let file_layer = logging::path()
        .and_then(|p| logging::CappedFile::open(p, 50 * 1024 * 1024).ok())
        .map(|cf| {
            fmt::layer()
                .with_ansi(false)
                .with_writer(logging::CappedWriter::new(cf))
                .with_filter(quiet_noise(EnvFilter::new("warn")))
        });

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();
}
