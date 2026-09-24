//! 设置窗口（独立 root 窗口）的接线。
//!
//! 为什么需要这个模块（两条都是 Slint 的行为，不是我们的选择）：
//! * `Theme` global **按窗口各自实例化** —— 独立窗口拿不到主窗口那份，必须逐项同步
//!   （`ProcWindow` / `SystemInfoWindow` 也是这么干的，见 `sync_proc_theme`）；
//! * macOS 上新映射的第二个窗口**不会自动产生首帧** —— 表现是"只有标题栏、没有内容，
//!   拉伸一下才出来"，所以 `show()` 之后要按 0 / 50 / 200ms 请求三次重绘。
//!
//! 回调全部**一行转发**给主窗口：那边已经有 49 套验过的实现（持久化 + 应用到主窗口），
//! 这里不复制任何业务逻辑。开窗时把主窗口的当前值整份拷过去（57 项），所以不需要再
//! 从配置重新推导标签 / 模型。

use std::time::Duration;

use i_slint_backend_winit::WinitWindowAccessor as _;
use slint::{ComponentHandle, Timer};

use super::resource_ui::request_first_frame;
use crate::ui::{AppWindow, SettingsWindow, Theme};

thread_local! {
    /// 设置窗口句柄（`wire_settings_window` 里登记一次）。用来支持"外面改了主题顺手回灌"。
    static SETTINGS_WEAK: std::cell::RefCell<Option<slint::Weak<SettingsWindow>>> =
        const { std::cell::RefCell::new(None) };
}

/// 主题在**设置窗口之外**被改动时，顺手把设置窗口也刷一遍。
///
/// 为什么需要：设置窗口是独立窗口，用户完全可能开着它去点主窗口的深浅开关（或等
/// "跟随系统"每 5 秒自动跟随）—— 那条路径不经过设置窗口的回调，不回灌就会看到
/// "主窗口已经变浅、设置窗口还黑着"。调用点都是本来就在同步 `ProcWindow` 主题的地方。
pub(super) fn resync_if_open(main: &AppWindow) {
    SETTINGS_WEAK.with(|s| {
        let weak = s.borrow().clone();
        if let Some(sw) = weak.and_then(|w| w.upgrade()) {
            sync_settings_theme(main, &sw);
        }
    });
}

/// 与主窗口的 `Theme` global 同步（Slint 的 global 是每个窗口一份实例）。
///
/// 10 个值：前 8 个抄 `sync_proc_theme` 的口径（含 `is-mac` —— `proc_window.slint:169`
/// 留着一条血泪注释：子窗口里 `Theme.is-mac` 恒为 false，而设置页的更新频道文案与圆角
/// 尺寸都依赖它）；后 2 个是主题色，不同步的话这个窗口的按钮 / 选中态会退回默认蓝。
pub(super) fn sync_settings_theme(m: &AppWindow, sw: &SettingsWindow) {
    let t = m.global::<Theme>();
    sw.set_dark_mode(t.get_dark());
    sw.set_ui_scale(t.get_ui_scale());
    sw.set_ui_font_family(t.get_ui_font_family());
    sw.set_is_mac(t.get_is_mac());
    sw.set_wallpaper_img(t.get_wallpaper());
    sw.set_wallpaper_active(t.get_wallpaper_active());
    sw.set_wp_accent(t.get_wp_accent());
    sw.set_wp_tint(t.get_wp_tint());
    sw.set_accent_overridden(t.get_accent_overridden());
    sw.set_accent_seed(t.get_accent_seed());
    sw.set_panel_alpha(t.get_panel_alpha());
}

