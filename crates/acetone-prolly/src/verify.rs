//! Integrity walk for `fsck`: read and validate every chunk of a tree.
//!
//! [`verify_reachable`] is the verifier's eyes. Unlike
//! [`collect_reachable_chunks`](crate::collect_reachable_chunks) — which
//! only needs child *addresses* and therefore never reads leaves — this
//! walk reads every chunk's bytes (root, internal nodes and leaves) and
//! validates each against its position in the tree with the same checks
//! the read paths apply in `read_node`: the level tag must match the level
//! the tree claims, the child's last key must equal its parent's boundary
//! claim, and its first key must lie above the preceding sibling's. A
//! chunk that fails any check, or that the store cannot return, is a
//! [`ChunkFault`].
//!
//! # Totality (hostile input)
//!
//! The walk never panics and never aborts on the first fault: sibling
//! subtrees are still verified, so one damaged chunk does not mask the
//! rest of the tree. Termination and bounded work on a maliciously
//! cross-linked chunk set come from two things: levels strictly decrease
//! on the way down (so no descent path exceeds
//! [`MAX_HEIGHT`](crate::MAX_HEIGHT)), and each inner node's subtree is
//! **expanded at most once** however many parents point at it (a diamond
//! cannot blow up combinatorially, because a hash's children are enqueued
//! only the first time it is reached). Total reads are therefore bounded by
//! the number of references in the tree, not by any exponential of its
//! depth. Every reference is still *position-checked* — level tag, parent
//! boundary claim, sibling lower bound — so a chunk that is valid at one
//! position but referenced from a wrong one (a self-reference, a forged
//! pointer) is caught, not silently deduplicated away.
//!
//! Only inner nodes are cached (their decoded child lists, to re-check a
//! second parent's claim and to mark the subtree expanded); leaves are
//! position-checked and dropped, so the walk holds at most the internal
//! nodes of one map in memory, not its leaves.
//!
//! A missing or corrupt *internal* node hides the addresses of everything
//! beneath it, so faults strictly below a reported fault are not
//! enumerated; the reported parent is the actionable signal (see
//! ADR-0011).

use std::collections::{HashMap, HashSet};

use acetone_store::{Bytes, ChunkStore, Hash};

use crate::Root;
use crate::node::{Node, decode_node_self};

/// Why a chunk referenced by a tree fails verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkFaultKind {
    /// Referenced by the tree but absent from the store (a dangling
    /// reference — a gc'd or untransferred chunk).
    Missing,
    /// Present but not a valid node at its position in the tree, or the
    /// store could not return it (e.g. a physically damaged loose object
    /// whose zlib/hash check fails on read). Either way the chunk cannot
    /// be trusted as the node the tree requires.
    Corrupt,
}

/// One faulty chunk found by [`verify_reachable`], always naming the
/// offending chunk — for [`ChunkFaultKind::Corrupt`] as well as
/// [`ChunkFaultKind::Missing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkFault {
    /// Address of the offending chunk.
    pub hash: Hash,
    /// Whether the chunk is missing or corrupt.
    pub kind: ChunkFaultKind,
    /// Human-readable detail (the underlying reason).
    pub reason: String,
}

/// One pending reference to verify: a chunk address plus the constraints
/// its position in the tree imposes.
struct Reference {
    hash: Hash,
    /// The level the tree says this chunk sits at.
    level: u8,
    /// The parent's claim about this child's largest key (`None` for the
    /// root, which has no parent).
    last_key: Option<Bytes>,
    /// The preceding sibling's largest key: an exclusive lower bound on
    /// this chunk's smallest key (`None` for a first child or the root).
    min_key: Option<Bytes>,
}

/// A cached inner node: enough to re-check any parent's claim about it and
/// to enumerate its children, without re-reading the chunk.
struct InnerNode {
    level: u8,
    children: Vec<(Bytes, Hash)>,
}

