use super::*;

pub(super) fn blank_trigger_draft() -> TriggerDraft {
    TriggerDraft {
        expect: "".into(),
        response: "".into(),
        append_enter: true,
        repeat: false,
    }
}

pub(super) fn trigger_model(triggers: &[TriggerDraft]) -> ModelRc<TriggerDraft> {
    ModelRc::from(Rc::new(VecModel::from(triggers.to_vec())))
}

pub(super) fn trigger_drafts(triggers: &[crate::config::SessionTrigger]) -> Vec<TriggerDraft> {
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

pub(super) fn validated_triggers(
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

