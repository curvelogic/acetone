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
