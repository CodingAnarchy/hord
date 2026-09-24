//! Deterministic seeded faults (ADR 0023): one fault in one function the
//! commit wrote. The body panics, returns `Default::default()`, or has its
//! first comparison or boolean flipped, whichever parses and type-checks,
//! tried in a seeded order.

use std::ops::Range;

use hord_core::Node;
use hord_lang::{LangAdapter, NodeTree};
use hord_lang_rust::RustAdapter;
use serde::{Deserialize, Serialize};

use crate::prepare::FaultTarget;

/// The fault kinds of ADR 0023.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) enum FaultKind {
    /// The body is `panic!(..)`.
    Panic,
    /// The body is `Default::default()`.
    Default,
    /// The first comparison, `&&`/`||`, or boolean literal is flipped.
    Flip,
}

pub(crate) const KINDS: [FaultKind; 3] = [FaultKind::Panic, FaultKind::Default, FaultKind::Flip];

/// SplitMix64: a tiny deterministic generator (the order only has to be
/// reproducible, not random-quality).
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64, key: &str) -> Self {
        // FNV-1a of the key, mixed with the seed.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        Self(h ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15))
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = usize::try_from(self.next() % (i as u64 + 1)).unwrap_or(0);
            items.swap(i, j);
        }
    }
}

/// The attempts for a commit, in order: targets in a seeded order, each
/// with the fault kinds in a seeded order.
pub(crate) fn attempts(
    seed: u64,
    commit: &str,
    targets: &[FaultTarget],
) -> Vec<(usize, FaultKind)> {
    let mut rng = Rng::new(seed, commit);
    let mut order: Vec<usize> = (0..targets.len()).collect();
    rng.shuffle(&mut order);
    let mut out = Vec::new();
    for t in order {
        let mut kinds = KINDS;
        rng.shuffle(&mut kinds);
        out.extend(kinds.into_iter().map(|k| (t, k)));
    }
    out
}

/// Pre-order walk with byte offsets: the first node matching `pred`.
fn find<'t>(
    tree: &'t NodeTree,
    id: hord_core::ObjectId,
    at: usize,
    pred: &dyn Fn(&Node) -> bool,
) -> Option<(Range<usize>, &'t Node)> {
    let node = tree.get(id)?;
    let len = node.raw.as_slice().len();
    if pred(node) {
        return Some((at..at + len, node));
    }
    let mut offset = at;
    for child in &node.children {
        if let Some(hit) = find(tree, *child, offset, pred) {
            return Some(hit);
        }
        offset += tree.get(*child).map_or(0, |c| c.raw.as_slice().len());
    }
    None
}

/// Byte range and id of the body block of the first function in `tree`.
fn body(tree: &NodeTree) -> Option<(Range<usize>, hord_core::ObjectId)> {
    let root = tree.root()?;
    let (range, func) = find(tree, root, 0, &|n| n.kind.as_str() == "function_item")?;
    let mut offset = range.start;
    for child in &func.children {
        let node = tree.get(*child)?;
        let len = node.raw.as_slice().len();
        if node.kind.as_str() == "block" {
            return Some((offset..offset + len, *child));
        }
        offset += len;
    }
    None
}

fn flipped(op: &str) -> Option<&'static str> {
    Some(match op {
        "==" => "!=",
        "!=" => "==",
        "<" => ">=",
        ">=" => "<",
        ">" => "<=",
        "<=" => ">",
        "&&" => "||",
        "||" => "&&",
        "true" => "false",
        "false" => "true",
        _ => return None,
    })
}

