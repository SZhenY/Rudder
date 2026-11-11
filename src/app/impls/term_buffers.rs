use super::*;



/// Max bytes merged into one Output event before starting a fresh chunk (#209).
/// Keeps a single UI callback from spending hundreds of ms in vt100 ingest.
pub(crate) const OUTPUT_MERGE_BYTE_CAP: usize = 64 * 1024;

/// Output parsed between UI-flush checkpoints during sustained traffic.
pub(crate) const INGEST_FRAME_BUDGET: usize = 64 * 1024;


pub(crate) const INTERACTIVE_ECHO_WINDOW: std::time::Duration = std::time::Duration::from_millis(180);

pub(crate) fn term_buf(bufs: &TermBuffers, tab_id: &str) -> Option<TermBufferHandle> {
    bufs.lock().unwrap_or_else(|e| e.into_inner()).get(tab_id).cloned()
}

pub(crate) fn with_term_buf<R>(
    bufs: &TermBuffers,
    tab_id: &str,
    f: impl FnOnce(&mut TermBuffer) -> R,
) -> Option<R> {
    let h = term_buf(bufs, tab_id)?;
    let mut guard = h.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut guard))
}

pub(crate) fn ingest_terminal_output(bufs: &TermBuffers, tab_id: &str, chunk: &[u8]) -> Vec<u8> {
    if let Some(h) = term_buf(bufs, tab_id) {
        h.lock().unwrap_or_else(|e| e.into_inner()).ingest(chunk)
    } else {
        Vec::new()
    }
}

pub(crate) fn record_ingested_chunk(chunk_len: usize, ingested_since_checkpoint: &mut usize) -> bool {
    debug_assert!(*ingested_since_checkpoint < INGEST_FRAME_BUDGET);
    if chunk_len == 0 {
        return false;
    }

    let remaining = INGEST_FRAME_BUDGET - *ingested_since_checkpoint;
    if chunk_len < remaining {
        *ingested_since_checkpoint += chunk_len;
        false
    } else {
        *ingested_since_checkpoint = (chunk_len - remaining) % INGEST_FRAME_BUDGET;
        true
    }
}

pub(crate) fn event_requires_immediate_ui(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Connected
            | SessionEvent::Closed(_)
            | SessionEvent::HostKeyPrompt { .. }
            | SessionEvent::CredentialPrompt { .. }
            | SessionEvent::MfaPrompt { .. }
    )
}

#[cfg(test)]
mod ingest_frame_tests {
    use super::{INGEST_FRAME_BUDGET, event_requires_immediate_ui, record_ingested_chunk};
    use crate::ssh::SessionEvent;

    fn count_requests(chunk_lengths: &[usize]) -> (usize, usize) {
        let mut since_checkpoint = 0usize;
        let mut requests = 0usize;
        let mut dirty_since_request = false;
        for &chunk_len in chunk_lengths {
            dirty_since_request = true;
            if record_ingested_chunk(chunk_len, &mut since_checkpoint) {
                requests += 1;
                dirty_since_request = false;
            }
        }
        if dirty_since_request {
            requests += 1;
        }
        (requests, since_checkpoint)
    }

    #[test]
    fn exact_frame_budget_chunks_do_not_add_an_empty_tail_request() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET, INGEST_FRAME_BUDGET]);
        assert_eq!(requests, 2);
        assert_eq!(remainder, 0);
    }

    #[test]
    fn a_partial_tail_gets_one_final_request() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET, INGEST_FRAME_BUDGET, 1]);
        assert_eq!(requests, 3);
        assert_eq!(remainder, 1);
    }

    #[test]
    fn checkpoint_budget_carries_across_input_events() {
        let mut since_checkpoint = 0usize;
        assert!(!record_ingested_chunk(
            INGEST_FRAME_BUDGET - 1,
            &mut since_checkpoint
        ));
        assert!(record_ingested_chunk(1, &mut since_checkpoint));
        assert_eq!(since_checkpoint, 0);
    }

    #[test]
    fn an_oversized_output_event_stays_one_atomic_checkpoint() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET * 2 + 1]);
        assert_eq!(requests, 1);
        assert_eq!(remainder, 1);
    }

    #[test]
    fn routine_shell_metadata_does_not_disable_tail_pacing() {
        assert!(!event_requires_immediate_ui(&SessionEvent::CommandRan(
            "tail -n 1000000 app.log".into()
        )));
        assert!(!event_requires_immediate_ui(&SessionEvent::CwdChanged(
            "/var/log".into()
        )));
        assert!(event_requires_immediate_ui(&SessionEvent::Connected));
        assert!(event_requires_immediate_ui(&SessionEvent::Closed(
            "connection lost".into()
        )));
    }
}