/// Read and validate every chunk reachable from `root`, returning every
/// fault found. An **empty** vector means the tree is intact: every chunk
/// — root, internal nodes and leaves — is present and decodes as a valid
/// node consistent with its position (level tag, parent boundary claim,
/// sibling lower bound).
///
/// See the module docs for the totality and under-reporting guarantees.
pub fn verify_reachable<S: ChunkStore>(store: &S, root: &Root) -> Vec<ChunkFault> {
    let mut faults = Vec::new();
    // Inner nodes seen so far. Membership doubles as the "already expanded"
    // marker — a hash is inserted exactly when its children are enqueued —
    // so every subtree is walked once and a diamond cannot re-expand.
    let mut inner_cache: HashMap<Hash, InnerNode> = HashMap::new();
    // Missing/unreadable chunks already reported, so one broken chunk
    // referenced from several places yields one finding.
    let mut reported: HashSet<Hash> = HashSet::new();

    let mut frontier: Vec<Reference> = vec![Reference {
        hash: root.hash(),
        level: root.top_level(),
        last_key: None,
        min_key: None,
    }];

    while let Some(reference) = frontier.pop() {
        // A chunk we have already decoded as an inner node: re-check this
        // parent's claim against the cached decode, but do not re-read or
        // re-expand it. Re-checking level and the last-key claim here is
        // enough: a shared subtree that is *misplaced* (its keys fall below
        // a tighter parent's position) cannot reach this branch undetected,
        // because from a single root any two parents of a chunk have a
        // lowest common ancestor that orders them — the misplaced parent is
        // either the root's later child (flagged by its own bound before its
        // children are enqueued) or is descended fresh, where the bound is
        // threaded to the leaves. Two parents both descending a shared child
        // would need overlapping, non-orderable sibling ranges, which the
        // ancestor check rejects first.
        if let Some(inner) = inner_cache.get(&reference.hash) {
            let first = inner.children.first().map(|(k, _)| k.as_ref());
            let last = inner.children.last().map(|(k, _)| k.as_ref());
            if let Some(reason) = position_fault(inner.level, first, last, &reference) {
                faults.push(ChunkFault {
                    hash: reference.hash,
                    kind: ChunkFaultKind::Corrupt,
                    reason,
                });
            }
            continue;
        }

        let (level, node) = match store.get(&reference.hash) {
            Ok(Some(bytes)) => match decode_node_self(&bytes) {
                Ok(decoded) => decoded,
                Err(err) => {
                    if reported.insert(reference.hash) {
                        faults.push(ChunkFault {
                            hash: reference.hash,
                            kind: ChunkFaultKind::Corrupt,
                            reason: err.to_string(),
                        });
                    }
                    continue;
                }
            },
            Ok(None) => {
                if reported.insert(reference.hash) {
                    faults.push(ChunkFault {
                        hash: reference.hash,
                        kind: ChunkFaultKind::Missing,
                        reason: "referenced by the tree but absent from the store".to_owned(),
                    });
                }
                continue;
            }
            // Present but the store could not return it — a physically
            // damaged loose object whose zlib/hash check fails on read, an
            // oversized object, a wrong-kind object. Treat as corruption of
            // this chunk (ADR-0011), never a propagated abort or a panic.
            Err(err) => {
                if reported.insert(reference.hash) {
                    faults.push(ChunkFault {
                        hash: reference.hash,
                        kind: ChunkFaultKind::Corrupt,
                        reason: format!("store could not return the chunk: {err}"),
                    });
                }
                continue;
            }
        };

        if let Some(reason) = position_fault(level, node.first_key(), node.last_key(), &reference) {
            // Mis-positioned: report it and do not descend (its children's
            // claimed positions would be meaningless).
            faults.push(ChunkFault {
                hash: reference.hash,
                kind: ChunkFaultKind::Corrupt,
                reason,
            });
            continue;
        }

        if let Node::Inner(refs) = node {
            let children: Vec<(Bytes, Hash)> =
                refs.iter().map(|r| (r.last_key.clone(), r.hash)).collect();
            let child_level = level - 1;
            // The first child inherits *this* node's own lower bound (the
            // ancestor bound threaded down the left spine); each later child
            // is bounded below by its preceding sibling's last key. Losing
            // this inheritance on the first child was a false-clean: a leaf
            // on the left spine could hold keys below its position and pass.
            let mut prev: Option<Bytes> = reference.min_key.clone();
            for (last_key, hash) in &children {
                frontier.push(Reference {
                    hash: *hash,
                    level: child_level,
                    last_key: Some(last_key.clone()),
                    min_key: prev.take(),
                });
                prev = Some(last_key.clone());
            }
            // Cache after enqueueing: this hash is now "expanded".
            inner_cache.insert(reference.hash, InnerNode { level, children });
        }
    }

    faults
}

/// Check a chunk against the constraints its position imposes, mirroring
/// the checks [`crate::node::read_node`] applies on the read paths. Returns
/// `Some(reason)` on the first violation, `None` when the chunk sits where
/// the tree says it does.
fn position_fault(
    level: u8,
    first_key: Option<&[u8]>,
    last_key: Option<&[u8]>,
    reference: &Reference,
) -> Option<String> {
    if level != reference.level {
        return Some(format!(
            "level tag {level} where the tree requires level {}",
            reference.level
        ));
    }
    if let Some(want) = reference.last_key.as_deref() {
        match last_key {
            Some(got) if got == want => {}
            _ => return Some("chunk does not end at the key its parent declares".to_owned()),
        }
    }
    if let Some(min) = reference.min_key.as_deref()
        && let Some(first) = first_key
        && first <= min
    {
        return Some("chunk contains keys below its position in the tree".to_owned());
    }
    None
}
