//! Prolly-tree construction, point lookup and batched mutation.
//!
//! Layout: sorted key/value leaf chunks split at content-defined boundaries
//! (see `chunker`); internal nodes list `(last_key, child hash)` entries and
//! are split by the same chunker over their serialised entry stream; the
//! address of every chunk IS its content hash in the [`ChunkStore`].
//!
//! # History independence (spec §3.2, normative)
//!
//! Every level's chunking is a pure function of that level's serialised
//! entry stream, which is itself a pure function of map contents.
//! [`apply_batch`] rebuilds only the affected region of each level and
//! splices back into existing chunks at the first re-synchronised boundary,
//! producing a tree bit-identical to [`bulk_load`] of the same contents.
//!
//! # What a batch loads
//!
//! Unlike the Phase 0 spike (which read every internal node per batch),
//! [`apply_batch`] descends only the root→leaf paths whose key ranges
//! intersect the batch. Untouched subtrees travel through the rebuild as
//! opaque references and are re-attached verbatim; they are opened —
//! one node per level — only when a shifted chunk boundary cascades into
//! them, which the content-defined chunker bounds to an O(1) expected
//! number of chunks per level.

use std::collections::VecDeque;

use acetone_store::{Bytes, ChunkStore, Hash};

use crate::chunker::{ChunkParams, Chunker};
use crate::error::ProllyError;
use crate::node::{
    NODE_HEADER_LEN, Node, NodeRef, encode_inner_entry, encode_leaf_entry, read_node,
};
use crate::{BatchOp, MAX_HEIGHT, Root};

/// An entry in some level's serialised stream, in memory.
#[derive(Debug, Clone)]
pub(crate) enum Entry {
    Leaf { key: Bytes, value: Bytes },
    Inner { last_key: Bytes, hash: Hash },
}

impl Entry {
    fn key(&self) -> &[u8] {
        match self {
            Entry::Leaf { key, .. } => key,
            Entry::Inner { last_key, .. } => last_key,
        }
    }
}

/// One item of a level's input stream during a (re)build, in key order.
#[derive(Debug)]
enum Item {
    /// An untouched old node at the current level. Reused verbatim —
    /// without being read — when the builder sits at a chunk boundary and
    /// the node's own trailing cut is reproducible; otherwise read and
    /// re-chunked.
    Old { node: NodeRef, is_final: bool },
    /// An untouched old subtree rooted at `level` (strictly above the
    /// current one). Passes through unopened unless a shifted boundary
    /// cascades into it, in which case it is expanded one level at a time.
    Subtree {
        node: NodeRef,
        level: u8,
        is_final: bool,
    },
    /// Fresh entries to chunk.
    Fresh(Vec<Entry>),
}

/// One output of chunking a level.
#[derive(Debug)]
enum OutItem {
    /// A chunk at this level: newly written, or an old node emitted
    /// verbatim.
    Chunk(NodeRef),
    /// An untouched subtree passed through towards its own level.
    Subtree {
        node: NodeRef,
        level: u8,
        is_final: bool,
    },
}

/// An internal node read (opened) during descent, recorded so the rebuild
/// can re-attach it verbatim if its children all survive unchanged.
#[derive(Debug)]
struct OpenedInner {
    node: NodeRef,
    is_final: bool,
    children: Vec<Hash>,
}

/// Streaming builder for one level: buffers serialised entries, cuts at
/// content-defined boundaries, writes each chunk to the store.
struct LevelBuilder {
    level: u8,
    chunker: Chunker,
    buf: Vec<u8>,
    count: u32,
    last_key: Vec<u8>,
    out: Vec<OutItem>,
}

impl LevelBuilder {
    fn new(level: u8, params: ChunkParams) -> Self {
        LevelBuilder {
            level,
            chunker: Chunker::new(params),
            buf: Vec::new(),
            count: 0,
            last_key: Vec::new(),
            out: Vec::new(),
        }
    }

    fn at_boundary(&self) -> bool {
        self.count == 0
    }

