//! Chunk-set enumeration for commit anchoring.
//!
//! Git cannot parse chunks, so a commit must anchor the **complete
//! transitive chunk set** of every map it references
//! (`acetone_store::NewCommit::anchors`); anything unanchored is pruned by
//! `git gc` and silently absent from clones. These walks enumerate that
//! set for a root.
//!
//! # Cost
//!
//! Only internal nodes are read — leaf addresses come from their parents —
//! so a walk costs one chunk read per *internal* node not already in the
//! visited set, and no reads at all for shared subtrees. Callers
//! assembling anchors for several roots (a manifest's maps, or successive
//! commits) should reuse one accumulator across
//! [`collect_reachable_chunks`] calls: every chunk already collected
//! prunes its whole subtree from later walks.

use std::collections::BTreeSet;

use acetone_store::{Bytes, ChunkStore, Hash};

use crate::Root;
use crate::error::ProllyError;
use crate::node::{Node, read_node};

/// Estimate how many entries a tree holds, by sampling each level
/// (acetone-2ck.2).
///
/// An exact count is a full walk, and a stored count would be an on-disk
/// format change — interior nodes carry `(last_key, child_hash)` and nothing
/// else. So this walks down the tree estimating the **mean fanout at each
/// level** from a handful of nodes on that level, and multiplies: the node
/// count at each level is the product of the mean fanouts above it, and the
/// entry count is the leaf count times the mean leaf occupancy.
///
/// Sampling per level, rather than following one path, is what makes it
/// robust. A single path multiplies *that path's* fanout at every level as
/// though the whole level looked like it — so a tree with a fat region has
/// its high local fanout extrapolated over the whole level. Measured, that
/// over-estimated by **8.4x** on a tree whose middle third carried a large
/// property, and since the caller spends a *fraction* of the estimate, an
/// over-estimate scales the seek it authorises linearly: that one ran 12.5x
/// slower than the scan it replaced (PR #224 review blocker 1). Sampling
/// [`LEVEL_SAMPLES`] nodes spread across each level instead brings the worst
/// observed error to well under 2x, in both directions.
///
/// Costs `LEVEL_SAMPLES * height` chunk reads at most: two dozen or so on a
/// real tree, which is height 2–4, and bounded above by
/// `LEVEL_SAMPLES * MAX_HEIGHT` on any tree the format admits. Callers
/// should still sample once per query rather than once per row.
///
/// It is a planner input, never a correctness input: a wrong estimate can
/// only make the planner choose a slower plan, never a wrong answer.
pub fn estimate_entries<S: ChunkStore>(store: &S, root: &Root) -> Result<usize, ProllyError> {
    /// Nodes to read per level. More reads buy a better mean; the whole
    /// walk is a fixed cost per query, so a dozen or two is cheap.
    const LEVEL_SAMPLES: usize = 8;

    let mut level = root.top_level();
    // (hash, parent's last-key claim) of the nodes sampled at `level`, and
    // how many nodes that level is estimated to hold in total.
    let mut frontier: Vec<(Hash, Option<Bytes>)> = vec![(root.hash(), None)];
    let mut level_width: f64 = 1.0;

    while level > 0 {
        let mut fanout_total = 0usize;
        let mut sampled = 0usize;
        let mut children: Vec<(Hash, Option<Bytes>)> = Vec::new();
        for (hash, claim) in &frontier {
            let Node::Inner(refs) = read_node(store, hash, level, claim.as_deref(), None)? else {
                unreachable!("level > 0 is checked by read_node")
            };
            if refs.is_empty() {
                return Ok(0);
            }
            fanout_total += refs.len();
            sampled += 1;
            // Spread the descent across each sampled node's children, so
            // the next level's sample is spread across the whole tree
            // rather than clustered under one parent.
            for i in spread(refs.len(), LEVEL_SAMPLES.div_ceil(frontier.len())) {
                children.push((refs[i].hash, Some(refs[i].last_key.clone())));
            }
        }
        level_width *= fanout_total as f64 / sampled.max(1) as f64;
        children.truncate(LEVEL_SAMPLES);
        frontier = children;
        level -= 1;
    }

    // `frontier` is now leaves, and `level_width` their estimated count.
    let mut entries_total = 0usize;
    let mut sampled = 0usize;
    for (hash, claim) in &frontier {
        let Node::Leaf(entries) = read_node(store, hash, 0, claim.as_deref(), None)? else {
            unreachable!("level 0 is a leaf")
        };
        entries_total += entries.len();
        sampled += 1;
    }
    let mean_leaf = entries_total as f64 / sampled.max(1) as f64;
    finite_estimate(level_width, mean_leaf)
}

