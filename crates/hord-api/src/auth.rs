//! Scopes and which one each RPC needs (spec §10.5.4).
//!
//! A bearer token carries [`Scope`]s. [`requirement`] maps every RPC of
//! `hord.proto`, by its gRPC path, to what the caller's token must hold; a
//! server that requires auth refuses a call whose path has no entry, and a
//! test fails when an RPC is added without one.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// The gRPC metadata key a token travels in: `authorization: Bearer <t>`.
pub const AUTHORIZATION: &str = "authorization";

/// The token in an `authorization` header value, if it is `Bearer <t>`.
#[must_use]
pub fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// What a token may do (spec §10.5.4).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Scope {
    /// Fetch objects, read the log, queue, queries, and events.
    Read,
    /// Put objects, submit, and attach non-review evidence.
    Propose,
    /// Sign `Evidence { kind: Review }` with this qualifier (ADR 0026):
    /// `review:human`, `review:agent-reviewer`, ….
    Review(String),
    /// Resolve parked changes (spec §6.4).
    Arbitrate,
    /// Mint tokens. Grants nothing else.
    Admin,
}

/// A scope string that is none of the forms [`Scope`] names.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("unknown scope {0:?}: one of read, propose, review:<kind>, arbitrate, admin")]
pub struct ParseScopeError(pub String);

impl FromStr for Scope {
    type Err = ParseScopeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ParseScopeError(s.to_owned());
        match s {
            "read" => Ok(Self::Read),
            "propose" => Ok(Self::Propose),
            "arbitrate" => Ok(Self::Arbitrate),
            "admin" => Ok(Self::Admin),
            _ => {
                let kind = s.strip_prefix("review:").ok_or_else(bad)?;
                let valid = !kind.is_empty()
                    && kind
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                if valid {
                    Ok(Self::Review(kind.to_owned()))
                } else {
                    Err(bad())
                }
            }
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => f.write_str("read"),
            Self::Propose => f.write_str("propose"),
            Self::Review(kind) => write!(f, "review:{kind}"),
            Self::Arbitrate => f.write_str("arbitrate"),
            Self::Admin => f.write_str("admin"),
        }
    }
}

/// What a call's token must hold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Requirement {
    /// Nothing: no token needed (the schema, logging in).
    Public,
    /// Any valid token.
    Authenticated,
    /// This scope.
    Scope(Scope),
    /// `AttachEvidence`: `propose`, or `review:<kind>` for review evidence
    /// of that kind. The route admits a token with either; the service
    /// checks the evidence itself.
    Evidence,
}

impl Requirement {
    /// Whether a token with `scopes` may make the call (for
    /// [`Self::Evidence`], whether it may attach some evidence).
    #[must_use]
    pub fn admits(&self, scopes: &[Scope]) -> bool {
        match self {
            Self::Public | Self::Authenticated => true,
            Self::Scope(scope) => scopes.contains(scope),
            Self::Evidence => scopes
                .iter()
                .any(|s| matches!(s, Scope::Propose | Scope::Review(_))),
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Public => f.write_str("nothing"),
            Self::Authenticated => f.write_str("a token"),
            Self::Scope(scope) => write!(f, "scope {scope}"),
            Self::Evidence => f.write_str("scope propose or review:<kind>"),
        }
    }
}

/// Every RPC of `hord.proto` by gRPC path, and what it needs.
const TABLE: &[(&str, Req)] = &[
    // RepoBackend (spec §10.5.2).
    ("/hord.v1.RepoBackend/GetObjects", Req::Read),
    ("/hord.v1.RepoBackend/PutObjects", Req::Propose),
    ("/hord.v1.RepoBackend/Has", Req::Read),
    ("/hord.v1.RepoBackend/StreamObjects", Req::Read),
    ("/hord.v1.RepoBackend/Head", Req::Read),
    ("/hord.v1.RepoBackend/Log", Req::Read),
    ("/hord.v1.RepoBackend/Refs", Req::Read),
    ("/hord.v1.RepoBackend/Submit", Req::Propose),
    ("/hord.v1.RepoBackend/Queue", Req::Read),
    ("/hord.v1.RepoBackend/Arbitrate", Req::Arbitrate),
    ("/hord.v1.RepoBackend/NodeHistory", Req::Read),
    ("/hord.v1.RepoBackend/Edges", Req::Read),
    ("/hord.v1.RepoBackend/ResolveName", Req::Read),
    ("/hord.v1.RepoBackend/AttachEvidence", Req::Evidence),
    ("/hord.v1.RepoBackend/Events", Req::Read),
    // Workspaces: a repository's daemon only (ADR 0024 amendment).
    ("/hord.v1.Workspaces/WsNew", Req::Propose),
    ("/hord.v1.Workspaces/WsList", Req::Read),
    ("/hord.v1.Workspaces/WsRm", Req::Propose),
    ("/hord.v1.Workspaces/WsGc", Req::Propose),
    ("/hord.v1.Workspaces/Status", Req::Read),
    ("/hord.v1.Workspaces/Propose", Req::Propose),
    ("/hord.v1.Workspaces/PolicyCheck", Req::Read),
    ("/hord.v1.Workspaces/Verify", Req::Propose),
    ("/hord.v1.Workspaces/Shutdown", Req::Admin),
    // Schema.
    ("/hord.v1.Schema/GetSchema", Req::Public),
    // Changes: read-only views for the web UI (ADR 0030).
    ("/hord.v1.Changes/GetChange", Req::Read),
    ("/hord.v1.Changes/ChangeDiff", Req::Read),
    ("/hord.v1.Changes/ListRecordings", Req::Read),
    ("/hord.v1.Changes/GetRecording", Req::Read),
    ("/hord.v1.Changes/NodeLineage", Req::Read),
    ("/hord.v1.Changes/ChangeTrace", Req::Read),
    ("/hord.v1.Changes/ListTree", Req::Read),
    ("/hord.v1.Changes/GetFile", Req::Read),
    ("/hord.v1.Changes/NodeEdges", Req::Read),
    // Auth.
    ("/hord.v1.Auth/Login", Req::Public),
    ("/hord.v1.Auth/MintToken", Req::Admin),
    ("/hord.v1.Auth/WhoAmI", Req::Authenticated),
    ("/hord.v1.Auth/GetKey", Req::Read),
    // Audit: M6's acceptance auditor (`hord audit`).
    ("/hord.v1.Audit/AuditLog", Req::Read),
];

