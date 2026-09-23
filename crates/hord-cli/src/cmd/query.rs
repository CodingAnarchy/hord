//! `hord query <edge> <node>`
//!
//! Prints target [`hord_core::NodeId`]s from [`hord_txn::Query::edges`] for
//! `head`'s result snapshot. The node argument is the source.
//! `references`, `dependents`, and `tests-of` map to
//! [`hord_store::EdgeKind::References`] (resolved over the snapshot, plus
//! recorded edges), [`hord_store::EdgeKind::Depends`], and
//! [`hord_store::EdgeKind::Tests`] (recorded edges).

use anyhow::{Result, anyhow};
use hord_store::EdgeKind;
use serde::Serialize;

use crate::cli::QueryEdge;
use crate::output;
use crate::resolve;
use crate::txn::{self, block_on};

#[derive(Debug, Serialize)]
struct QueryResult {
    snapshot: String,
    edge: &'static str,
    node: String,
    targets: Vec<String>,
}

pub fn run(json: bool, edge: QueryEdge, node: String) -> Result<()> {
    let repo = txn::open()?;
    let node = resolve::parse_node_id(&node).ok_or_else(|| anyhow!("invalid NodeId {node:?}"))?;
    let head = block_on(repo.head())?;
    if head.change.is_none() {
        return Err(anyhow!("cannot resolve a snapshot: nothing has landed"));
    }
    let snapshot = head.snapshot;
    let query = repo.query();
    let targets = block_on(query.edges(snapshot, node, edge_kind(edge)))?;
    let result = QueryResult {
        snapshot: snapshot.to_hex(),
        edge: edge_name(edge),
        node: node.to_string(),
        targets: targets.iter().map(ToString::to_string).collect(),
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

fn edge_kind(edge: QueryEdge) -> EdgeKind {
    match edge {
        QueryEdge::References => EdgeKind::References,
        QueryEdge::Dependents => EdgeKind::Depends,
        QueryEdge::TestsOf => EdgeKind::Tests,
    }
}

fn edge_name(edge: QueryEdge) -> &'static str {
    match edge {
        QueryEdge::References => "references",
        QueryEdge::Dependents => "dependents",
        QueryEdge::TestsOf => "tests-of",
    }
}
