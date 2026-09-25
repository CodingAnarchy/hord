//! Impact sets (spec §6.5): `write_set ∪ transitive References-dependents`,
//! bounded by hops and the package (crate) boundary.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hord_core::{NodeId, RepoPath};
use serde::{Deserialize, Serialize};

use crate::{ChangeFacts, Result};

/// The reference edges of one snapshot, read backwards.
///
/// `hord-txn` implements it over a snapshot's resolved `References` edges
/// (spec §3.8); tests and benches implement it over their own maps.
pub trait ReferenceGraph {
    /// Definitions whose bodies name `node` (`References(d, node)`), in
    /// any order. Over-approximation is safe; missing a dependent is not.
    fn dependents(&self, node: NodeId) -> Result<Vec<NodeId>>;

    /// Package (crate) that contains `node`, if known. The impact set does
    /// not cross from one known package into another when
    /// [`ImpactBound::package_boundary`] is set.
    fn package(&self, node: NodeId) -> Option<String>;

    /// File that contains `node`, if known. Selection uses it to confine a
    /// non-function edit in a test module to that module (ADR 0022, R2); an
    /// unknown path keeps the package fallback.
    fn path(&self, _node: NodeId) -> Option<RepoPath> {
        None
    }
}

/// How far the impact set follows dependents (spec §6.5; policy may
/// change it). The default is 2 hops and the crate boundary, whichever
/// stops first.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ImpactBound {
    /// Follow at most this many dependent hops from the write set; `None`
    /// is unbounded.
    pub max_hops: Option<u32>,
    /// Do not add a dependent in another known package than the node it
    /// depends on.
    pub package_boundary: bool,
}

impl Default for ImpactBound {
    fn default() -> Self {
        Self {
            max_hops: Some(2),
            package_boundary: true,
        }
    }
}

/// What a change can affect, for planning (spec §6.5).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImpactSet {
    /// The change's write set.
    pub write_set: BTreeSet<NodeId>,
    /// `write_set` plus its bounded dependents, each with its hop count
    /// (0 for the write set).
    pub nodes: BTreeMap<NodeId, u32>,
    /// What the change touched, for selection fallbacks. Empty when the
    /// caller has no file-level facts; a verifier then selects
    /// conservatively.
    pub facts: ChangeFacts,
    /// The file of each impacted node the graph knows
    /// ([`ReferenceGraph::path`]).
    #[serde(default)]
    pub paths: BTreeMap<NodeId, RepoPath>,
}

impl ImpactSet {
    /// The impacted nodes, without hop counts.
    #[must_use]
    pub fn node_set(&self) -> BTreeSet<NodeId> {
        self.nodes.keys().copied().collect()
    }

