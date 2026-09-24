//! Intent files (spec §10.3): Markdown with a YAML front matter block.
//!
//! ```text
//! ---
//! summary: One line.
//! refs:
//!   - https://example.com/spec#6      # URL (anything with ://)
//!   - PROJ-12                         # issue id
//!   - issue: 12                       # or tagged: issue | url | change | git
//! acceptance:
//!   - test: store::round_trip         # test | check | invariant
//!   - check: cargo clippy --all-targets
//!   - The queue never loses an entry  # plain text is an invariant
//! reads:                              # ADR 0012, optional
//!   - crate::store::Store::open       # qualified name
//!   - path: src/lib.rs                # name | path | node
//!   - node: 01ARZ3NDEKTSV4RRFFQ69G5FAV
//! ---
//! Body: the full task description.
//! ```
//!
//! The front matter is parsed by `serde_yaml_ng`; only the `---` fences are
//! located here.

use anyhow::{Context, Result, anyhow, bail};
use hord_core::{Acceptance, Intent, IntentRef, ObjectId};
use hord_txn::ReadDeclaration;
use serde::Deserialize;
use serde_yaml_ng::Value;

use crate::resolve;

/// A parsed intent file.
pub struct IntentFile {
    pub intent: Intent,
    pub reads: Vec<ReadDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    summary: String,
    #[serde(default)]
    refs: Vec<Value>,
    #[serde(default)]
    acceptance: Vec<Value>,
    #[serde(default)]
    reads: Vec<Value>,
}

pub fn parse(text: &str) -> Result<IntentFile> {
    let (yaml, body) = split(text)?;
    let front: FrontMatter =
        serde_yaml_ng::from_str(yaml).context("intent front matter is not valid")?;
    let summary = front.summary.trim().to_owned();
    if summary.is_empty() || summary.contains('\n') {
        bail!("intent summary must be one non-empty line");
    }
    let refs = front
        .refs
        .iter()
        .map(intent_ref)
        .collect::<Result<Vec<_>>>()?;
    let acceptance = front
        .acceptance
        .iter()
        .map(acceptance)
        .collect::<Result<Vec<_>>>()?;
    let reads = front.reads.iter().map(read).collect::<Result<Vec<_>>>()?;
    Ok(IntentFile {
        intent: Intent {
            summary,
            body: body.trim().to_owned(),
            refs,
            acceptance,
        },
        reads,
    })
}

/// Split `---\n<yaml>\n---\n<body>`. The closing fence may also be `...`.
fn split(text: &str) -> Result<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("intent file must start with a `---` front matter block"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let fence = line.trim_end_matches(['\r', '\n']);
        if fence == "---" || fence == "..." {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("intent front matter has no closing `---`")
}

/// A scalar as text (`issue: 12` is a number in YAML).
fn scalar(value: &Value, what: &str) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("{what} must be a scalar"),
    }
}

/// A one-key mapping `{ tag: value }`.
fn tagged<'a>(value: &'a Value, what: &str) -> Result<(String, &'a Value)> {
    let Value::Mapping(map) = value else {
        bail!("{what} must be text or a one-key mapping");
    };
    let mut entries = map.iter();
    match (entries.next(), entries.next()) {
        (Some((key, value)), None) => Ok((scalar(key, what)?, value)),
        _ => bail!("{what} must be a one-key mapping"),
    }
}

fn intent_ref(value: &Value) -> Result<IntentRef> {
    if let Value::String(text) = value {
        return Ok(if text.contains("://") {
            IntentRef::Url { url: text.clone() }
        } else {
            IntentRef::Issue { id: text.clone() }
        });
    }
    let (tag, value) = tagged(value, "a ref")?;
    let text = scalar(value, "a ref")?;
    Ok(match tag.as_str() {
        "issue" => IntentRef::Issue { id: text },
        "url" => IntentRef::Url { url: text },
        "change" => IntentRef::Change {
            change: text
                .parse::<ObjectId>()
                .with_context(|| format!("ref change {text:?} is not a change id"))?,
        },
        "git" => IntentRef::GitCommit { sha: text },
        other => bail!("unknown ref kind {other:?} (issue, url, change, git)"),
    })
}

fn acceptance(value: &Value) -> Result<Acceptance> {
    if let Value::String(text) = value {
        return Ok(Acceptance::Invariant {
            description: text.clone(),
        });
    }
    let (tag, value) = tagged(value, "an acceptance entry")?;
    let text = scalar(value, "an acceptance entry")?;
    Ok(match tag.as_str() {
        "test" => Acceptance::Test { name: text },
        "check" => Acceptance::Check { command: text },
        "invariant" => Acceptance::Invariant { description: text },
        other => bail!("unknown acceptance kind {other:?} (test, check, invariant)"),
    })
}

fn read(value: &Value) -> Result<ReadDeclaration> {
    if let Value::String(text) = value {
        return Ok(ReadDeclaration::Name(text.clone()));
    }
    let (tag, value) = tagged(value, "a read")?;
    let text = scalar(value, "a read")?;
    Ok(match tag.as_str() {
        "name" => ReadDeclaration::Name(text),
        "path" => ReadDeclaration::Path(
            text.parse()
                .map_err(|_| anyhow!("read path {text:?} is not a repository path"))?,
        ),
        "node" => ReadDeclaration::Node(
            resolve::parse_node_id(&text)
                .ok_or_else(|| anyhow!("read node {text:?} is not a NodeId"))?,
        ),
        other => bail!("unknown read kind {other:?} (name, path, node)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_intent_file() -> anyhow::Result<()> {
        let text = "---\nsummary: Make beta return 20\nrefs:\n  - https://example.com/x\n  - PROJ-1\n  - issue: 12\n  - git: abc123\nacceptance:\n  - test: beta_is_20\n  - check: cargo test\n  - never panics\nreads:\n  - gamma\n  - path: src/lib.rs\n  - node: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\nThe body.\n\nMore.\n";
        let file = parse(text)?;
        assert_eq!(file.intent.summary, "Make beta return 20");
        assert_eq!(file.intent.body, "The body.\n\nMore.");
        assert_eq!(
            file.intent.refs,
            vec![
                IntentRef::Url {
                    url: "https://example.com/x".into()
                },
                IntentRef::Issue {
                    id: "PROJ-1".into()
                },
                IntentRef::Issue { id: "12".into() },
                IntentRef::GitCommit {
                    sha: "abc123".into()
                },
            ]
        );
        assert_eq!(file.intent.acceptance.len(), 3);
        assert!(matches!(
            file.intent.acceptance[2],
            Acceptance::Invariant { .. }
        ));
        assert_eq!(file.reads.len(), 3);
        assert_eq!(file.reads[0], ReadDeclaration::Name("gamma".into()));
        Ok(())
    }

    #[test]
    fn rejects_bad_files() {
        assert!(parse("summary: no fences\n").is_err());
        assert!(parse("---\nsummary: open\n").is_err());
        assert!(
            parse("---\nrefs: []\n---\n").is_err(),
            "summary is required"
        );
        assert!(parse("---\nsummary: x\nbogus: 1\n---\n").is_err());
        assert!(parse("---\nsummary: x\nrefs:\n  - weird: 1\n---\n").is_err());
        assert!(parse("---\nsummary: x\n---").is_ok());
    }
}