/// The product of the sampled per-level fanouts and the mean leaf
/// occupancy, refused when it is not finite: a tree whose sampled shape
/// multiplies past `f64` range is not a plausible map, and the saturating
/// `as usize` cast would otherwise hand callers a `usize::MAX` "estimate"
/// that makes any budget derived from it vacuous — a crafted store could
/// use it to keep an unselective seek from ever declining
/// (acetone-2ck.20). Callers already treat an error as "cannot sample";
/// for planning that means scan, which is always a correct answer.
fn finite_estimate(level_width: f64, mean_leaf: f64) -> Result<usize, ProllyError> {
    let estimate = level_width * mean_leaf;
    if !estimate.is_finite() {
        return Err(ProllyError::Corrupt {
            context: "entry estimate",
            reason: format!(
                "sampled fanout product {level_width:e} x mean leaf occupancy {mean_leaf:e} \
                 is not finite"
            ),
        });
    }
    Ok(estimate as usize)
}

/// Up to `want` indices spread evenly across `len`, avoiding the extremes
/// when there is a choice — a node's first and last children are boundary
/// artefacts of chunking, not representative of it.
fn spread(len: usize, want: usize) -> Vec<usize> {
    let want = want.max(1);
    if len <= want {
        return (0..len).collect();
    }
    (1..=want).map(|i| i * len / (want + 1)).collect()
}

/// Add every chunk reachable from `root` (the root chunk, all internal
/// nodes, all leaves) to `out`. Chunks already present in `out` — from a
/// previous walk or a shared subtree — are skipped without being read.
pub fn collect_reachable_chunks<S: ChunkStore>(
    store: &S,
    root: &Root,
    out: &mut BTreeSet<Hash>,
) -> Result<(), ProllyError> {
    if !out.insert(root.hash) {
        return Ok(());
    }
    // (hash, level, parent's last-key claim) frontier of internal nodes
    // still to read. The root has no parent claim.
    let mut frontier: Vec<(Hash, u8, Option<Bytes>)> = Vec::new();
    if root.top_level() > 0 {
        frontier.push((root.hash, root.top_level(), None));
    }
    while let Some((hash, level, claim)) = frontier.pop() {
        let node = read_node(store, &hash, level, claim.as_deref(), None)?;
        let Node::Inner(refs) = node else {
            unreachable!("level > 0 checked by read_node")
        };
        for r in refs {
            if out.insert(r.hash) && level > 1 {
                frontier.push((r.hash, level - 1, Some(r.last_key)));
            }
        }
    }
    Ok(())
}

/// The complete transitive chunk set of one root, sorted and deduplicated
/// — directly usable as `NewCommit::anchors`.
pub fn reachable_chunks<S: ChunkStore>(store: &S, root: &Root) -> Result<Vec<Hash>, ProllyError> {
    let mut set = BTreeSet::new();
    collect_reachable_chunks(store, root, &mut set)?;
    Ok(set.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::finite_estimate;

    #[test]
    fn plausible_estimates_pass_through() {
        assert_eq!(finite_estimate(0.0, 0.0).expect("zero"), 0);
        assert_eq!(finite_estimate(1000.0, 8.0).expect("small"), 8000);
        // Large but finite still saturates on cast rather than erroring —
        // the guard is against non-finite shape, not against big maps.
        assert!(finite_estimate(1e18, 8.0).is_ok());
    }

    #[test]
    fn a_non_finite_product_is_refused_not_saturated() {
        // 64 levels of ~6e4 sampled fanout overflow f64 (~1e307): the
        // product arrives here infinite and must refuse, because
        // `f64::INFINITY as usize` saturates to `usize::MAX` and a budget
        // divided from that never declines.
        assert!((f64::INFINITY * 8.0).is_infinite());
        let err = finite_estimate(f64::INFINITY, 8.0).expect_err("refused");
        assert!(err.to_string().contains("not finite"), "{err}");
        assert!(finite_estimate(f64::MAX, f64::MAX).is_err());
    }
}
