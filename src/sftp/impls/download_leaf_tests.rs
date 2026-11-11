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