    fn push<S: ChunkStore>(&mut self, store: &S, entry: &Entry) -> Result<(), ProllyError> {
        let start = self.buf.len();
        match entry {
            Entry::Leaf { key, value } => {
                debug_assert_eq!(self.level, 0, "leaf entry above level 0");
                encode_leaf_entry(key, value, &mut self.buf)?;
            }
            Entry::Inner { last_key, hash } => {
                debug_assert!(self.level > 0, "inner entry at level 0");
                encode_inner_entry(last_key, hash, &mut self.buf)?;
            }
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(ProllyError::TooManyEntries)?;
        self.last_key.clear();
        self.last_key.extend_from_slice(entry.key());
        if self.chunker.feed_entry(&self.buf[start..]) {
            self.emit(store)?;
        }
        Ok(())
    }

    fn emit<S: ChunkStore>(&mut self, store: &S) -> Result<(), ProllyError> {
        let mut chunk = Vec::with_capacity(NODE_HEADER_LEN + self.buf.len());
        chunk.push(self.level);
        chunk.extend_from_slice(&self.count.to_be_bytes());
        chunk.extend_from_slice(&self.buf);
        let hash = store.put(&chunk)?;
        self.out.push(OutItem::Chunk(NodeRef {
            last_key: Bytes::from(std::mem::take(&mut self.last_key)),
            hash,
        }));
        self.buf.clear();
        self.count = 0;
        self.chunker.reset();
        Ok(())
    }
}

/// Decode a node's entries for re-chunking.
fn node_entries(node: Node) -> Vec<Entry> {
    match node {
        Node::Leaf(entries) => entries
            .into_iter()
            .map(|(key, value)| Entry::Leaf { key, value })
            .collect(),
        Node::Inner(refs) => refs
            .into_iter()
            .map(|r| Entry::Inner {
                last_key: r.last_key,
                hash: r.hash,
            })
            .collect(),
    }
}

/// Chunk one level of the tree from its item stream. Bit-identical to
/// chunking the fully expanded entry stream from scratch: reuse and
/// pass-through only happen where the produced bytes are provably the same
/// (builder at a boundary, and the old node's trailing cut reproducible —
/// i.e. the old node was not its level's final chunk, whose cut may have
/// been end-of-stream rather than content-defined, unless it is final here
/// too).
fn chunk_level<S: ChunkStore>(
    store: &S,
    level: u8,
    params: ChunkParams,
    items: Vec<Item>,
) -> Result<Vec<OutItem>, ProllyError> {
    let mut b = LevelBuilder::new(level, params);
    let mut queue: VecDeque<Item> = items.into();
    while let Some(item) = queue.pop_front() {
        let is_last = queue.is_empty();
        match item {
            Item::Fresh(entries) => {
                for e in &entries {
                    b.push(store, e)?;
                }
            }
            Item::Old { node, is_final } => {
                if b.at_boundary() && (!is_final || is_last) {
                    b.out.push(OutItem::Chunk(node));
                } else {
                    let n = read_node(store, &node.hash, level, Some(&node.last_key), None)?;
                    for e in node_entries(n) {
                        b.push(store, &e)?;
                    }
                }
            }
            Item::Subtree {
                node,
                level: sub_level,
                is_final,
            } => {
                debug_assert!(sub_level > level, "subtree at or below current level");
                if b.at_boundary() && (!is_final || is_last) {
                    b.out.push(OutItem::Subtree {
                        node,
                        level: sub_level,
                        is_final,
                    });
                } else {
                    // Cascade: a shifted boundary ran into this untouched
                    // subtree. Open exactly one node and re-queue its
                    // children; re-chunking resynchronises at the first
                    // reproduced boundary and the remaining children pass
                    // back into the fast path above.
                    let n = read_node(store, &node.hash, sub_level, Some(&node.last_key), None)?;
                    let Node::Inner(refs) = n else {
                        unreachable!("level > 0 checked by read_node")
                    };
                    let last_idx = refs.len() - 1;
                    for (j, r) in refs.into_iter().enumerate().rev() {
                        let child_final = is_final && j == last_idx;
                        let child_level = sub_level - 1;
                        let it = if child_level == level {
                            Item::Old {
                                node: r,
                                is_final: child_final,
                            }
                        } else {
                            Item::Subtree {
                                node: r,
                                level: child_level,
                                is_final: child_final,
                            }
                        };
                        queue.push_front(it);
                    }
                }
            }
        }
    }
    if b.count > 0 {
        b.emit(store)?;
    }
    if b.out.is_empty() && level == 0 {
        // Empty map: a single empty leaf is the canonical root.
        b.emit(store)?;
    }
    Ok(b.out)
}

/// Turn one level's outputs into the next level's item stream, re-forming
/// opened parents verbatim where their exact child run survived.
///
/// `opened` is the key-ordered list of level-`next_level` nodes read during
/// descent. A parent is re-attached as [`Item::Old`] when the outputs
/// contain precisely its children, consecutively and in order — hash
/// equality is content equality, so this reproduces the parent's exact
/// bytes. (Within one level all chunk hashes are distinct — equal content
/// would mean overlapping key ranges — so matching is unambiguous.)
fn promote(outputs: Vec<OutItem>, opened: &[OpenedInner], next_level: u8) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let mut fresh: Vec<Entry> = Vec::new();
    let flush = |fresh: &mut Vec<Entry>, items: &mut Vec<Item>| {
        if !fresh.is_empty() {
            items.push(Item::Fresh(std::mem::take(fresh)));
        }
    };

