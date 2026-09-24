//! Blob-tier 3-way merge via diffy (spec §5.2 rule 5).

use hord_diff::{ConflictKind, merge_blob};

#[test]
fn blob_three_way_clean_merge() -> Result<(), Box<dyn std::error::Error>> {
    let base = b"\
alpha
beta
gamma
";
    let ours = b"\
alpha
beta
gamma
delta
";
    let theirs = b"\
omega
alpha
beta
gamma
";
    let merged = merge_blob(base, ours, theirs).expect("clean blob merge");
    let text = String::from_utf8(merged)?;
    assert!(text.contains("omega"));
    assert!(text.contains("delta"));
    assert!(text.contains("beta"));
    Ok(())
}

#[test]
fn blob_overlapping_edit_is_hard_conflict() {
    let base = b"line\n";
    let ours = b"ours\n";
    let theirs = b"theirs\n";
    let err = merge_blob(base, ours, theirs).expect_err("conflict");
    assert_eq!(err.kind, ConflictKind::Hard);
    assert!(err.nodes.is_empty());
}

#[test]
fn blob_binary_is_hard_conflict() {
    let base = &[0xff, 0xfe, 0x00];
    let ours = &[0xff, 0xfe, 0x01];
    let theirs = &[0xff, 0xfe, 0x02];
    let err = merge_blob(base, ours, theirs).expect_err("binary");
    assert_eq!(err.kind, ConflictKind::Hard);
    assert!(err.reason.contains("binary"));
}
