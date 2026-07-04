//! Map-level three-way merge (spec §3.2/§6, Load-Bearing Invariant 4).
//!
//! `merge(base, ours, theirs)` is a **pure function** of the three roots:
//! it always produces the same merged root and the same ordered conflict
//! stream, regardless of how the inputs were built. Conflicts are data,
//! not errors.
//!
//! # Semantics
//!
//! Per key, comparing each side's value (or absence) against base:
//!
//! - changed on one side only → the change is taken;
//! - identical change on both sides (including both deleting) → taken once;
//! - different changes on both sides (deleting counts as a change, so
//!   delete-vs-modify conflicts) → a [`Conflict`] record.
//!
//! # Conflicted keys are absent from the merged root
//!
//! A conflicted key is **excluded from the merged tree entirely** and
//! delivered only in the conflict stream. Neither side's value (nor the
//! base's) is silently preferred: the merged root is a deterministic
//! function with no hidden bias, and the caller — the graph layer's merge
//! orchestration (spec §6), which owns the `conflicts` map — decides how
//! to materialise each resolution. Callers MUST treat a merge with a
//! non-empty conflict stream as incomplete until every conflict is
//! resolved by a subsequent batch.

use acetone_store::{Bytes, ChunkStore};

use crate::error::ProllyError;
use crate::{BatchOp, Root, apply_batch, diff::DiffEntry, diff::diff};

/// One key changed incompatibly on both sides. `None` means the key is
/// absent in that tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The conflicted key (absent from the merged root).
    pub key: Bytes,
    /// The value in the merge base, if present.
    pub base: Option<Bytes>,
    /// The value in `ours`, if present.
    pub ours: Option<Bytes>,
    /// The value in `theirs`, if present.
    pub theirs: Option<Bytes>,
}

/// The result of a three-way merge: the merged root plus every conflicted
/// key, in ascending key order.
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    /// Root of the merged map — base plus all clean changes, minus every
    /// conflicted key.
    pub root: Root,
    /// Key-ordered conflict records. Empty for a clean merge.
    pub conflicts: Vec<Conflict>,
}

/// Three-way merge of `ours` and `theirs` against their common `base`.
///
/// All three roots must carry the same chunk parameters (they are fixed
/// per repository — spec §3.2); [`ProllyError::ParamsMismatch`] otherwise.
///
/// Cost: two structural diffs against base — O(changed keys), independent
/// of map size — plus one batch application.
pub fn merge<S: ChunkStore>(
    store: &S,
    base: &Root,
    ours: &Root,
    theirs: &Root,
) -> Result<MergeOutcome, ProllyError> {
    if base.params != ours.params || base.params != theirs.params {
        return Err(ProllyError::ParamsMismatch);
    }

    let mut ours_diff = diff(store, base, ours)?;
    let mut theirs_diff = diff(store, base, theirs)?;

    let mut ops: Vec<BatchOp> = Vec::new();
    let mut conflicts: Vec<Conflict> = Vec::new();

    let mut o = ours_diff.next().transpose()?;
    let mut t = theirs_diff.next().transpose()?;
    loop {
        match (o.take(), t.take()) {
            (None, None) => break,
            (Some(oe), None) => {
                ops.push(to_op(&oe));
                o = ours_diff.next().transpose()?;
                t = None;
            }
            (None, Some(te)) => {
                ops.push(to_op(&te));
                t = theirs_diff.next().transpose()?;
                o = None;
            }
            (Some(oe), Some(te)) => match oe.key.cmp(&te.key) {
                std::cmp::Ordering::Less => {
                    ops.push(to_op(&oe));
                    o = ours_diff.next().transpose()?;
                    t = Some(te);
                }
                std::cmp::Ordering::Greater => {
                    ops.push(to_op(&te));
                    t = theirs_diff.next().transpose()?;
                    o = Some(oe);
                }
                std::cmp::Ordering::Equal => {
                    // Both diffs are against the same base, so their
                    // `before` views of this key must agree; disagreement
                    // means the store returned inconsistent data.
                    if oe.before != te.before {
                        return Err(ProllyError::corrupt(
                            "three-way merge",
                            "the two diffs disagree on the base value of a key",
                        ));
                    }
                    if oe.after == te.after {
                        // Identical change on both sides: clean.
                        ops.push(to_op(&oe));
                    } else {
                        // Divergent change: conflict. The key is removed
                        // from the merged tree (a no-op if base lacked
                        // it); no resolution is applied here.
                        ops.push(BatchOp::Delete(oe.key.to_vec()));
                        conflicts.push(Conflict {
                            key: oe.key,
                            base: te.before,
                            ours: oe.after,
                            theirs: te.after,
                        });
                    }
                    o = ours_diff.next().transpose()?;
                    t = theirs_diff.next().transpose()?;
                }
            },
        }
    }

    let root = apply_batch(store, base, ops)?;
    Ok(MergeOutcome { root, conflicts })
}

/// The batch op that applies one side's change to the base.
fn to_op(e: &DiffEntry) -> BatchOp {
    match &e.after {
        Some(v) => BatchOp::Put(e.key.to_vec(), v.to_vec()),
        None => BatchOp::Delete(e.key.to_vec()),
    }
}
