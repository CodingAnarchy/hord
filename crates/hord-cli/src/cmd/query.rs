//! `hord query <edge> <node>`
//!
//! Target NodeIds of one edge kind leaving `node` in head's result
//! snapshot, through the session's backend (`RepoBackend::edges`).
//! `references`, `dependents`, and `tests-of` are the References, Depends,
//! and Tests edges (spec §3.8).

use anyhow::{Result, anyhow};
use hord_api::proto;

use crate::cli::QueryEdge;
use crate::output;
use crate::resolve;
use crate::session::{Session, Target};
use crate::txn::{self, block_on};

pub fn run(json: bool, target: &Target, edge: QueryEdge, node: String) -> Result<()> {
    let node = resolve::parse_node_id(&node).ok_or_else(|| anyhow!("invalid NodeId {node:?}"))?;
    let session = Session::open(target)?;
    let backend = session.backend();
    let Some((_, snapshot)) = txn::backend_head(backend.as_ref())? else {
        return Err(anyhow!("cannot resolve a snapshot: nothing has landed"));
    };
    let reply = block_on(backend.edges(proto::EdgesRequest {
        snapshot: txn::hex(snapshot),
        node: node.to_string(),
        kind: edge_kind(edge).into(),
    }))?;
    let result = proto::QueryResult {
        snapshot: txn::hex(snapshot),
        edge: edge_name(edge).into(),
        node: node.to_string(),
        targets: reply.nodes,
    };
    if json {
        output::print_json(&result)?;
    } else if result.targets.is_empty() {
        println!("(none)");
    } else {
        for id in &result.targets {
            println!("{id}");
        }
    }
    Ok(())
}

fn edge_kind(edge: QueryEdge) -> proto::EdgeKind {
    match edge {
        QueryEdge::References => proto::EdgeKind::References,
        QueryEdge::Dependents => proto::EdgeKind::Depends,
        QueryEdge::TestsOf => proto::EdgeKind::Tests,
    }
}

fn edge_name(edge: QueryEdge) -> &'static str {
    match edge {
        QueryEdge::References => "references",
        QueryEdge::Dependents => "dependents",
        QueryEdge::TestsOf => "tests-of",
    }
}
