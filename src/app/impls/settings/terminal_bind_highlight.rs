use super::*;

pub(super) fn wire_highlight(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            s.save_logging();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            // 规则清单变了，上一次的"规则已添加/已删除"提示文案必须清掉。
            w.set_output_highlight_rule_status("".into());
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font(move |label: SharedString| {
            // The picker labels entries with their source; store only the
            // bare family name so the config stays portable. Group headers
            // (▍…) are not selectable — ignore them.
            let Some(family) = family_from_label(&label) else {
                return;
            };
            {
                let mut s = store.borrow_mut();
                s.set_font_family(family.to_string());
                s.save_logging();
            }
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_term_font_family(family.into());
                w.global::<Theme>()
                    .set_term_font_cjk(term_font_covers_cjk(family));
                // 选中项必须同步到主窗口：AI 页 1 Hz 轮询会 `resync_if_open` 把
                // 设置窗的 `term-font-index` 从主窗口覆盖回来，不回写就会"点了
                // 一秒内自己弹回原项"，看起来像根本改不动。
                let index = w
                    .get_term_fonts()
                    .iter()
                    .position(|item| item == label)
                    .unwrap_or(0) as i32;
                w.set_term_font_index(index);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight(move |enabled, preset: SharedString| {
            let preset = preset.to_string();
            {
                let mut s = store.borrow_mut();
                s.set_output_highlight_enabled(enabled);
                s.set_output_highlight_preset(preset.clone());
                s.save_logging();
            }
            if let Some(w) = weak.upgrade() {
                apply_output_highlight(&w, &bufs, enabled, &preset);
                // 回写主窗口属性：设置窗每次打开都按它播种，不回写就会在重开时
                // 退回修改前的值（配置其实已经生效）。
                w.set_output_highlight_enabled(enabled);
                w.set_output_highlight_preset(preset.into());
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_json_format_output(move |enabled| {
            {
                let mut s = store.borrow_mut();
                s.set_json_format_output(enabled);
                s.save_logging();
            }
            // Flip live buffers so the change applies without reconnecting.
            for buffer in bufs.lock().unwrap_or_else(|e| e.into_inner()).values() {
                buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .json_format_output = enabled;
            }
            if let Some(w) = weak.upgrade() {
                w.set_json_format_output(enabled);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_size(move |size: i32| {
            // 字号滑条是逐帧触发的 → 走防抖出口（见 `settings::persist`）。
            crate::app::settings::persist(&store, |s| s.set_font_size(size as u32));
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_term_font_size(size as f32);
            }
        });
    }
}
