use super::*;

#[test]
fn chunk_len_caps_at_chunk_size() {
    assert_eq!(chunk_len(1024 * 1024, 0), CHUNK);
}

#[test]
fn chunk_len_returns_exact_remainder() {
    assert_eq!(chunk_len(100, 0), 100);
    assert_eq!(chunk_len(CHUNK as u64 + 17, CHUNK as u64), 17);
}

#[test]
fn chunk_len_is_zero_at_end() {
    assert_eq!(chunk_len(4096, 4096), 0);
}

#[test]
fn partial_file_is_kept_once_any_byte_exists() {
    assert!(should_keep_partial(1));
    assert!(should_keep_partial(CHUNK as u64));
}

#[test]
fn zero_byte_partial_is_cleaned_up() {
    assert!(!should_keep_partial(0));
}

fn mark(remote: &str, size: u64, mtime: u32) -> PartialMark {
    PartialMark {
        remote: remote.to_string(),
        size,
        mtime,
    }
}

#[test]
fn resumes_only_when_the_marker_matches() {
    let m = mark("/srv/data.bin", 1000, 7);
    assert_eq!(resume_offset(Some(100), 1000, Some(&m), "/srv/data.bin", 7), 100);
}

#[test]
fn a_different_remote_path_starts_over() {
    let m = mark("/srv/other.bin", 1000, 7);
    assert_eq!(resume_offset(Some(100), 1000, Some(&m), "/srv/data.bin", 7), 0);
}

#[test]
fn a_changed_size_starts_over() {
    let m = mark("/srv/data.bin", 999, 7);
    assert_eq!(resume_offset(Some(100), 1000, Some(&m), "/srv/data.bin", 7), 0);
}

#[test]
fn a_changed_mtime_starts_over() {
    let m = mark("/srv/data.bin", 1000, 8);
    assert_eq!(resume_offset(Some(100), 1000, Some(&m), "/srv/data.bin", 7), 0);
}

#[test]
fn a_missing_marker_never_resumes_a_same_named_file() {
    assert_eq!(resume_offset(Some(100), 1000, None, "/srv/data.bin", 7), 0);
}

#[test]
fn empty_or_complete_local_files_start_over() {
    let m = mark("/srv/data.bin", 1000, 7);
    assert_eq!(resume_offset(Some(0), 1000, Some(&m), "/srv/data.bin", 7), 0);
    assert_eq!(resume_offset(Some(1000), 1000, Some(&m), "/srv/data.bin", 7), 0);
    assert_eq!(resume_offset(None, 1000, Some(&m), "/srv/data.bin", 7), 0);
}