    let mut p = 0usize; // next unconsidered opened parent
    let mut i = 0usize;
    while i < outputs.len() {
        match &outputs[i] {
            OutItem::Subtree {
                node,
                level,
                is_final,
            } => {
                flush(&mut fresh, &mut items);
                if *level == next_level {
                    items.push(Item::Old {
                        node: node.clone(),
                        is_final: *is_final,
                    });
                } else {
                    items.push(Item::Subtree {
                        node: node.clone(),
                        level: *level,
                        is_final: *is_final,
                    });
                }
                i += 1;
            }
            OutItem::Chunk(r) => {
                // Try to match a run of outputs against the next opened
                // parent's child list.
                let matched = loop {
                    let Some(cand) = opened.get(p) else {
                        break None;
                    };
                    if cand.children.first() == Some(&r.hash) {
                        let k = cand.children.len();
                        let full_run = outputs.len() - i >= k
                            && (0..k).all(|j| {
                                matches!(&outputs[i + j],
                                    OutItem::Chunk(c) if c.hash == cand.children[j])
                            });
                        if full_run {
                            break Some(k);
                        }
                    }
                    // The candidate's span ends at or before this chunk's
                    // last key, so it can never match a later output run;
                    // otherwise it may still match ahead — stop here.
                    if cand.node.last_key <= r.last_key {
                        p += 1;
                        continue;
                    }
                    break None;
                };
                if let Some(k) = matched {
                    flush(&mut fresh, &mut items);
                    items.push(Item::Old {
                        node: opened[p].node.clone(),
                        is_final: opened[p].is_final,
                    });
                    p += 1;
                    i += k;
                } else {
                    fresh.push(Entry::Inner {
                        last_key: r.last_key.clone(),
                        hash: r.hash,
                    });
                    i += 1;
                }
            }
        }
    }
    flush(&mut fresh, &mut items);
    items
}

/// Chunk level after level until a single root remains.
fn build_levels<S: ChunkStore>(
    store: &S,
    params: ChunkParams,
    mut items: Vec<Item>,
    opened: &[Vec<OpenedInner>],
) -> Result<Root, ProllyError> {
    let mut level: u8 = 0;
    loop {
        let mut outputs = chunk_level(store, level, params, items)?;
        if outputs.len() == 1 {
            return Ok(match outputs.remove(0) {
                OutItem::Chunk(r) => Root {
                    hash: r.hash,
                    height: u32::from(level) + 1,
                    params,
                },
                OutItem::Subtree {
                    node,
                    level: sub_level,
                    ..
                } => Root {
                    hash: node.hash,
                    height: u32::from(sub_level) + 1,
                    params,
                },
            });
        }
        if u32::from(level) + 1 >= MAX_HEIGHT {
            // Unreachable with sane parameters (fan-out keeps height in
            // single digits for any realistic map), but bounds the loop.
            return Err(ProllyError::InvalidRoot {
                reason: format!("tree exceeds the maximum height of {MAX_HEIGHT}"),
            });
        }
        level += 1;
        let opened_here = opened
            .get(usize::from(level))
            .map_or(&[][..], Vec::as_slice);
        items = promote(outputs, opened_here, level);
    }
}

