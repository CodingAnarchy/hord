//! Rename carrying (spec §3.4 step 4, ADR 0007).
//!
//! Ordered tree-edit distance on the trivia-stripped definition subtree.
//! Leaf labels include token text. Pairs larger than 256 nodes, or whose
//! sizes differ by more than 20%, are not renames.

use hord_core::NodeKind;

use crate::tree::NodeTree;

/// Largest trivia-stripped definition we will compare (ADR 0007).
const MAX_NODES: usize = 256;

#[derive(Clone)]
struct TedNode {
    kind: NodeKind,
    /// Set only for leaves. Internal labels are the [`NodeKind`] alone.
    leaf: Option<Vec<u8>>,
    children: Vec<TedNode>,
}

#[derive(Clone)]
struct Flat {
    kind: Vec<NodeKind>,
    leaf: Vec<Option<Vec<u8>>>,
    /// Postorder index of the leftmost leaf of each node.
    leftmost: Vec<usize>,
}

/// Edits remaining under the 0.8 line (`max - ted`), if the pair qualifies.
pub(crate) fn similarity_slack(
    base: &NodeTree,
    base_id: hord_core::ObjectId,
    result: &NodeTree,
    result_id: hord_core::ObjectId,
) -> Option<usize> {
    let (left, right) = (build(base, base_id)?, build(result, result_id)?);
    let n = count(&left);
    let m = count(&right);
    if n == 0 || m == 0 || n > MAX_NODES || m > MAX_NODES {
        return None;
    }
    let max = n.max(m);
    let min = n.min(m);
    // Within 20%: min >= 0.8 * max.
    if min * 5 < max * 4 {
        return None;
    }
    let ted = tree_distance(&left, &right)?;
    if ted * 5 > max {
        return None;
    }
    Some(max - ted)
}

fn count(node: &TedNode) -> usize {
    1 + node.children.iter().map(count).sum::<usize>()
}

fn build(tree: &NodeTree, id: hord_core::ObjectId) -> Option<TedNode> {
    let node = tree.get(id)?;
    let children: Vec<TedNode> = node
        .children
        .iter()
        .filter_map(|child| build(tree, *child))
        .collect();
    let leaf = if children.is_empty() {
        Some(tree.stripped(id)?.to_vec())
    } else {
        None
    };
    Some(TedNode {
        kind: node.kind,
        leaf,
        children,
    })
}

fn labels_equal(flat_a: &Flat, i: usize, flat_b: &Flat, j: usize) -> bool {
    flat_a.kind[i] == flat_b.kind[j] && flat_a.leaf[i] == flat_b.leaf[j]
}

/// Unit-cost ordered tree edit distance (Zhang-Shasha).
///
/// `None` when the forest tables would exceed the cell budget. Refusing the
/// pair keeps a birth and a death, which cannot false-rename.
fn tree_distance(a: &TedNode, b: &TedNode) -> Option<usize> {
    let flat_a = flatten(a);
    let flat_b = flatten(b);
    let n = flat_a.kind.len();
    let m = flat_b.kind.len();
    let keys_a = keyroots(&flat_a.leftmost);
    let keys_b = keyroots(&flat_b.leftmost);
    let mut cells = 0usize;
    let mut treedist = vec![vec![0usize; m]; n];
    for &i in &keys_a {
        for &j in &keys_b {
            let rows = i - flat_a.leftmost[i] + 2;
            let cols = j - flat_b.leftmost[j] + 2;
            cells = cells.saturating_add(rows.saturating_mul(cols));
            if cells > 2_000_000 {
                return None;
            }
            forest_dist(&flat_a, &flat_b, i, j, &mut treedist);
        }
    }
    Some(treedist[n - 1][m - 1])
}

fn forest_dist(a: &Flat, b: &Flat, i: usize, j: usize, treedist: &mut [Vec<usize>]) {
    let li = a.leftmost[i];
    let lj = b.leftmost[j];
    let rows = i - li + 2;
    let cols = j - lj + 2;
    let mut fd = vec![vec![0usize; cols]; rows];
    for di in 1..rows {
        fd[di][0] = fd[di - 1][0] + 1;
    }
    for dj in 1..cols {
        fd[0][dj] = fd[0][dj - 1] + 1;
    }
    for di in 1..rows {
        for dj in 1..cols {
            let i1 = li + di - 1;
            let j1 = lj + dj - 1;
            if a.leftmost[i1] == li && b.leftmost[j1] == lj {
                let relabel = usize::from(!labels_equal(a, i1, b, j1));
                fd[di][dj] = (fd[di - 1][dj] + 1)
                    .min(fd[di][dj - 1] + 1)
                    .min(fd[di - 1][dj - 1] + relabel);
                treedist[i1][j1] = fd[di][dj];
            } else {
                let p = di - (i1 - a.leftmost[i1] + 1);
                let q = dj - (j1 - b.leftmost[j1] + 1);
                fd[di][dj] = (fd[di - 1][dj] + 1)
                    .min(fd[di][dj - 1] + 1)
                    .min(fd[p][q] + treedist[i1][j1]);
            }
        }
    }
}