    /// Number of impacted nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether nothing is impacted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// The impact set ADR 0022 selects tests with (amendments after the
/// 50-commit measurement): the write set, plus bounded `References`
/// dependents of only the writes coverage cannot attribute. A function the
/// change wrote is looked up in coverage directly (a test that never ran it
/// cannot observe it), so its callers are not added; types, fields, consts,
/// statics, trait items, declarations, and anything else that is not a
/// `function_item` in `facts` still expand to their dependents.
pub fn impact_set_attributable(
    graph: &dyn ReferenceGraph,
    write_set: &BTreeSet<NodeId>,
    bound: ImpactBound,
    facts: ChangeFacts,
) -> Result<ImpactSet> {
    let functions: BTreeSet<NodeId> = facts
        .touched
        .iter()
        .filter(|t| t.kind.as_str() == "function_item")
        .map(|t| t.node)
        .collect();
    let seeds: BTreeSet<NodeId> = write_set.difference(&functions).copied().collect();
    let mut set = impact_set(graph, &seeds, bound, facts)?;
    for w in write_set {
        set.nodes.insert(*w, 0);
    }
    set.write_set = write_set.clone();
    Ok(set)
}

/// Compute the impact set of `write_set` in `graph` under `bound`, and
/// attach `facts`.
///
/// Breadth-first, so each node gets its shortest hop count; cycles are
/// fine. The write set is always included, whatever the bound.
pub fn impact_set(
    graph: &dyn ReferenceGraph,
    write_set: &BTreeSet<NodeId>,
    bound: ImpactBound,
    facts: ChangeFacts,
) -> Result<ImpactSet> {
    let mut nodes: BTreeMap<NodeId, u32> = write_set.iter().map(|n| (*n, 0)).collect();
    let mut queue: VecDeque<NodeId> = write_set.iter().copied().collect();
    while let Some(node) = queue.pop_front() {
        let hops = nodes[&node];
        if bound.max_hops.is_some_and(|max| hops >= max) {
            continue;
        }
        let package = bound
            .package_boundary
            .then(|| graph.package(node))
            .flatten();
        let mut next = graph.dependents(node)?;
        next.sort();
        next.dedup();
        for dependent in next {
            if nodes.contains_key(&dependent) {
                continue;
            }
            if let Some(package) = &package
                && graph
                    .package(dependent)
                    .is_some_and(|other| &other != package)
            {
                continue;
            }
            nodes.insert(dependent, hops + 1);
            queue.push_back(dependent);
        }
    }
    let paths = nodes
        .keys()
        .filter_map(|n| Some((*n, graph.path(*n)?)))
        .collect();
    Ok(ImpactSet {
        write_set: write_set.clone(),
        nodes,
        facts,
        paths,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;

    /// `edges[a]` = nodes that reference `a`.
    struct Graph {
        dependents: HashMap<u128, Vec<u128>>,
        packages: HashMap<u128, &'static str>,
    }

    impl ReferenceGraph for Graph {
        fn dependents(&self, node: NodeId) -> Result<Vec<NodeId>> {
            Ok(self
                .dependents
                .get(&node.as_u128())
                .map(|v| v.iter().copied().map(NodeId::from_u128).collect())
                .unwrap_or_default())
        }

        fn package(&self, node: NodeId) -> Option<String> {
            self.packages.get(&node.as_u128()).map(|p| (*p).to_owned())
        }
    }

    fn ids(v: &[u128]) -> BTreeSet<NodeId> {
        v.iter().copied().map(NodeId::from_u128).collect()
    }

    fn chain() -> Graph {
        // 1 <- 2 <- 3 <- 4, and 5 (another package) references 1; 2 <-> 6 cycle.
        Graph {
            dependents: HashMap::from([
                (1, vec![2, 5]),
                (2, vec![3, 6]),
                (3, vec![4]),
                (6, vec![2]),
            ]),
            packages: HashMap::from([(1, "a"), (2, "a"), (3, "a"), (4, "a"), (5, "b"), (6, "a")]),
        }
    }

    #[test]
    fn default_bound_is_two_hops_inside_the_package() {
        let set = impact_set(
            &chain(),
            &ids(&[1]),
            ImpactBound::default(),
            ChangeFacts::default(),
        )
        .expect("impact set");
        assert_eq!(set.node_set(), ids(&[1, 2, 3, 6]));
        assert_eq!(set.nodes[&NodeId::from_u128(3)], 2);
        assert_eq!(set.write_set, ids(&[1]));
    }

    #[test]
    fn the_impact_set_records_the_files_the_graph_knows() {
        struct WithPaths(Graph);
        impl ReferenceGraph for WithPaths {
            fn dependents(&self, node: NodeId) -> Result<Vec<NodeId>> {
                self.0.dependents(node)
            }
            fn package(&self, node: NodeId) -> Option<String> {
                self.0.package(node)
            }
            fn path(&self, node: NodeId) -> Option<RepoPath> {
                (node.as_u128() != 3)
                    .then(|| RepoPath::new(vec![format!("f{}.rs", node.as_u128())]))
            }
        }
        let set = impact_set(
            &WithPaths(chain()),
            &ids(&[1]),
            ImpactBound::default(),
            ChangeFacts::default(),
        )
        .expect("impact set");
        let known: BTreeSet<NodeId> = set.paths.keys().copied().collect();
        assert_eq!(known, ids(&[1, 2, 6]), "3's file is unknown");
        assert_eq!(set.paths[&NodeId::from_u128(2)].components(), ["f2.rs"]);
        // A graph without paths records none.
        let bare = impact_set(
            &chain(),
            &ids(&[1]),
            ImpactBound::default(),
            ChangeFacts::default(),
        )
        .expect("impact set");
        assert!(bare.paths.is_empty());
    }

    #[test]
    fn bounds_are_configurable() {
        let unbounded = ImpactBound {
            max_hops: None,
            package_boundary: false,
        };
        let set = impact_set(&chain(), &ids(&[1]), unbounded, ChangeFacts::default())
            .expect("impact set");
        assert_eq!(set.node_set(), ids(&[1, 2, 3, 4, 5, 6]));
        let zero = ImpactBound {
            max_hops: Some(0),
            package_boundary: true,
        };
        let set =
            impact_set(&chain(), &ids(&[1, 4]), zero, ChangeFacts::default()).expect("impact set");
        assert_eq!(set.node_set(), ids(&[1, 4]));
    }

    #[test]
    fn attributable_impact_expands_only_non_functions() {
        use hord_core::{NodeKind, RepoPath};

        use crate::{DefDelta, TouchedDef};
        let touched = |node: u128, kind: &str| TouchedDef {
            node: NodeId::from_u128(node),
            path: RepoPath::default(),
            kind: NodeKind::new(kind),
            name: None,
            delta: DefDelta::Edited,
            attributes_changed: false,
            test: false,
            dispatch: false,
        };
        // 1 is a function (its callers 2, 5 are not added); 3 is a struct
        // referenced by 4.
        let graph = Graph {
            dependents: HashMap::from([(1, vec![2, 5]), (3, vec![4])]),
            packages: HashMap::new(),
        };
        let facts = ChangeFacts {
            paths: Default::default(),
            touched: vec![touched(1, "function_item"), touched(3, "struct_item")],
        };
        let set = impact_set_attributable(&graph, &ids(&[1, 3]), ImpactBound::default(), facts)
            .expect("impact set attributable");
        assert_eq!(set.node_set(), ids(&[1, 3, 4]));
        assert_eq!(set.write_set, ids(&[1, 3]));
    }

    proptest! {
        /// The impact set contains the write set, only reachable nodes,
        /// and grows with the hop bound.
        #[test]
        fn impact_is_monotone_in_hops(
            edges in proptest::collection::vec((0u128..12, 0u128..12), 0..40),
            write in proptest::collection::btree_set(0u128..12, 0..4),
            hops in 0u32..4,
        ) {
            let mut dependents: HashMap<u128, Vec<u128>> = HashMap::new();
            for (a, b) in edges {
                dependents.entry(a).or_default().push(b);
            }
            let graph = Graph { dependents, packages: HashMap::new() };
            let write: BTreeSet<NodeId> = write.into_iter().map(NodeId::from_u128).collect();
            let bound = |h| ImpactBound { max_hops: Some(h), package_boundary: true };
            let small = impact_set(&graph, &write, bound(hops), ChangeFacts::default()).expect("impact set");
            let big = impact_set(&graph, &write, bound(hops + 1), ChangeFacts::default()).expect("impact set");
            prop_assert!(write.is_subset(&small.node_set()));
            prop_assert!(small.node_set().is_subset(&big.node_set()));
            prop_assert!(small.nodes.values().all(|h| *h <= hops));
        }
    }
}
