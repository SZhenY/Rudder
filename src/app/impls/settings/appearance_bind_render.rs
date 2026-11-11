use super::*;

pub(super) fn wire_render(window: &AppWindow, store: &Store) {
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_set_animations_enabled(move |v| {
            persist(&store, |s| {
                s.set_animations_enabled(v);
            });
            // 动画开关读的是 `AnimationSettings` 全局，而全局是**每个窗口一份**。
            // 不推主窗口那份，主界面的动效要重启才变。
            if let Some(w) = weak.upgrade() {
                w.global::<AnimationSettings>().set_enabled(v);
            }
        });
    }

    {
        // Renderer selection is consumed before the first native window exists,
        // so persist it now and apply it on the next launch (#280).
        let store = store.clone();
        let weak = window.as_weak();
        window.on_set_renderer_mode(move |mode: SharedString| {
            // 设置页给的是三档语义值（auto / wgpu / software），归一化后再落盘 ——
            // 直接把后端名写进配置会让各平台的 `renderer_mode()` 认不出来。
            persist(&store, |s| {
                s.set_renderer_mode_choice(mode.as_str());
            });

            // 必须回写窗口属性：下拉框的高亮绑的是 `renderer-mode`，只落盘不回写的话
            // 它永远停在旧值 —— 用户点哪一档都不动。
            let choice = match mode.as_str() {
                "software" => "software",
                "wgpu" => "wgpu",
                _ => "auto",
            };
            if let Some(w) = weak.upgrade() {
                w.set_renderer_mode(choice.into());
            }
        });
    }

    {
        let store = store.clone();
        // ⚠ 除了写盘，**必须立刻应用到主窗口的 `Theme.panel-alpha`** —— 面板底色的 `frost()`
        // 就是拿它算 alpha 的。少了这一步，只有设置窗口自己会变（它的 `Theme` 是独立副本），
        // 主界面纹丝不动 —— 上一版就是这个毛病（用户报"只能改设置界面的透明度"）。
        let weak = window.as_weak();
        window.on_persist_wallpaper_overlay(move |v| {
            persist(&store, |s| {
                s.set_wallpaper_overlay(v);
            });
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_panel_alpha(v);
            }
        });
    }

    {
        // 拖动中的实时预览：只改主窗口的 `Theme.panel-alpha`，**不写盘**（写盘走 persist）。
        // 拆成两条回调是因为 Slider 的 `released` 在拖动时不一定触发（点按才触发），
        // 而写盘又不能每帧做。
        let weak = window.as_weak();
        window.on_preview_wallpaper_overlay(move |v| {
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_panel_alpha(v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_hide_special_partitions(move |v: bool| {
            persist(&store, |s| {
                s.set_hide_special_partitions(v);
            });
            // 主窗口属性是设置窗的播种源，不回写会在重开时退回旧值。
            if let Some(w) = weak.upgrade() {
                w.set_hide_special_partitions(v);
            }
        });
    }
}