/// `def_text` (a function item, trivia included) with `kind` injected, or
/// `None` when the kind does not apply (no body, nothing to flip).
pub(crate) fn inject(kind: FaultKind, def_text: &str) -> Option<String> {
    let tree = RustAdapter.parse(def_text.as_bytes()).ok()?;
    let (body, block) = body(&tree)?;
    let replace = |range: Range<usize>, with: &str| {
        format!(
            "{}{with}{}",
            &def_text[..range.start],
            &def_text[range.end..]
        )
    };
    match kind {
        FaultKind::Panic => Some(replace(body, "{ panic!(\"hord-m4 injected fault\") }")),
        FaultKind::Default => Some(replace(body, "{ ::core::default::Default::default() }")),
        FaultKind::Flip => {
            // A binary operator or boolean literal, not a `<` of generics.
            let is_op = |tree: &NodeTree, id: &hord_core::ObjectId| {
                tree.get(*id)
                    .is_some_and(|c| c.children.is_empty() && flipped(c.kind.as_str()).is_some())
            };
            let (range, expr) = find(&tree, block, body.start, &|n| {
                matches!(n.kind.as_str(), "binary_expression" | "boolean_literal")
                    && n.children.iter().any(|c| is_op(&tree, c))
            })?;
            let mut offset = range.start;
            for child in &expr.children {
                let node = tree.get(*child)?;
                let len = node.raw.as_slice().len();
                if is_op(&tree, child) {
                    // The token's raw text may carry trailing trivia.
                    let op = node.kind.as_str();
                    return Some(replace(offset..offset + op.len(), flipped(op)?));
                }
                offset += len;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const FUNC: &str = "/// Doc.\n#[inline]\npub fn is_small(x: u32) -> bool {\n    x < 10\n}";

    #[test]
    fn each_kind_rewrites_the_body() {
        assert_eq!(
            inject(FaultKind::Panic, FUNC).unwrap(),
            "/// Doc.\n#[inline]\npub fn is_small(x: u32) -> bool { panic!(\"hord-m4 injected fault\") }"
        );
        assert_eq!(
            inject(FaultKind::Default, FUNC).unwrap(),
            "/// Doc.\n#[inline]\npub fn is_small(x: u32) -> bool { ::core::default::Default::default() }"
        );
        assert_eq!(
            inject(FaultKind::Flip, FUNC).unwrap(),
            "/// Doc.\n#[inline]\npub fn is_small(x: u32) -> bool {\n    x >= 10\n}"
        );
        // Signature operators are not flipped; body ones are.
        let generic = "fn f<T: Fn() -> bool>(t: T) -> bool {\n    t() == true\n}";
        assert_eq!(
            inject(FaultKind::Flip, generic).unwrap(),
            "fn f<T: Fn() -> bool>(t: T) -> bool {\n    t() != true\n}"
        );
        assert_eq!(inject(FaultKind::Flip, "fn f() { g(); }"), None);
        assert_eq!(inject(FaultKind::Panic, "fn f();"), None);
    }

    #[test]
    fn attempts_are_deterministic_per_commit() {
        let t = |n: u128| FaultTarget {
            path: "src/lib.rs".into(),
            node: hord_core::NodeId::from_u128(n),
            name: String::new(),
            span: 0..1,
        };
        let targets = vec![t(1), t(2), t(3)];
        let a = attempts(7, "abc", &targets);
        assert_eq!(a, attempts(7, "abc", &targets));
        assert_eq!(a.len(), 9);
        assert_ne!(a, attempts(7, "abd", &targets));
        for i in 0..3 {
            assert_eq!(a.iter().filter(|(t, _)| *t == i).count(), 3);
        }
    }

    /// ADR 0023: every fault kind is caught by a test that calls the
    /// faulted function. Builds a one-function crate, injects the fault,
    /// and runs its test.
    fn caught(kind: FaultKind) {
        static N: AtomicU64 = AtomicU64::new(0);
        let root: PathBuf = std::env::temp_dir().join(format!(
            "hord-m4-fault-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"faulty\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        let test = "\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn small() {\n        assert!(super::is_small(3));\n        assert!(!super::is_small(30));\n    }\n}\n";
        let run = |src: &str| {
            fs::write(root.join("src/lib.rs"), format!("{src}{test}")).unwrap();
            Command::new("cargo")
                .args(["test", "--offline"])
                .current_dir(&root)
                .output()
                .unwrap()
        };
        assert!(run(FUNC).status.success(), "the unfaulted test passes");
        let faulted = inject(kind, FUNC).unwrap();
        let out = run(&faulted);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let _ = fs::remove_dir_all(&root);
        assert!(!out.status.success(), "{kind:?} was not caught");
        assert!(
            stdout.contains("test tests::small ... FAILED"),
            "{kind:?}: {stdout}"
        );
    }

    #[test]
    fn panic_fault_is_caught() {
        caught(FaultKind::Panic);
    }

    #[test]
    fn default_fault_is_caught() {
        caught(FaultKind::Default);
    }

    #[test]
    fn flip_fault_is_caught() {
        caught(FaultKind::Flip);
    }
}
