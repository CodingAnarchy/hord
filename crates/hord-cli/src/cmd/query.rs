//! `hord query <edge> <node>`
//!
//! Prints target [`hord_core::NodeId`]s from [`hord_store::Store::edges`] for
//! the result snapshot of the newest change in the log. The node argument is
//! the source. `references`, `dependents`, and `tests-of` map to
//! [`hord_store::EdgeKind::References`], [`hord_store::EdgeKind::Depends`],
//! and [`hord_store::EdgeKind::Tests`].

use anyhow::{Result, anyhow};
use hord_store::EdgeKind;
use serde::Serialize;

use crate::cli::QueryEdge;
use crate::output;
use crate::repo;
use crate::resolve;

#[derive(Debug, Serialize)]
struct QueryResult {
    snapshot: String,
    edge: &'static str,
    node: String,
    targets: Vec<String>,
}

pub fn run(json: bool, edge: QueryEdge, node: String) -> Result<()> {
    let store = repo::discover()?;
    let node = resolve::parse_node_id(&node).ok_or_else(|| anyhow!("invalid NodeId {node:?}"))?;
    let snapshot = resolve::latest_result_snapshot(&store)?;
    let targets = store.edges(snapshot, node, edge_kind(edge))?;
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
