//! Ordered range scans, forward and reverse (spec §3.2 requires both).
//!
//! Both directions share one iterator: the scan seeks the boundary leaf on
//! its *entry* side (start bound for forward, end bound for reverse),
//! loading only that root→leaf path, then steps leaf by leaf, checking the
//! *exit* bound as it goes. Reverse is the exact mirror of forward — same
//! stack, same descent, opposite index arithmetic — not a buffered
//! afterthought.
//!
//! Corruption handling: every node is validated on read (level tag,
//! parent-declared boundary, intra-node ordering), and the scan
//! additionally enforces cross-leaf monotonicity on the keys it yields, so
//! a hostile tree produces an `Err` item, never out-of-order or duplicated
//! results. After yielding an error the scan is fused.

use std::ops::{Bound, RangeBounds};

use acetone_store::{Bytes, ChunkStore};

use crate::Root;
use crate::error::ProllyError;
use crate::node::{Node, NodeRef, read_node};

/// Scan direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Reverse,
}

/// One level of the descent path: the children of an inner node and the
/// index of the child the scan is currently inside.
#[derive(Debug)]
struct Frame {
    refs: Vec<NodeRef>,
    idx: usize,
    /// Level of the nodes the refs point at.
    child_level: u8,
}

/// Ordered iterator over a key range. Yields `Err` once and stops on
/// storage corruption. Forward scans yield ascending keys; reverse scans
/// yield descending keys.
pub struct Scan<'s, S> {
    store: &'s S,
    stack: Vec<Frame>,
    leaf: Vec<(Bytes, Bytes)>,
    /// Forward: index of the next entry to yield. Reverse: one past the
    /// next entry to yield (0 = leaf exhausted).
    pos: usize,
    /// The exit bound, checked as entries are yielded: the end bound for
    /// forward scans, the start bound for reverse scans.
    exit: Bound<Vec<u8>>,
    dir: Direction,
    /// Last key yielded, for cross-leaf monotonicity enforcement.
    prev_key: Option<Bytes>,
    done: bool,
}

fn clone_bound(b: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match b {
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// Ordered forward scan over `range`, loading only the root→leaf path of
/// the start bound and then one leaf at a time.
pub fn scan<'s, S, R>(store: &'s S, root: &Root, range: R) -> Result<Scan<'s, S>, ProllyError>
where
    S: ChunkStore,
    R: RangeBounds<[u8]>,
{
    seek(
        store,
        root,
        range.start_bound(),
        clone_bound(range.end_bound()),
        Direction::Forward,
    )
}

/// Ordered reverse scan over `range`: the same entries as [`scan`], in
/// descending key order, loading only the root→leaf path of the end bound
/// and then one leaf at a time.
pub fn scan_rev<'s, S, R>(store: &'s S, root: &Root, range: R) -> Result<Scan<'s, S>, ProllyError>
where
    S: ChunkStore,
    R: RangeBounds<[u8]>,
{
    seek(
        store,
        root,
        range.end_bound(),
        clone_bound(range.start_bound()),
        Direction::Reverse,
    )
}

/// Descend to the boundary leaf on the scan's entry side.
fn seek<'s, S: ChunkStore>(
    store: &'s S,
    root: &Root,
    entry_bound: Bound<&[u8]>,
    exit: Bound<Vec<u8>>,
    dir: Direction,
) -> Result<Scan<'s, S>, ProllyError> {
    let mut out = Scan {
        store,
        stack: Vec::new(),
        leaf: Vec::new(),
        pos: 0,
        exit,
        dir,
        prev_key: None,
        done: false,
    };

    let mut hash = root.hash;
    let mut level = root.top_level();
    let mut expect_last_key: Option<Bytes> = None;
    loop {
        let node = read_node(store, &hash, level, expect_last_key.as_deref(), None)?;
        match node {
            Node::Inner(refs) => {
                // The first child whose key span can contain in-range
                // entries, seen from this scan's entry side.
                let split = match entry_bound {
                    Bound::Included(k) => refs.partition_point(|r| r.last_key.as_ref() < k),
                    Bound::Excluded(k) => match dir {
                        // Forward enters above k, reverse enters below it;
                        // either way the child holding the boundary key's
                        // neighbours is the one whose last_key first
                        // reaches the bound.
                        Direction::Forward => refs.partition_point(|r| r.last_key.as_ref() <= k),
                        Direction::Reverse => refs.partition_point(|r| r.last_key.as_ref() < k),
                    },
                    Bound::Unbounded => match dir {
                        Direction::Forward => 0,
                        Direction::Reverse => refs.len() - 1,
                    },
                };
                let idx = match dir {
                    Direction::Forward => {
                        if split == refs.len() {
                            // Every key in the tree is below the start
                            // bound: empty scan.
                            out.done = true;
                            return Ok(out);
                        }
                        split
                    }
                    // Reverse: if every child is below the bound, enter at
                    // the rightmost; in-leaf positioning trims the rest.
                    Direction::Reverse => split.min(refs.len() - 1),
                };
                hash = refs[idx].hash;
                expect_last_key = Some(refs[idx].last_key.clone());
                let child_level = level - 1;
                out.stack.push(Frame {
                    refs,
                    idx,
                    child_level,
                });
                level = child_level;
            }
            Node::Leaf(entries) => {
                out.pos = match dir {
                    Direction::Forward => match entry_bound {
                        Bound::Included(k) => entries.partition_point(|(ek, _)| ek.as_ref() < k),
                        Bound::Excluded(k) => entries.partition_point(|(ek, _)| ek.as_ref() <= k),
                        Bound::Unbounded => 0,
                    },
                    Direction::Reverse => match entry_bound {
                        Bound::Included(k) => entries.partition_point(|(ek, _)| ek.as_ref() <= k),
                        Bound::Excluded(k) => entries.partition_point(|(ek, _)| ek.as_ref() < k),
                        Bound::Unbounded => entries.len(),
                    },
                };
                out.leaf = entries;
                return Ok(out);
            }
        }
    }
}

