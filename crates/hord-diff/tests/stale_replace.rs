//! `apply` refuses a `Replace` whose `from` is not the content at its node:
//! a script diffed from one tree must not overwrite another tree's edit of
//! the same definition (quality review finding 1).

mod common;

use hord_diff::{Error, apply, diff};
use hord_lang::IdentifiedTree;

use common::{identify_result, parse_identified, project, rust};

const BASE: &[u8] =
    b"pub fn a() -> Vec<u32> {\n    vec![0; 3]\n}\n\npub fn b() -> u32 {\n    2\n}\n";
/// Edits `a` (only a separator changes).
const OURS_A: &[u8] =
    b"pub fn a() -> Vec<u32> {\n    vec![0, 3]\n}\n\npub fn b() -> u32 {\n    2\n}\n";
/// Edits `b`.
const OURS_B: &[u8] =
    b"pub fn a() -> Vec<u32> {\n    vec![0; 3]\n}\n\npub fn b() -> u32 {\n    20\n}\n";
/// Edits `a` differently.
const THEIRS: &[u8] =
    b"pub fn a() -> Vec<u32> {\n    vec![0; 3].clone()\n}\n\npub fn b() -> u32 {\n    2\n}\n";

fn ours(base: &IdentifiedTree, src: &[u8]) -> IdentifiedTree {
    let (tree, mapping) = identify_result(&rust(), base, src);
    IdentifiedTree::new(tree, mapping.nodes)
}

#[test]
fn replace_over_a_changed_definition_is_stale() -> Result<(), Box<dyn std::error::Error>> {
    let base = parse_identified(&rust(), BASE);
    let (theirs, mapping) = identify_result(&rust(), &base, THEIRS);
    let ops = diff(&common::file(), &base, &theirs, &mapping);
    let ours = ours(&base, OURS_A);
    match apply(&common::file(), &ours, &ops, &theirs) {
        Err(Error::StaleReplace {
            expected, found, ..
        }) => assert_ne!(expected, found),
        other => {
            return Err(format!(
                "expected StaleReplace, got {:?}",
                other.map(|t| project(&rust(), &t.tree))
            )
            .into());
        }
    }
    Ok(())
}

#[test]
fn replace_of_an_untouched_definition_still_applies() {
    let base = parse_identified(&rust(), BASE);
    let (theirs, mapping) = identify_result(&rust(), &base, THEIRS);
    let ops = diff(&common::file(), &base, &theirs, &mapping);
    let ours = ours(&base, OURS_B);
    let applied = apply(&common::file(), &ours, &ops, &theirs).expect("disjoint edit applies");
    let got = project(&rust(), &applied.tree);
    assert_eq!(
        String::from_utf8_lossy(&got),
        "pub fn a() -> Vec<u32> {\n    vec![0; 3].clone()\n}\n\npub fn b() -> u32 {\n    20\n}\n"
    );
}
