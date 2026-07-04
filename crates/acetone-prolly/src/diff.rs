//! Structural diff between two roots (spec §3.2).
//!
//! `diff(a, b)` streams `(key, before, after)` in ascending key order.
//! Content addressing makes hash equality a proof of subtree equality, so
//! the walk skips every shared subtree and its cost is O(changed keys ×
//! tree height), independent of map size — the property benchmarked in
//! Phase 0 (ADR-0002: ~2 chunk reads per changed key at 1M keys).
//!
//! Height mismatches are handled by expanding the taller side until both
//! walks are at the same level; the trees need not share chunk parameters
//! (a diff across parameter changes is valid, merely unable to skip
//! subtrees).

use acetone_store::{Bytes, ChunkStore};

use crate::Root;
use crate::error::ProllyError;
use crate::node::{Node, NodeRef, read_node};

/// One changed key: `before` is the value in `a`, `after` the value in
/// `b`; `None` means absent on that side. Equal values are never emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    /// The key that differs.
    pub key: Bytes,
    /// The value in the `a` (old) tree, if present.
    pub before: Option<Bytes>,
    /// The value in the `b` (new) tree, if present.
    pub after: Option<Bytes>,
}

/// A key-aligned pair of same-level node runs whose contents differ.
struct Region {
    /// Level of the nodes the refs point at.
    level: u8,
    a: Vec<NodeRef>,
    b: Vec<NodeRef>,
    /// True only for the initial region holding the two root references,
    /// whose `last_key`s are placeholders rather than parent claims.
    pseudo: bool,
}

/// Ordered stream of differences between two roots. Yields `Err` once and
/// stops on storage corruption.
pub struct Diff<'s, S> {
    store: &'s S,
    /// Pending regions, key order; the top of the stack is the earliest.
    regions: Vec<Region>,
    /// Entries decoded from the current leaf region, drained in order.
    buffered: std::vec::IntoIter<DiffEntry>,
    done: bool,
}

/// Structural diff of `b` relative to `a`: an ordered stream of
/// `(key, before, after)` skipping every subtree the two roots share.
pub fn diff<'s, S: ChunkStore>(
    store: &'s S,
    a: &Root,
    b: &Root,
) -> Result<Diff<'s, S>, ProllyError> {
    let mut out = Diff {
        store,
        regions: Vec::new(),
        buffered: Vec::new().into_iter(),
        done: false,
    };
    if a.hash == b.hash {
        // Content addressing: identical roots are identical maps.
        out.done = true;
        return Ok(out);
    }
    // Start from pseudo-references to the two roots, expanding the taller
    // side until both runs point at nodes of the same level.
    let pseudo = |root: &Root| NodeRef {
        last_key: Bytes::new(),
        hash: root.hash,
    };
    let mut la = a.top_level();
    let mut lb = b.top_level();
    let mut ra = vec![pseudo(a)];
    let mut rb = vec![pseudo(b)];
    while la > lb {
        ra = expand(store, &ra, la, true)?;
        la -= 1;
    }
    while lb > la {
        rb = expand(store, &rb, lb, true)?;
        lb -= 1;
    }
    // The initial region skips last-key claim checks: at least one side
    // still holds a placeholder root reference (and after height
    // alignment the other side's refs, though real, sit next to it).
    // Every deeper region carries real claims and is fully checked.
    out.regions.push(Region {
        level: la,
        a: ra,
        b: rb,
        pseudo: true,
    });
    Ok(out)
}

/// Read every node in `refs` (which are at `level`) and concatenate their
/// children. `pseudo` marks root references whose `last_key` is not a real
/// parent claim and must not be verified.
fn expand<S: ChunkStore>(
    store: &S,
    refs: &[NodeRef],
    level: u8,
    pseudo: bool,
) -> Result<Vec<NodeRef>, ProllyError> {
    let mut out = Vec::new();
    for r in refs {
        let expect_last = (!pseudo).then_some(r.last_key.as_ref());
        match read_node(store, &r.hash, level, expect_last, None)? {
            Node::Inner(children) => out.extend(children),
            Node::Leaf(_) => unreachable!("level > 0 checked by read_node"),
        }
    }
    Ok(out)
}

/// Read every leaf in `refs` and concatenate their entries. `pseudo` is
/// true only for the initial root-references region (height-1 trees).
fn leaf_entries<S: ChunkStore>(
    store: &S,
    refs: &[NodeRef],
    pseudo: bool,
) -> Result<Vec<(Bytes, Bytes)>, ProllyError> {
    let mut out = Vec::new();
    for r in refs {
        let expect_last = (!pseudo).then_some(r.last_key.as_ref());
        match read_node(store, &r.hash, 0, expect_last, None)? {
            Node::Leaf(entries) => out.extend(entries),
            Node::Inner(_) => unreachable!("level 0 checked by read_node"),
        }
    }
    Ok(out)
}

