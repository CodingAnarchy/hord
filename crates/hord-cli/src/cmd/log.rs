//! `hord log [--node <NodeId|name>] [--path <p>] [--actor <a>] [--since <t>]`
//!
//! Landed changes, oldest first, through the session's backend
//! (`RepoBackend::log`, paged). Filters are combined: a change is kept only
//! when every flag matches. `--node` uses the store's touch rule (write
//! set, ops, and identity deltas; not the read set); a name is resolved at
//! head. `--path` keeps a change that wrote a node under that path.

use anyhow::Result;
use hord_api::proto;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on};

pub fn run(
    json: bool,
    target: &Target,
    node: Option<String>,
    path: Option<String>,
    actor: Option<String>,
    since: Option<u64>,
) -> Result<()> {
    let session = Session::open(target)?;
    let backend = session.backend();
    let node = match node {
        Some(spec) => Some(txn::backend_resolve_node(backend.as_ref(), &spec)?.to_string()),
        None => None,
    };
    let mut query = proto::LogQuery {
        actor,
        node,
        path,
        since_ms: since,
        limit: 1_000,
        after: None,
    };
    let mut changes = Vec::new();
    loop {
        let page = block_on(backend.log(query.clone()))?;
        changes.extend(page.items);
        match page.next {
            Some(next) => query.after = Some(next),
            None => break,
        }
    }
    changes.reverse();
    let head = block_on(backend.head(proto::HeadRequest {}))?.change;
    let result = proto::LogResult { head, changes };
    if json {
        output::print_json(&result)?;
    } else if result.changes.is_empty() {
        println!("(empty log)");
    } else {
        for change in &result.changes {
            println!("{}", change.change);
        }
    }
    Ok(())
}