/// 只留一个原生关闭按钮：隐藏 macOS 的「最小化(黄)」与「缩放(绿)」，保留「关闭(红)」。
///
/// 为什么只能这么干：Slint 没暴露这个开关，winit 0.30 也没有 —— `maximizable` 在 winit
/// 里只是 X11 内部按 `resizable` 派生出来的，Windows/macOS 都没有公开 API。所以拿到底层
/// `NSWindow` 直接调 AppKit（`standardWindowButton:` 取按钮、`setHidden:` 藏起来）。
/// 非 macOS 是空实现：那边的标题栏按钮归窗口管理器管，程序改不了。
#[cfg(target_os = "macos")]
fn keep_only_close_button(w: &slint::Window) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `WindowHandle` 借用 winit 窗口，不能从闭包里带出来 → 只取指针（`usize` 是 'static）。
    let view_ptr = w
        .with_winit_window(|ww| {
            let handle = ww.window_handle().ok()?;
            match handle.as_raw() {
                RawWindowHandle::AppKit(a) => Some(a.ns_view.as_ptr() as usize),
                _ => None,
            }
        })
        .flatten();
    let Some(view_ptr) = view_ptr else { return };
    let Some(view_ptr) = std::ptr::NonNull::new(view_ptr as *mut std::ffi::c_void) else {
        return;
    };
    let ns_view = view_ptr.as_ptr() as *mut AnyObject;

    // SAFETY: 指针来自 winit 的 raw window handle，指向本窗口自己的 `NSView`，只在 UI 线程用；
    // `window` / `standardWindowButton:` / `setHidden:` 都是 AppKit 的常规操作，按钮对象由
    // 窗口持有，生命周期覆盖本函数。
    unsafe {
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return;
        }
        // NSWindowButton：0 = close(红) / 1 = miniaturize(黄) / 2 = zoom(绿)。
        for idx in [1usize, 2usize] {
            let button: *mut AnyObject = msg_send![ns_window, standardWindowButton: idx];
            if !button.is_null() {
                let _: () = msg_send![button, setHidden: true];
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn keep_only_close_button(_w: &slint::Window) {}

/// 把「主窗口可能改过、而设置窗口需要显示」的那批值灌回去。
///
/// 为什么需要：设置窗口的属性只是**开窗时拷来的一份副本**（不像旧覆盖层那样是 `<=>` 双向
/// 直连主窗口）。所以会出现"点了推荐色 → 主窗口确实改了 `term-cursor-choice`，但设置窗口
/// 那份没变 → 色块的 `selected` 永远不高亮"，看起来就是"点了没反应、选不中"（用户报的现象）。
/// 每次转发回调之后把这类**回显型**属性灌一遍即可；刻意**不**包含输入框类（webdav-url /
/// username / password 等），免得把用户正在输入、还没提交的内容冲掉。
fn sync_settings_reflected(m: &AppWindow, sw: &SettingsWindow) {
    // 光标颜色（「终端设置」那一栏的推荐色块 / 取色框靠这几个回显）
    sw.set_term_cursor_choice(m.get_term_cursor_choice());
    sw.set_term_cursor_color(m.get_term_cursor_color());
    sw.set_term_cursor_color_hex(m.get_term_cursor_color_hex());
    // 主题色（「配色」分区：预设高亮、当前方案名、自定义色回填）
    sw.set_accent_choice(m.get_accent_choice());
    sw.set_accent_hex(m.get_accent_hex());
    sw.set_accent_name(m.get_accent_name());
    sw.set_accent_presets(m.get_accent_presets());
    sw.set_accent_mode(m.get_accent_mode());
    // 字体 / 壁纸 / 渲染器（其余可能被 Rust 侧规范化或回落的值）
    sw.set_term_font_index(m.get_term_font_index());
    sw.set_ui_font_index(m.get_ui_font_index());
    sw.set_current_wallpaper(m.get_current_wallpaper());
    sw.set_custom_wallpaper_name(m.get_custom_wallpaper_name());
    sw.set_wallpaper_index(m.get_wallpaper_index());
    sw.set_wallpaper_labels(m.get_wallpaper_labels());
    sw.set_renderer_mode(m.get_renderer_mode());
    // 异步状态（上传 / 下载 / 更新检查 / 输出高亮规则校验）
    sw.set_webdav_status(m.get_webdav_status());
    sw.set_update_check_status(m.get_update_check_status());
    sw.set_update_checking(m.get_update_checking());
    sw.set_update_last_check(m.get_update_last_check());
    sw.set_output_highlight_rule_status(m.get_output_highlight_rule_status());
    sw.set_output_highlight_rules(m.get_output_highlight_rules());
    sw.set_wsl_profiles(m.get_wsl_profiles());
}

/// 打开设置窗口：播种主窗口当前值 → 同步主题 → `show()` → 首帧补救三连。
pub(super) fn open_settings_window(m: &AppWindow, sw: &SettingsWindow) {
    // 每次打开都重新播种：设置窗口的属性是**独立副本**（不像旧覆盖层那样双向绑定），
    // 期间主窗口可能被别处改过（例如「还原本页默认」），重播一次就不会滞后。
    // 值全部从主窗口现成属性拷过来，不需要再从配置重新推导标签 / 模型。
    seed_settings_window(m, sw);
    sync_settings_theme(m, sw);
    let _ = sw.show();
    sw.window().with_winit_window(|ww| ww.focus_window());
    // 首帧：0 / 50 / 200ms 各请求一次（首次可能早于窗口映射完成而被 winit 丢弃）。
    request_first_frame(sw.window());
    keep_only_close_button(sw.window());
    let f1 = sw.as_weak();
    Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(w) = f1.upgrade() {
            request_first_frame(w.window());
            // 首帧补救的同时再藏一次：`show()` 当下窗口可能还没映射完，
            // 那一刻取不到 `NSWindow`（取不到就跳过，所以这里补两轮）。
            keep_only_close_button(w.window());
        }
    });
    let f2 = sw.as_weak();
    Timer::single_shot(Duration::from_millis(200), move || {
        if let Some(w) = f2.upgrade() {
            request_first_frame(w.window());
            keep_only_close_button(w.window());
        }
    });
}