impl<S: ChunkStore> Diff<'_, S> {
    /// Process regions until leaf differences are buffered or everything
    /// is exhausted.
    fn refill(&mut self) -> Result<(), ProllyError> {
        while let Some(region) = self.regions.pop() {
            if region.level == 0 {
                let ea = leaf_entries(self.store, &region.a, region.pseudo)?;
                let eb = leaf_entries(self.store, &region.b, region.pseudo)?;
                let entries = merge_leaf_diff(ea, eb);
                if !entries.is_empty() {
                    self.buffered = entries.into_iter();
                    return Ok(());
                }
                continue;
            }
            let ca = expand(self.store, &region.a, region.level, region.pseudo)?;
            let cb = expand(self.store, &region.b, region.level, region.pseudo)?;
            // Push sub-regions in reverse so the earliest is on top.
            let mut subs = align_regions(&ca, &cb, region.level - 1);
            subs.reverse();
            self.regions.extend(subs);
        }
        self.done = true;
        Ok(())
    }
}

/// Scan two same-level child runs covering the same key span, skipping
/// hash-equal children and grouping each maximal mismatched stretch into a
/// key-aligned region (the two sides re-align where their trailing
/// `last_key`s agree).
fn align_regions(a: &[NodeRef], b: &[NodeRef], level: u8) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i].hash == b[j].hash {
            i += 1;
            j += 1;
            continue;
        }
        let mut ra: Vec<NodeRef> = Vec::new();
        let mut rb: Vec<NodeRef> = Vec::new();
        loop {
            let a_span = ra.last().map(|r| r.last_key.as_ref());
            let b_span = rb.last().map(|r| r.last_key.as_ref());
            if let (Some(x), Some(y)) = (a_span, b_span)
                && x == y
            {
                break;
            }
            let extend_a = match (a_span, b_span) {
                (None, _) => true,
                (_, None) => false,
                (Some(x), Some(y)) => x < y,
            };
            if extend_a && i < a.len() {
                ra.push(a[i].clone());
                i += 1;
            } else if !extend_a && j < b.len() {
                rb.push(b[j].clone());
                j += 1;
            } else if i < a.len() {
                ra.push(a[i].clone());
                i += 1;
            } else if j < b.len() {
                rb.push(b[j].clone());
                j += 1;
            } else {
                break;
            }
        }
        regions.push(Region {
            level,
            a: ra,
            b: rb,
            pseudo: false,
        });
    }
    regions
}

/// Key-merge two sorted leaf-entry runs covering the same key span,
/// keeping only genuine differences.
fn merge_leaf_diff(a: Vec<(Bytes, Bytes)>, b: Vec<(Bytes, Bytes)>) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    let mut ai = a.into_iter().peekable();
    let mut bi = b.into_iter().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (Some((ka, _)), Some((kb, _))) => match ka.cmp(kb) {
                std::cmp::Ordering::Less => {
                    let (key, va) = ai.next().expect("peeked");
                    out.push(DiffEntry {
                        key,
                        before: Some(va),
                        after: None,
                    });
                }
                std::cmp::Ordering::Greater => {
                    let (key, vb) = bi.next().expect("peeked");
                    out.push(DiffEntry {
                        key,
                        before: None,
                        after: Some(vb),
                    });
                }
                std::cmp::Ordering::Equal => {
                    let (key, va) = ai.next().expect("peeked");
                    let (_, vb) = bi.next().expect("peeked");
                    if va != vb {
                        out.push(DiffEntry {
                            key,
                            before: Some(va),
                            after: Some(vb),
                        });
                    }
                }
            },
            (Some(_), None) => {
                let (key, va) = ai.next().expect("peeked");
                out.push(DiffEntry {
                    key,
                    before: Some(va),
                    after: None,
                });
            }
            (None, Some(_)) => {
                let (key, vb) = bi.next().expect("peeked");
                out.push(DiffEntry {
                    key,
                    before: None,
                    after: Some(vb),
                });
            }
            (None, None) => break,
        }
    }
    out
}

impl<S: ChunkStore> Iterator for Diff<'_, S> {
    type Item = Result<DiffEntry, ProllyError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.buffered.next() {
                return Some(Ok(entry));
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.refill() {
                self.done = true;
                return Some(Err(e));
            }
        }
    }
}
