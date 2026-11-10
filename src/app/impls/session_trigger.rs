//! Expect/send login triggers (#212): the session-dialog draft layer.
//!
//! Mirrors `port_forward.rs`: the dialog works on a `TriggerDraft` (plain
//! strings, never the stored secret), and only saving converts the drafts into
//! stored `SessionTrigger` rules.

use super::*;

pub(crate) fn blank_trigger_draft() -> TriggerDraft {
    TriggerDraft {
        expect: "".into(),
        response: "".into(),
        append_enter: true,
        repeat: false,
    }
}

/// Load saved rules into the dialog. The response is deliberately left blank so
/// a stored secret is never echoed back into an editable field (#10).
pub(crate) fn trigger_drafts(triggers: &[crate::config::SessionTrigger]) -> Vec<TriggerDraft> {
    triggers
        .iter()
        .map(|trigger| TriggerDraft {
            expect: trigger.expect.clone().into(),
            response: "".into(),
            append_enter: trigger.append_enter,
            repeat: trigger.repeat,
        })
        .collect()
}

pub(crate) fn trigger_model(triggers: &[TriggerDraft]) -> ModelRc<TriggerDraft> {
    ModelRc::from(Rc::new(VecModel::from(triggers.to_vec())))
}

/// Validate drafts into stored rules. `saved_responses` holds the responses of
/// the session being edited, index-aligned with the drafts: a blank response box
/// means "keep what was saved" rather than "clear it".
pub(crate) fn validated_triggers(
    drafts: &[TriggerDraft],
    saved_responses: &[Secret],
) -> std::result::Result<Vec<crate::config::SessionTrigger>, String> {
    let mut out = Vec::new();
    for (index, draft) in drafts.iter().enumerate() {
        if draft.expect.trim().is_empty() && draft.response.is_empty() {
            continue;
        }
        if draft.expect.trim().is_empty() {
            return Err(t("请输入触发器的期望文本", "Enter the expected trigger text.").to_string());
        }
        let response = if draft.response.is_empty() {
            saved_responses.get(index).cloned().unwrap_or_default()
        } else {
            Secret::new(draft.response.to_string())
        };
        if response.is_empty() {
            return Err(t("请输入触发器的回复内容", "Enter the trigger response.").to_string());
        }
        out.push(crate::config::SessionTrigger {
            expect: draft.expect.trim().to_string(),
            response,
            append_enter: draft.append_enter,
            repeat: draft.repeat,
        });
    }
    Ok(out)
}