/// 关闭设置窗口。窗口**只隐藏不销毁** —— 所以没有 `#323/#343` 那类
/// "在事件派发栈上销毁子树"的风险；下次打开 `show()` 即可。
pub(super) fn close_settings_window(sw: &SettingsWindow) {
    let _ = sw.hide();
}

/// 主窗口关闭 / 退出时，把设置窗口一起收掉（不需要持有句柄，走登记表）。
///
/// 为什么需要：设置窗口是**独立 root 窗口**，它自己就足以让事件循环继续活着 —— 主窗口关掉
/// 之后如果它还开着，用户看到的就是"程序关了、设置还挂在桌面上"。
pub(super) fn close_if_open() {
    SETTINGS_WEAK.with(|s| {
        let weak = s.borrow().clone();
        if let Some(sw) = weak.and_then(|w| w.upgrade()) {
            close_settings_window(&sw);
        }
    });
}

/// 开窗播种：把主窗口的当前值整份拷过来（57 项，全部由脚本生成核对过）。
fn seed_settings_window(m: &AppWindow, sw: &SettingsWindow) {
    sw.set_accent_mode(m.get_accent_mode());
    sw.set_accent_choice(m.get_accent_choice());
    sw.set_accent_presets(m.get_accent_presets());
    sw.set_accent_hex(m.get_accent_hex());
    sw.set_accent_name(m.get_accent_name());
    sw.set_collapse_sftp_default(m.get_collapse_sftp_default());
    sw.set_collapse_sidebar_default(m.get_collapse_sidebar_default());
    sw.set_convert_eol(m.get_convert_eol());
    sw.set_current_wallpaper(m.get_current_wallpaper());
    sw.set_custom_wallpaper_name(m.get_custom_wallpaper_name());
    sw.set_wallpaper_labels(m.get_wallpaper_labels());
    sw.set_wallpaper_index(m.get_wallpaper_index());
    sw.set_download_always_ask(m.get_download_always_ask());
    sw.set_hide_special_partitions(m.get_hide_special_partitions());
    sw.set_is_windows(m.get_is_windows());
    sw.set_json_format_output(m.get_json_format_output());
    sw.set_lang_en(m.get_lang_en());
    sw.set_large_scrollback(m.get_large_scrollback());
    sw.set_mount_filter(m.get_mount_filter());
    sw.set_osc52_clipboard(m.get_osc52_clipboard());
    sw.set_output_highlight_enabled(m.get_output_highlight_enabled());
    sw.set_output_highlight_preset(m.get_output_highlight_preset());
    sw.set_output_highlight_rule_status(m.get_output_highlight_rule_status());
    sw.set_output_highlight_rules(m.get_output_highlight_rules());
    sw.set_quick_commands_as_sidebar(m.get_quick_commands_as_sidebar());
    sw.set_quick_panel_collapsed(m.get_quick_panel_collapsed());
    sw.set_quick_panel_open(m.get_quick_panel_open());
    sw.set_renderer_mode(m.get_renderer_mode());
    sw.set_scrollback_lines(m.get_scrollback_lines());
    sw.set_sftp_follow_cd(m.get_sftp_follow_cd());
    sw.set_show_cmd_bar(m.get_show_cmd_bar());
    sw.set_sync_upload_enabled(m.get_sync_upload_enabled());
    sw.set_term_cursor_color(m.get_term_cursor_color());
    sw.set_term_cursor_choice(m.get_term_cursor_choice());
    sw.set_term_cursor_color_hex(m.get_term_cursor_color_hex());
    sw.set_term_cursor_style(m.get_term_cursor_style());
    sw.set_term_font_index(m.get_term_font_index());
    sw.set_term_fonts(m.get_term_fonts());
    sw.set_ui_font_index(m.get_ui_font_index());
    sw.set_ui_fonts(m.get_ui_fonts());
    sw.set_update_check_enabled(m.get_update_check_enabled());
    sw.set_update_check_status(m.get_update_check_status());
    sw.set_update_checking(m.get_update_checking());
    sw.set_update_channel(m.get_update_channel());
    sw.set_update_freq_labels(m.get_update_freq_labels());
    sw.set_update_freq_index(m.get_update_freq_index());
    sw.set_update_last_check(m.get_update_last_check());
    sw.set_webdav_accept_invalid_certs(m.get_webdav_accept_invalid_certs());
    sw.set_webdav_enabled(m.get_webdav_enabled());
    sw.set_webdav_password(m.get_webdav_password());
    sw.set_webdav_remote_path(m.get_webdav_remote_path());
    sw.set_webdav_status(m.get_webdav_status());
    sw.set_webdav_url(m.get_webdav_url());
    sw.set_webdav_username(m.get_webdav_username());
    sw.set_welcome_as_sidebar(m.get_welcome_as_sidebar());
    sw.set_wsl_new_directory(m.get_wsl_new_directory());
    sw.set_wsl_profiles(m.get_wsl_profiles());
}

