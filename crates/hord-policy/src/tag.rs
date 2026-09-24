//! Evidence tags: `kind` or `kind:qualifier` (spec §7.2).

use std::fmt;
use std::str::FromStr;

use hord_core::{Evidence, EvidenceKind};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An evidence kind with an optional qualifier, as policy names it:
/// `check`, `test:selected`, `test:full`, `review:human`,
/// `review:agent-reviewer`, `bench:no-regression`.
///
/// In a policy it is a requirement; on [`crate::EvidenceFact`] it names what
/// a piece of evidence is. Serialized as its text form.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvidenceTag {
    kind: String,
    qualifier: Option<String>,
}

impl EvidenceTag {
    /// A tag from its parts. Fails like [`FromStr`] on an empty kind or
    /// qualifier, whitespace, or a `:` inside a part.
    pub fn new(kind: &str, qualifier: Option<&str>) -> Result<Self, String> {
        check_part(kind, "kind")?;
        if let Some(q) = qualifier {
            check_part(q, "qualifier")?;
        }
        Ok(Self {
            kind: kind.to_owned(),
            qualifier: qualifier.map(str::to_owned),
        })
    }

    /// The kind, e.g. `test`.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The qualifier, e.g. `selected`, if any.
    #[must_use]
    pub fn qualifier(&self) -> Option<&str> {
        self.qualifier.as_deref()
    }

    /// Whether evidence tagged `self` meets `requirement` (ADR 0026): the
    /// kinds are equal and, if the requirement has a qualifier, so are the
    /// qualifiers. A bare requirement (`check`, `review`) is met by any
    /// qualifier of its kind. `test:full` also meets `test:selected`, since
    /// a full run is a superset; the reverse does not hold.
    #[must_use]
    pub fn satisfies(&self, requirement: &EvidenceTag) -> bool {
        if self.kind != requirement.kind {
            return false;
        }
        let Some(wanted) = requirement.qualifier.as_deref() else {
            return true;
        };
        let have = self.qualifier.as_deref();
        have == Some(wanted)
            || (self.kind == "test" && wanted == "selected" && have == Some("full"))
    }

    /// The tag of a stored [`Evidence`]: its kind in policy spelling
    /// (`check`, `test`, `bench`, `lint`, `review`, or a `Custom` name) and
    /// its [`Evidence::qualifier`]. `None` for [`EvidenceKind::Rebase`],
    /// which records landing, not verification, and for a `Custom` name or
    /// qualifier that is not a valid tag part.
    #[must_use]
    pub fn of(evidence: &Evidence) -> Option<Self> {
        let kind = match &evidence.kind {
            EvidenceKind::Check => "check",
            EvidenceKind::Test => "test",
            EvidenceKind::Bench => "bench",
            EvidenceKind::Lint => "lint",
            EvidenceKind::Review => "review",
            EvidenceKind::Custom(name) => name.as_str(),
            EvidenceKind::Rebase { .. } => return None,
        };
        Self::new(kind, evidence.qualifier.as_deref()).ok()
    }
}

fn check_part(part: &str, what: &str) -> Result<(), String> {
    if part.is_empty() {
        return Err(format!("evidence {what} is empty"));
    }
    if part.contains(':') || part.chars().any(char::is_whitespace) {
        return Err(format!(
            "evidence {what} `{part}` must not contain `:` or whitespace"
        ));
    }
    Ok(())
}

impl FromStr for EvidenceTag {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, qualifier) = match s.split_once(':') {
            Some((kind, qualifier)) => (kind, Some(qualifier)),
            None => (s, None),
        };
        Self::new(kind, qualifier).map_err(|e| format!("`{s}`: {e}"))
    }
}

impl fmt::Display for EvidenceTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.kind)?;
        if let Some(q) = &self.qualifier {
            write!(f, ":{q}")?;
        }
        Ok(())
    }
}

impl Serialize for EvidenceTag {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EvidenceTag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> EvidenceTag {
        s.parse().expect("parse evidence tag literal")
    }

    #[test]
    fn parse_and_display() {
        for s in [
            "check",
            "lint",
            "test:selected",
            "test:full",
            "review:human",
            "review:agent-reviewer",
            "bench:no-regression",
        ] {
            assert_eq!(tag(s).to_string(), s);
        }
        assert_eq!(tag("test:full").kind(), "test");
        assert_eq!(tag("test:full").qualifier(), Some("full"));
        assert_eq!(tag("check").qualifier(), None);
    }

    #[test]
    fn rejects_malformed() {
        for s in ["", ":x", "test:", "a:b:c", "te st", "test: full"] {
            assert!(s.parse::<EvidenceTag>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn satisfaction() {
        assert!(tag("test:selected").satisfies(&tag("test:selected")));
        assert!(tag("test:selected").satisfies(&tag("test")));
        assert!(!tag("test:selected").satisfies(&tag("test:full")));
        assert!(tag("test:full").satisfies(&tag("test:selected")));
        assert!(tag("test:full").satisfies(&tag("test")));
        assert!(!tag("bench:full").satisfies(&tag("bench:selected")));
        assert!(!tag("test").satisfies(&tag("test:full")));
        assert!(tag("review:human").satisfies(&tag("review")));
        assert!(!tag("check").satisfies(&tag("lint")));
    }

    fn evidence(kind: EvidenceKind, qualifier: Option<&str>) -> Evidence {
        Evidence {
            kind,
            qualifier: qualifier.map(str::to_owned),
            snapshot: hord_core::ObjectId::from_canonical(b"s"),
            toolchain: hord_core::ObjectId::from_canonical(b"t"),
            command: String::new(),
            scope: None,
            result: hord_core::EvidenceResult::Pass,
            log: None,
            cost_ms: 0,
            produced_by: hord_core::Actor::Human { id: "ada".into() },
            produced_at: hord_core::Timestamp::from_millis(0),
        }
    }

    #[test]
    fn tag_of_evidence() {
        let of = |kind, q| EvidenceTag::of(&evidence(kind, q)).map(|t| t.to_string());
        assert_eq!(of(EvidenceKind::Check, None).as_deref(), Some("check"));
        assert_eq!(
            of(EvidenceKind::Test, Some("selected")).as_deref(),
            Some("test:selected")
        );
        assert_eq!(
            of(EvidenceKind::Review, Some("agent-reviewer")).as_deref(),
            Some("review:agent-reviewer")
        );
        assert_eq!(
            of(EvidenceKind::Bench, Some("no-regression")).as_deref(),
            Some("bench:no-regression")
        );
        assert_eq!(of(EvidenceKind::Lint, None).as_deref(), Some("lint"));
        assert_eq!(
            of(EvidenceKind::Custom("coverage".into()), None).as_deref(),
            Some("coverage")
        );
        assert_eq!(of(EvidenceKind::Custom("a:b".into()), None), None);
        assert_eq!(of(EvidenceKind::Test, Some("")), None);
        let rebase = EvidenceKind::Rebase {
            submitted: hord_core::ObjectId::from_canonical(b"c"),
        };
        assert_eq!(of(rebase, None), None);
    }

    #[test]
    fn serde_is_text() -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&tag("review:human"))?;
        assert_eq!(json, "\"review:human\"");
        let back: EvidenceTag = serde_json::from_str(&json)?;
        assert_eq!(back, tag("review:human"));
        Ok(())
    }
}