/// Create a map from an unsorted iterator of key/value pairs (duplicate
/// keys: last one wins) and return its root.
///
/// The chunk parameters are format-defining (spec §3.2: fixed at repository
/// init, recorded in the manifest); the same parameters must be passed for
/// every operation on the same map.
pub fn bulk_load<S: ChunkStore>(
    store: &S,
    params: ChunkParams,
    entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
) -> Result<Root, ProllyError> {
    let mut sorted: Vec<(Vec<u8>, Vec<u8>)> = entries.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut deduped: Vec<Entry> = Vec::with_capacity(sorted.len());
    for (key, value) in sorted {
        let value = Bytes::from(value);
        match deduped.last_mut() {
            Some(Entry::Leaf { key: k, value: v }) if k.as_ref() == key.as_slice() => *v = value,
            _ => deduped.push(Entry::Leaf {
                key: Bytes::from(key),
                value,
            }),
        }
    }
    build_levels(store, params, vec![Item::Fresh(deduped)], &[])
}

/// The canonical empty map: a single empty leaf chunk.
pub fn empty<S: ChunkStore>(store: &S, params: ChunkParams) -> Result<Root, ProllyError> {
    bulk_load(store, params, Vec::new())
}

/// Walker producing the leaf-level item stream for a batch: it follows
/// only the root→leaf paths whose key ranges intersect the ops.
struct Descent<'s, S> {
    store: &'s S,
    items: Vec<Item>,
    /// Internal nodes read on the way down, indexed by level.
    opened: Vec<Vec<OpenedInner>>,
}

impl<S: ChunkStore> Descent<'_, S> {
    /// Visit the node at `hash`, emitting merged fresh entries where ops
    /// landed and untouched leaves / whole untouched subtrees elsewhere.
    fn descend(
        &mut self,
        hash: &Hash,
        level: u8,
        expect_last_key: Option<&[u8]>,
        min_key_exclusive: Option<&[u8]>,
        is_final: bool,
        ops: &[BatchOp],
    ) -> Result<(), ProllyError> {
        let node = read_node(self.store, hash, level, expect_last_key, min_key_exclusive)?;
        match node {
            Node::Leaf(entries) => {
                self.items.push(Item::Fresh(merge_ops(entries, ops)));
                Ok(())
            }
            Node::Inner(refs) => {
                self.opened[usize::from(level)].push(OpenedInner {
                    node: NodeRef {
                        last_key: refs
                            .last()
                            .expect("decode rejects empty inner nodes")
                            .last_key
                            .clone(),
                        hash: *hash,
                    },
                    is_final,
                    children: refs.iter().map(|r| r.hash).collect(),
                });
                let last_idx = refs.len() - 1;
                let mut op_idx = 0usize;
                let mut prev_last_key: Option<Bytes> = None;
                for (i, child) in refs.into_iter().enumerate() {
                    let is_last = i == last_idx;
                    let child_final = is_final && is_last;
                    let child_level = level - 1;
                    let start = op_idx;
                    while op_idx < ops.len()
                        && (is_last || ops[op_idx].key() <= child.last_key.as_ref())
                    {
                        op_idx += 1;
                    }
                    let lower = prev_last_key.as_deref().or(min_key_exclusive);
                    if start == op_idx {
                        // Untouched: carry the reference, do not read it.
                        let item = if child_level == 0 {
                            Item::Old {
                                node: child.clone(),
                                is_final: child_final,
                            }
                        } else {
                            Item::Subtree {
                                node: child.clone(),
                                level: child_level,
                                is_final: child_final,
                            }
                        };
                        self.items.push(item);
                    } else {
                        self.descend(
                            &child.hash,
                            child_level,
                            Some(&child.last_key),
                            lower,
                            child_final,
                            &ops[start..op_idx],
                        )?;
                    }
                    prev_last_key = Some(child.last_key);
                }
                Ok(())
            }
        }
    }
}