/// [`TABLE`]'s entries, `const`-constructible.
#[derive(Clone, Copy)]
enum Req {
    Public,
    Authenticated,
    Read,
    Propose,
    Arbitrate,
    Admin,
    Evidence,
}

impl From<Req> for Requirement {
    fn from(req: Req) -> Self {
        match req {
            Req::Public => Self::Public,
            Req::Authenticated => Self::Authenticated,
            Req::Read => Self::Scope(Scope::Read),
            Req::Propose => Self::Scope(Scope::Propose),
            Req::Arbitrate => Self::Scope(Scope::Arbitrate),
            Req::Admin => Self::Scope(Scope::Admin),
            Req::Evidence => Self::Evidence,
        }
    }
}

/// What a call to `path` (`/hord.v1.<Service>/<Method>`, without a
/// `/r/<name>` prefix) needs; `None` for a path that is no RPC of
/// `hord.proto`. `GET /schema.json` is [`Requirement::Public`].
#[must_use]
pub fn requirement(path: &str) -> Option<Requirement> {
    if path == "/schema.json" {
        return Some(Requirement::Public);
    }
    TABLE
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, req)| Requirement::from(*req))
}

#[cfg(test)]
mod tests {
    use prost::Message;
    use prost_types::FileDescriptorSet;

    use super::*;

    /// Fails when an RPC is added to `hord.proto` without a scope.
    #[test]
    fn every_rpc_has_a_requirement() -> Result<(), Box<dyn std::error::Error>> {
        let set = FileDescriptorSet::decode(crate::schema::descriptor_set())?;
        let mut paths = Vec::new();
        for file in &set.file {
            let package = file.package();
            for service in &file.service {
                for method in &service.method {
                    paths.push(format!("/{package}.{}/{}", service.name(), method.name()));
                }
            }
        }
        assert!(paths.len() > 20, "{paths:?}");
        let missing: Vec<&String> = paths.iter().filter(|p| requirement(p).is_none()).collect();
        assert!(missing.is_empty(), "RPCs without a scope: {missing:?}");
        // And no stale entries.
        let stale: Vec<&str> = TABLE
            .iter()
            .map(|(p, _)| *p)
            .filter(|p| !paths.iter().any(|q| q == p))
            .collect();
        assert!(stale.is_empty(), "scopes for no RPC: {stale:?}");
        assert_eq!(requirement("/hord.v1.RepoBackend/Nope"), None);
        Ok(())
    }

    #[test]
    fn scopes_round_trip_and_admit() -> Result<(), ParseScopeError> {
        for text in [
            "read",
            "propose",
            "review:human",
            "review:agent-reviewer",
            "arbitrate",
            "admin",
        ] {
            assert_eq!(text.parse::<Scope>()?.to_string(), text);
        }
        for bad in ["", "review:", "review", "review:a b", "write"] {
            assert!(bad.parse::<Scope>().is_err(), "{bad}");
        }
        let reviewer = [Scope::Read, Scope::Review("human".into())];
        assert!(Requirement::Evidence.admits(&reviewer));
        assert!(!Requirement::Scope(Scope::Propose).admits(&reviewer));
        assert!(!Requirement::Evidence.admits(&[Scope::Read]));
        // Admin grants nothing else.
        assert!(!Requirement::Scope(Scope::Read).admits(&[Scope::Admin]));
        Ok(())
    }

    #[test]
    fn bearer_values_parse() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer "), None);
    }
}