/// 49 个回调一行转发给主窗口（`invoke_*` 是 Slint 为回调生成的调用入口）。
///
/// 转发之后一律做两件事：① 回灌 `Theme` / `Palette`（按窗口各自实例，见 `sync_settings_theme`）；
/// ② 回灌**回显型**属性（`sync_settings_reflected`，例如光标色/主题色的选中态与当前方案名）——
/// 否则设置窗口里那些 `selected` / 回显绑定会一直停在开窗时的旧值（用户报的"推荐色点不中"）。
///
/// 有返回值的回调把结果透传；无返回值的直接调用（`let` 绑 unit 会被 clippy 拦）。
pub(super) fn wire_settings_window(m: &slint::Weak<AppWindow>, sw: &SettingsWindow) {
    SETTINGS_WEAK.with(|s| *s.borrow_mut() = Some(sw.as_weak()));
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_add_output_highlight_rule(move |a0, a1, a2, a3, a4| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_add_output_highlight_rule(a0, a1, a2, a3, a4);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_add_wsl_profile(move |a0, a1, a2| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_add_wsl_profile(a0, a1, a2);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_check_update_now(move || {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_check_update_now();
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_persist_wallpaper_overlay(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_persist_wallpaper_overlay(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_preview_wallpaper_overlay(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_preview_wallpaper_overlay(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_pick_wallpaper_file(move || {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_pick_wallpaper_file();
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_pick_wsl_directory(move || {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_pick_wsl_directory();
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_remove_output_highlight_rule(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_remove_output_highlight_rule(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_remove_wsl_profile(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_remove_wsl_profile(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_reset_page(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_reset_page(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_save_webdav_settings(move |a0, a1, a2, a3, a4, a5| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_save_webdav_settings(a0, a1, a2, a3, a4, a5);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_animations_enabled(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_animations_enabled(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_collapse_sftp_default(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_collapse_sftp_default(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_collapse_sidebar_default(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_collapse_sidebar_default(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_convert_eol(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_convert_eol(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_download_always_ask(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_download_always_ask(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_hide_special_partitions(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_hide_special_partitions(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_json_format_output(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_json_format_output(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_large_scrollback(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_large_scrollback(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_mount_filter(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_mount_filter(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_osc52_clipboard(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_osc52_clipboard(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_output_highlight(move |a0, a1| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_output_highlight(a0, a1);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_output_highlight_rule_enabled(move |a0, a1| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_output_highlight_rule_enabled(a0, a1);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_panel_font(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_panel_font(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_quick_commands_as_sidebar(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_quick_commands_as_sidebar(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_renderer_mode(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_renderer_mode(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_scrollback_lines(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_scrollback_lines(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_sftp_follow_cd(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_sftp_follow_cd(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_show_cmd_bar(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_show_cmd_bar(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_sync_upload_enabled(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_sync_upload_enabled(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_term_cursor_color(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_term_cursor_color(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_term_cursor_color_rgb(move |a0, a1, a2| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_term_cursor_color_rgb(a0, a1, a2);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_term_cursor_style(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_term_cursor_style(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_term_font(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_term_font(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_term_font_bold(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_term_font_bold(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_term_font_size(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_term_font_size(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_ui_font(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_ui_font(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_upload_ui_font(move || {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_upload_ui_font();
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_ui_scale(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_ui_scale(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_accent(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_accent(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_accent_rgb(move |a0, a1, a2| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_accent_rgb(a0, a1, a2);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_appearance_mode(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_appearance_mode(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_update_check_enabled(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_update_check_enabled(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_update_channel(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_update_channel(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_update_freq(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_update_freq(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_wallpaper(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_wallpaper(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_set_welcome_as_sidebar(move |a0| {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_set_welcome_as_sidebar(a0);
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_webdav_download(move || {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_webdav_download();
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
    {
        let m = m.clone();
        let s = sw.as_weak();
        sw.on_webdav_upload(move || {
            let (Some(m), Some(sw)) = (m.upgrade(), s.upgrade()) else {
                return Default::default();
            };
            let out = m.invoke_webdav_upload();
            sync_settings_theme(&m, &sw);
            sync_settings_reflected(&m, &sw);
            out
        });
    }
}
