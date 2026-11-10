use crate::app::port_forward::{blank_forward_draft, validated_port_forwards};

#[test]
fn blank_rows_are_ignored_when_saving() {
    assert!(
        validated_port_forwards(&[blank_forward_draft()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn filled_rows_are_saved_without_an_add_step() {
    let mut local = blank_forward_draft();
    local.bind_port = "8080".into();
    local.host = "service.internal".into();
    local.host_port = "80".into();

    let mut dynamic = blank_forward_draft();
    dynamic.kind = "dynamic".into();
    dynamic.bind_port = "1080".into();

    let forwards = validated_port_forwards(&[local, dynamic]).unwrap();
    assert_eq!(forwards.len(), 2);
    assert_eq!(forwards[0].bind_port, 8080);
    assert_eq!(forwards[0].host, "service.internal");
    assert_eq!(forwards[1].kind, "dynamic");
    assert_eq!(forwards[1].host_port, 0);
}

#[test]
fn partially_filled_rows_block_saving() {
    let mut draft = blank_forward_draft();
    draft.bind_port = "8080".into();
    assert!(validated_port_forwards(&[draft]).is_err());
}