fn keyroots(leftmost: &[usize]) -> Vec<usize> {
    let mut last = vec![None; leftmost.len()];
    for (i, &leaf) in leftmost.iter().enumerate() {
        last[leaf] = Some(i);
    }
    let mut keys: Vec<usize> = last.into_iter().flatten().collect();
    keys.sort_unstable();
    keys
}

fn flatten(root: &TedNode) -> Flat {
    let mut kind = Vec::new();
    let mut leaf = Vec::new();
    let mut leftmost = Vec::new();
    postorder(root, &mut kind, &mut leaf, &mut leftmost);
    Flat {
        kind,
        leaf,
        leftmost,
    }
}

fn postorder(
    node: &TedNode,
    kind: &mut Vec<NodeKind>,
    leaf: &mut Vec<Option<Vec<u8>>>,
    leftmost: &mut Vec<usize>,
) -> usize {
    if node.children.is_empty() {
        let index = kind.len();
        kind.push(node.kind);
        leaf.push(node.leaf.clone());
        leftmost.push(index);
        return index;
    }
    let first = postorder(&node.children[0], kind, leaf, leftmost);
    for child in node.children.iter().skip(1) {
        postorder(child, kind, leaf, leftmost);
    }
    kind.push(node.kind);
    leaf.push(None);
    leftmost.push(first);
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(kind: &str, text: &str) -> TedNode {
        TedNode {
            kind: NodeKind::new(kind),
            leaf: Some(text.as_bytes().to_vec()),
            children: Vec::new(),
        }
    }

    fn branch(kind: &str, children: Vec<TedNode>) -> TedNode {
        TedNode {
            kind: NodeKind::new(kind),
            leaf: None,
            children,
        }
    }

    fn brute(a: &[TedNode], b: &[TedNode]) -> usize {
        if a.is_empty() {
            return b.iter().map(count).sum();
        }
        if b.is_empty() {
            return a.iter().map(count).sum();
        }
        let a_last = a.last().expect("nonempty");
        let b_last = b.last().expect("nonempty");
        let mut deleted = a[..a.len() - 1].to_vec();
        deleted.extend_from_slice(&a_last.children);
        let del = brute(&deleted, b) + 1;
        let mut inserted = b[..b.len() - 1].to_vec();
        inserted.extend_from_slice(&b_last.children);
        let ins = brute(a, &inserted) + 1;
        let relabel = usize::from(a_last.kind != b_last.kind || a_last.leaf != b_last.leaf);
        let sub = brute(&a[..a.len() - 1], &b[..b.len() - 1])
            + brute(&a_last.children, &b_last.children)
            + relabel;
        del.min(ins).min(sub)
    }

    #[test]
    fn distance_matches_brute_force_on_small_trees() {
        let trees = [
            leaf("id", "a"),
            leaf("id", "b"),
            branch("fn", vec![leaf("id", "a")]),
            branch("fn", vec![leaf("id", "a"), leaf("id", "b")]),
            branch("fn", vec![leaf("id", "a"), leaf("id", "c")]),
            branch("mod", vec![leaf("id", "a"), leaf("id", "b")]),
            branch(
                "fn",
                vec![branch("block", vec![leaf("id", "a"), leaf("num", "1")])],
            ),
        ];
        for a in &trees {
            for b in &trees {
                let got = tree_distance(a, b).expect("small");
                let want = brute(std::slice::from_ref(a), std::slice::from_ref(b));
                assert_eq!(got, want, "ted mismatch");
            }
        }
    }

    #[test]
    fn ratio_rejects_a_one_node_relabel() {
        let a = leaf("fn", "old");
        let b = leaf("fn", "new");
        let ted = tree_distance(&a, &b).unwrap();
        let max = 1usize;
        assert!(ted * 5 > max);
    }
}
