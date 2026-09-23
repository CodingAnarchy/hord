//! `hord log [--node <NodeId|name>] [--path <p>] [--actor <a>] [--since <t>]`
//!
//! With no filters this prints the landed change ids, in landing order.
//! Filters are combined: a change is kept only when every flag matches.
//! `--node` uses the same touch rule as [`hord_store::Store::node_history`]
//! (write set, ops, and identity deltas; not the read set). `--path` keeps a
//! change when a write-set node is located at that path in the result
//! identity map, or in the base map when the result map does not place it.

use anyhow::{Context, Result};
use hord_core::RepoPath;
use serde::Serialize;

use crate::output;
use crate::repo;
use crate::resolve::{self, IdentityCache};

#[derive(Debug, Serialize)]
struct LogResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    log: Vec<String>,
}

pub fn run(
    json: bool,
    node: Option<String>,
    path: Option<String>,
    actor: Option<String>,
    since: Option<u64>,
) -> Result<()> {
    let store = repo::discover()?;
    let ids = store.log()?;
    let filtered = if node.is_none() && path.is_none() && actor.is_none() && since.is_none() {
        ids.iter().map(ToString::to_string).collect()
    } else {
        filter_log(&store, &ids, node, path, actor, since)?
    };
    let result = LogResult {
        head: store.head()?.map(|id| id.to_hex()),
        log: filtered,
    };
    if json {
        output::print_json(&result)?;
    } else if result.log.is_empty() {
        println!("(empty log)");
    } else {
        for id in &result.log {
            println!("{id}");
        }
    }
    Ok(())
}

fn filter_log(
    store: &hord_store::Store,
    ids: &[hord_core::ChangeId],
    node: Option<String>,
    path: Option<String>,
    actor: Option<String>,
    since: Option<u64>,
) -> Result<Vec<String>> {
    let node = match node {
        Some(spec) => Some(resolve::resolve_node_spec(store, &spec)?),
        None => None,
    };
    let path = match path {
        Some(spec) => Some(
            spec.parse::<RepoPath>()
                .with_context(|| format!("invalid repository path {spec:?}"))?,
        ),
        None => None,
    };
    let mut cache = IdentityCache::new();
    let mut kept = Vec::new();
    for id in ids {
        let Some(change) = repo::try_change(store, *id)? else {
            continue;
        };
        if let Some(actor) = &actor
            && resolve::actor_id(&change.provenance.actor) != actor
        {
            continue;
        }
        if let Some(since) = since
            && change.provenance.created_at.as_millis() < since
        {
            continue;
        }
        if let Some(node) = node
            && !resolve::touches_node(&change, node)
        {
            continue;
        }
        if let Some(path) = &path
            && !resolve::write_set_touches_path(store, &change, path, &mut cache)?
        {
            continue;
        }
        kept.push(id.to_string());
    }
    Ok(kept)
}