/// Merge a sorted leaf-entry list with a sorted, deduplicated op slice.
fn merge_ops(old: Vec<(Bytes, Bytes)>, ops: &[BatchOp]) -> Vec<Entry> {
    let mut out = Vec::with_capacity(old.len() + ops.len());
    let mut old_it = old.into_iter().peekable();
    let mut op_it = ops.iter().peekable();
    loop {
        match (old_it.peek(), op_it.peek()) {
            (Some((ok, _)), Some(op)) => match ok.as_ref().cmp(op.key()) {
                std::cmp::Ordering::Less => {
                    let (key, value) = old_it.next().expect("peeked");
                    out.push(Entry::Leaf { key, value });
                }
                std::cmp::Ordering::Equal => {
                    old_it.next();
                    apply_op(op_it.next().expect("peeked"), &mut out);
                }
                std::cmp::Ordering::Greater => {
                    apply_op(op_it.next().expect("peeked"), &mut out);
                }
            },
            (Some(_), None) => {
                let (key, value) = old_it.next().expect("peeked");
                out.push(Entry::Leaf { key, value });
            }
            (None, Some(_)) => apply_op(op_it.next().expect("peeked"), &mut out),
            (None, None) => break,
        }
    }
    out
}

fn apply_op(op: &BatchOp, out: &mut Vec<Entry>) {
    match op {
        BatchOp::Put(key, value) => out.push(Entry::Leaf {
            key: Bytes::copy_from_slice(key),
            value: Bytes::copy_from_slice(value),
        }),
        BatchOp::Delete(_) => {}
    }
}

/// Apply a batch of puts/deletes (duplicate keys: last one wins; deleting
/// an absent key is a no-op) and return the new root.
///
/// Only the root→leaf paths intersecting the batch are loaded, and only
/// the affected region of each level is re-chunked; the result is
/// bit-identical to [`bulk_load`] of the final contents (the
/// history-independence invariant, property-tested).
pub fn apply_batch<S: ChunkStore>(
    store: &S,
    root: &Root,
    ops: impl IntoIterator<Item = BatchOp>,
) -> Result<Root, ProllyError> {
    let mut sorted: Vec<BatchOp> = ops.into_iter().collect();
    // Stable sort preserves submission order between equal keys; the
    // dedupe below then keeps the last op for each key.
    sorted.sort_by(|a, b| a.key().cmp(b.key()));
    let mut ops: Vec<BatchOp> = Vec::with_capacity(sorted.len());
    for op in sorted {
        match ops.last_mut() {
            Some(last) if last.key() == op.key() => *last = op,
            _ => ops.push(op),
        }
    }
    if ops.is_empty() {
        return Ok(root.clone());
    }

    let mut descent = Descent {
        store,
        items: Vec::new(),
        opened: (0..root.height).map(|_| Vec::new()).collect(),
    };
    descent.descend(&root.hash, root.top_level(), None, None, true, &ops)?;
    build_levels(store, root.params, descent.items, &descent.opened)
}

/// Point lookup, loading only the root→leaf path for `key`.
pub fn get<S: ChunkStore>(
    store: &S,
    root: &Root,
    key: &[u8],
) -> Result<Option<Bytes>, ProllyError> {
    let mut hash = root.hash;
    let mut level = root.top_level();
    let mut expect_last_key: Option<Bytes> = None;
    let mut min_key_exclusive: Option<Bytes> = None;
    loop {
        let node = read_node(
            store,
            &hash,
            level,
            expect_last_key.as_deref(),
            min_key_exclusive.as_deref(),
        )?;
        match node {
            Node::Inner(refs) => {
                let idx = refs.partition_point(|r| r.last_key.as_ref() < key);
                if idx == refs.len() {
                    return Ok(None);
                }
                if idx > 0 {
                    min_key_exclusive = Some(refs[idx - 1].last_key.clone());
                }
                hash = refs[idx].hash;
                expect_last_key = Some(refs[idx].last_key.clone());
                level -= 1;
            }
            Node::Leaf(entries) => {
                return Ok(entries
                    .binary_search_by(|(k, _)| k.as_ref().cmp(key))
                    .ok()
                    .map(|i| entries[i].1.clone()));
            }
        }
    }
}
