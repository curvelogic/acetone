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
///
/// Streaming: leaf chunks are read one at a time as the iterator is
/// consumed, so a partially consumed diff of a large change costs only the
/// chunks behind the entries actually taken. Inner (reference-level) nodes
/// of a mismatched region are expanded a level at a time; the refs of a
/// fully-different level are held in memory (~35 bytes/chunk), which is
/// the same order as the region's key material and only significant when
/// the two maps share nothing.
pub struct Diff<'s, S> {
    store: &'s S,
    /// Pending regions, key order; the top of the stack is the earliest.
    regions: Vec<Region>,
    /// The level-0 region currently being merged, if any.
    leaves: Option<LeafMerge>,
    done: bool,
}

/// Incremental key-merge of one level-0 region: reads each side's next
/// leaf only when its entry buffer runs dry.
struct LeafMerge {
    a_refs: std::vec::IntoIter<NodeRef>,
    b_refs: std::vec::IntoIter<NodeRef>,
    a: std::collections::VecDeque<(Bytes, Bytes)>,
    b: std::collections::VecDeque<(Bytes, Bytes)>,
    pseudo: bool,
}

impl LeafMerge {
    fn new(region: Region) -> Self {
        debug_assert_eq!(region.level, 0);
        LeafMerge {
            a_refs: region.a.into_iter(),
            b_refs: region.b.into_iter(),
            a: std::collections::VecDeque::new(),
            b: std::collections::VecDeque::new(),
            pseudo: region.pseudo,
        }
    }

    /// Top up one side's buffer from its next leaf, if needed and possible.
    fn top_up<S: ChunkStore>(
        store: &S,
        buf: &mut std::collections::VecDeque<(Bytes, Bytes)>,
        refs: &mut std::vec::IntoIter<NodeRef>,
        pseudo: bool,
    ) -> Result<(), ProllyError> {
        while buf.is_empty() {
            let Some(r) = refs.next() else { return Ok(()) };
            let expect_last = (!pseudo).then_some(r.last_key.as_ref());
            match read_node(store, &r.hash, 0, expect_last, None)? {
                Node::Leaf(entries) => buf.extend(entries),
                Node::Inner(_) => unreachable!("level 0 checked by read_node"),
            }
        }
        Ok(())
    }

    /// The next difference in this region, or `None` when it is drained.
    fn next_entry<S: ChunkStore>(&mut self, store: &S) -> Result<Option<DiffEntry>, ProllyError> {
        loop {
            Self::top_up(store, &mut self.a, &mut self.a_refs, self.pseudo)?;
            Self::top_up(store, &mut self.b, &mut self.b_refs, self.pseudo)?;
            match (self.a.front(), self.b.front()) {
                (None, None) => return Ok(None),
                (Some(_), None) => {
                    let (key, va) = self.a.pop_front().expect("checked front");
                    return Ok(Some(DiffEntry {
                        key,
                        before: Some(va),
                        after: None,
                    }));
                }
                (None, Some(_)) => {
                    let (key, vb) = self.b.pop_front().expect("checked front");
                    return Ok(Some(DiffEntry {
                        key,
                        before: None,
                        after: Some(vb),
                    }));
                }
                (Some((ka, _)), Some((kb, _))) => match ka.cmp(kb) {
                    std::cmp::Ordering::Less => {
                        let (key, va) = self.a.pop_front().expect("checked front");
                        return Ok(Some(DiffEntry {
                            key,
                            before: Some(va),
                            after: None,
                        }));
                    }
                    std::cmp::Ordering::Greater => {
                        let (key, vb) = self.b.pop_front().expect("checked front");
                        return Ok(Some(DiffEntry {
                            key,
                            before: None,
                            after: Some(vb),
                        }));
                    }
                    std::cmp::Ordering::Equal => {
                        let (key, va) = self.a.pop_front().expect("checked front");
                        let (_, vb) = self.b.pop_front().expect("checked front");
                        if va != vb {
                            return Ok(Some(DiffEntry {
                                key,
                                before: Some(va),
                                after: Some(vb),
                            }));
                        }
                        // Equal entry: skip and continue.
                    }
                },
            }
        }
    }
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
        leaves: None,
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

impl<S: ChunkStore> Diff<'_, S> {
    /// The next difference, descending into pending regions as needed.
    fn next_entry(&mut self) -> Result<Option<DiffEntry>, ProllyError> {
        loop {
            if let Some(leaves) = &mut self.leaves {
                if let Some(entry) = leaves.next_entry(self.store)? {
                    return Ok(Some(entry));
                }
                self.leaves = None;
            }
            let Some(region) = self.regions.pop() else {
                return Ok(None);
            };
            if region.level == 0 {
                self.leaves = Some(LeafMerge::new(region));
                continue;
            }
            let ca = expand(self.store, &region.a, region.level, region.pseudo)?;
            let cb = expand(self.store, &region.b, region.level, region.pseudo)?;
            // Push sub-regions in reverse so the earliest is on top.
            let mut subs = align_regions(&ca, &cb, region.level - 1);
            subs.reverse();
            self.regions.extend(subs);
        }
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

impl<S: ChunkStore> Iterator for Diff<'_, S> {
    type Item = Result<DiffEntry, ProllyError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_entry() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
