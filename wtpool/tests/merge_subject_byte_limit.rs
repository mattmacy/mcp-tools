//! Regression tests for the 72-byte (UTF-8) cap on merge_message_subject.
//!
//! Wave-1 lost 3/3 PROCEED packets on 2026-05-03 because the orchestrator's
//! Python char-count truncation produced subjects whose UTF-8 byte length
//! exceeded the gate. This file pins the byte semantics at the Rust layer.

use wtpool::merge::{validate_request, MergeRequest};

fn make_request(subject: &str, body: &str) -> MergeRequest {
    MergeRequest {
        branch: "test-branch-merge-to-main-byte-truncation-fix-2026-05-04".to_string(),
        reviewer_voices: vec!["torvalds".to_string()],
        merge_message_subject: subject.to_string(),
        merge_message_body: body.to_string(),
        auto_resolve_cumulative_md: false,
        dry_run: true,
    }
}

#[test]
fn rejects_73_byte_ascii_subject() {
    // BYPASS_SUBJECT_BRANCH_CHECK avoids the substring guardrail; we test
    // the byte-cap branch in isolation.
    std::env::set_var("BYPASS_SUBJECT_BRANCH_CHECK", "1");
    let s = "x".repeat(73);
    let req = make_request(&s, "body");
    let result = validate_request(&req);
    assert!(result.is_err(), "expected Err for 73-byte subject");
    let msg = result.unwrap_err();
    assert!(msg.contains("73 bytes"), "expected '73 bytes' in error: {msg}");
    assert!(msg.contains("UTF-8"), "expected 'UTF-8' in error: {msg}");
}

#[test]
fn accepts_72_byte_subject_with_branch_substring() {
    // 72-byte subject that contains the branch substring satisfies BOTH
    // the byte cap AND the branch-substring guardrail.
    let branch = "test-branch-merge-to-main-byte-truncation-fix-2026-05-04";
    let subject = format!("Merge {}", branch);
    assert_eq!(subject.len(), 62);
    let pad_len = 72 - subject.len();
    let subject_72 = format!("{}{}", subject, "x".repeat(pad_len));
    assert_eq!(subject_72.len(), 72);
    let req = make_request(&subject_72, "body");
    let result = validate_request(&req);
    assert!(result.is_ok(), "expected Ok for 72-byte subject; got: {:?}", result.err());
}

#[test]
fn rejects_71_char_but_73_byte_unicode_subject() {
    // 70 ASCII + 1 em-dash (3 bytes UTF-8) = 73 bytes, 71 chars. This is the
    // exact regression class from Wave-1: char-count says ok, byte-count says no.
    std::env::set_var("BYPASS_SUBJECT_BRANCH_CHECK", "1");
    let s = format!("{}{}", "x".repeat(70), '\u{2014}');
    assert_eq!(s.chars().count(), 71);
    assert_eq!(s.len(), 73);
    let req = make_request(&s, "body");
    let result = validate_request(&req);
    assert!(result.is_err(), "expected Err for 73-byte unicode subject");
    let msg = result.unwrap_err();
    assert!(msg.contains("73 bytes"), "expected '73 bytes' in error: {msg}");
}