impl<S: ChunkStore> Scan<'_, S> {
    /// Whether `key` is within the exit bound for this direction.
    fn within_exit(&self, key: &[u8]) -> bool {
        match (&self.exit, self.dir) {
            (Bound::Included(e), Direction::Forward) => key <= e.as_slice(),
            (Bound::Excluded(e), Direction::Forward) => key < e.as_slice(),
            (Bound::Included(e), Direction::Reverse) => key >= e.as_slice(),
            (Bound::Excluded(e), Direction::Reverse) => key > e.as_slice(),
            (Bound::Unbounded, _) => true,
        }
    }

    /// Keys must move strictly in the scan direction; anything else is a
    /// forged tree, reported rather than yielded.
    fn check_monotonic(&self, key: &[u8]) -> Result<(), ProllyError> {
        if let Some(prev) = &self.prev_key {
            let ok = match self.dir {
                Direction::Forward => key > prev.as_ref(),
                Direction::Reverse => key < prev.as_ref(),
            };
            if !ok {
                return Err(ProllyError::corrupt(
                    "range scan",
                    "keys not strictly ordered across leaves",
                ));
            }
        }
        Ok(())
    }

    /// Move to the next leaf in the scan direction, popping exhausted
    /// frames and descending on the entry side of the next subtree.
    /// Returns false when the tree is exhausted.
    fn advance_leaf(&mut self) -> Result<bool, ProllyError> {
        loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(false);
            };
            match self.dir {
                Direction::Forward => {
                    if frame.idx + 1 < frame.refs.len() {
                        frame.idx += 1;
                        break;
                    }
                }
                Direction::Reverse => {
                    if frame.idx > 0 {
                        frame.idx -= 1;
                        break;
                    }
                }
            }
            self.stack.pop();
        }
        // Descend from the new child to a leaf, entering each inner node
        // on this direction's near side.
        let (mut hash, mut expect_last_key, mut level) = {
            let frame = self.stack.last().expect("frame checked above");
            let r = &frame.refs[frame.idx];
            (r.hash, r.last_key.clone(), frame.child_level)
        };
        loop {
            let node = read_node(self.store, &hash, level, Some(&expect_last_key), None)?;
            match node {
                Node::Inner(refs) => {
                    let idx = match self.dir {
                        Direction::Forward => 0,
                        Direction::Reverse => refs.len() - 1,
                    };
                    hash = refs[idx].hash;
                    expect_last_key = refs[idx].last_key.clone();
                    let child_level = level - 1;
                    self.stack.push(Frame {
                        refs,
                        idx,
                        child_level,
                    });
                    level = child_level;
                }
                Node::Leaf(entries) => {
                    self.pos = match self.dir {
                        Direction::Forward => 0,
                        Direction::Reverse => entries.len(),
                    };
                    self.leaf = entries;
                    return Ok(true);
                }
            }
        }
    }

    /// The next entry within the current leaf, if any.
    fn take_from_leaf(&mut self) -> Option<(Bytes, Bytes)> {
        match self.dir {
            Direction::Forward => {
                let entry = self.leaf.get(self.pos)?.clone();
                self.pos += 1;
                Some(entry)
            }
            Direction::Reverse => {
                if self.pos == 0 {
                    return None;
                }
                self.pos -= 1;
                Some(self.leaf[self.pos].clone())
            }
        }
    }
}

impl<S: ChunkStore> Iterator for Scan<'_, S> {
    type Item = Result<(Bytes, Bytes), ProllyError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if let Some((k, v)) = self.take_from_leaf() {
                if !self.within_exit(&k) {
                    self.done = true;
                    return None;
                }
                if let Err(e) = self.check_monotonic(&k) {
                    self.done = true;
                    return Some(Err(e));
                }
                self.prev_key = Some(k.clone());
                return Some(Ok((k, v)));
            }
            match self.advance_leaf() {
                Ok(true) => continue,
                Ok(false) => {
                    self.done = true;
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}
