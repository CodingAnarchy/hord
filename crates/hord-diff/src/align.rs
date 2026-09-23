//! Longest-common-subsequence spans shared by text and CST merge.
//!
//! Both walk the gaps between matched items. One implementation keeps the
//! tie-break (`dp[i - 1][j] >= dp[i][j - 1]` moves in the base) the same.

/// Unmatched range between two successive LCS hits.
///
/// `a0..a1` is the base side. `b0..b1` is the other side. Either side may be
/// empty (pure insert or pure delete).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    pub a0: usize,
    pub a1: usize,
    pub b0: usize,
    pub b1: usize,
}

/// Gaps between equal elements of `a` and `b`, in order.
pub(crate) fn spans<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Span> {
    let pairs = lcs(a, b);
    let mut out = Vec::new();
    let mut ai = 0usize;
    let mut bi = 0usize;
    for &(aj, bj) in &pairs {
        if ai < aj || bi < bj {
            out.push(Span {
                a0: ai,
                a1: aj,
                b0: bi,
                b1: bj,
            });
        }
        ai = aj + 1;
        bi = bj + 1;
    }
    if ai < a.len() || bi < b.len() {
        out.push(Span {
            a0: ai,
            a1: a.len(),
            b0: bi,
            b1: b.len(),
        });
    }
    out
}

fn lcs<T: PartialEq>(a: &[T], b: &[T]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in 0..n {
        for j in 0..m {
            dp[idx(i + 1, j + 1)] = if a[i] == b[j] {
                dp[idx(i, j)] + 1
            } else {
                dp[idx(i + 1, j)].max(dp[idx(i, j + 1)])
            };
        }
    }
    let mut pairs = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[idx(i - 1, j)] >= dp[idx(i, j - 1)] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pairs.reverse();
    pairs
}
