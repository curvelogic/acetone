//! Prolly-tree construction, lookup, scan and batch mutation over the git
//! object database.
//!
//! Layout: sorted key/value leaf chunks split at content-defined boundaries
//! (see `chunker`); internal nodes list `(last_key, child_oid)` entries and
//! are split by the same chunker over their serialised entry stream; the
//! address of every chunk IS its git blob OID (single addressing scheme —
//! Decision 1).
//!
//! History independence: every level's chunking is a pure function of that
//! level's serialised entry stream, which is itself a pure function of map
//! contents. `apply_batch` rebuilds only the affected region of each level
//! and splices back into existing chunks at the first re-synchronised
//! boundary, producing a tree bit-identical to `bulk_load` of the same
//! contents.
//!
//! Node encoding (deterministic):
//! ```text
//! node        := level:u8 count:u32be entry*
//! leaf entry  := klen:u32be key vlen:u32be value          (level 0)
//! inner entry := klen:u32be last_key olen:u8 oid_bytes    (level > 0)
//! ```

use std::collections::HashMap;
use std::ops::{Bound, RangeBounds};

use gix::ObjectId;

use crate::chunker::{ChunkParams, Chunker};
use crate::{BatchOp, Root, SpikeError, Store};

/// Reference to a child node: its OID plus the largest key reachable
/// beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeRef {
    pub last_key: Vec<u8>,
    pub oid: ObjectId,
}

/// An entry at some tree level, in memory.
#[derive(Debug, Clone)]
enum Entry {
    Leaf { key: Vec<u8>, value: Vec<u8> },
    Inner { last_key: Vec<u8>, oid: ObjectId },
}

impl Entry {
    fn key(&self) -> &[u8] {
        match self {
            Entry::Leaf { key, .. } => key,
            Entry::Inner { last_key, .. } => last_key,
        }
    }

    /// Append the deterministic serialisation of this entry.
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Entry::Leaf { key, value } => {
                out.extend_from_slice(&(key.len() as u32).to_be_bytes());
                out.extend_from_slice(key);
                out.extend_from_slice(&(value.len() as u32).to_be_bytes());
                out.extend_from_slice(value);
            }
            Entry::Inner { last_key, oid } => {
                out.extend_from_slice(&(last_key.len() as u32).to_be_bytes());
                out.extend_from_slice(last_key);
                let oid_bytes = oid.as_slice();
                out.push(oid_bytes.len() as u8);
                out.extend_from_slice(oid_bytes);
            }
        }
    }
}

/// A decoded node.
#[derive(Debug)]
pub(crate) enum Node {
    Leaf(Vec<(Vec<u8>, Vec<u8>)>),
    Inner(Vec<NodeRef>),
}

fn corrupt(msg: impl Into<String>) -> SpikeError {
    SpikeError::Corrupt(msg.into())
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], SpikeError> {
    if buf.len() < n {
        return Err(corrupt("truncated node"));
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

fn take_u32(buf: &mut &[u8]) -> Result<u32, SpikeError> {
    let b = take(buf, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub(crate) fn decode_node(data: &[u8]) -> Result<(u8, Node), SpikeError> {
    let mut buf = data;
    let level = take(&mut buf, 1)?[0];
    let count = take_u32(&mut buf)? as usize;
    if level == 0 {
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let klen = take_u32(&mut buf)? as usize;
            let key = take(&mut buf, klen)?.to_vec();
            let vlen = take_u32(&mut buf)? as usize;
            let value = take(&mut buf, vlen)?.to_vec();
            entries.push((key, value));
        }
        if !buf.is_empty() {
            return Err(corrupt("trailing bytes in leaf node"));
        }
        Ok((level, Node::Leaf(entries)))
    } else {
        let mut refs = Vec::with_capacity(count);
        for _ in 0..count {
            let klen = take_u32(&mut buf)? as usize;
            let last_key = take(&mut buf, klen)?.to_vec();
            let olen = take(&mut buf, 1)?[0] as usize;
            let oid = ObjectId::from_bytes_or_panic(take(&mut buf, olen)?);
            refs.push(NodeRef { last_key, oid });
        }
        if !buf.is_empty() {
            return Err(corrupt("trailing bytes in inner node"));
        }
        Ok((level, Node::Inner(refs)))
    }
}

/// Input to the level chunker: either an existing chunk that may be reused
/// verbatim, or a run of fresh entries to be (re-)chunked.
enum Segment {
    /// Index into the level's old node list.
    Reuse(usize),
    Fresh(Vec<Entry>),
}

/// One output chunk of a level, and — if it was reused byte-identically —
/// the index of the old node it is.
struct ChildOut {
    node: NodeRef,
    reused: Option<usize>,
}

/// Streaming builder for one level: buffers serialised entries, cuts at
/// content-defined boundaries, writes each chunk as a git blob.
struct LevelBuilder<'s> {
    store: &'s Store,
    level: u8,
    chunker: Chunker,
    buf: Vec<u8>,
    count: u32,
    last_key: Vec<u8>,
    out: Vec<ChildOut>,
}

impl<'s> LevelBuilder<'s> {
    fn new(store: &'s Store, level: u8, params: ChunkParams) -> Self {
        LevelBuilder {
            store,
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

    fn push_entry(&mut self, e: &Entry) -> Result<(), SpikeError> {
        let start = self.buf.len();
        e.encode(&mut self.buf);
        self.count += 1;
        self.last_key.clear();
        self.last_key.extend_from_slice(e.key());
        let encoded = self.buf[start..].to_vec();
        if self.chunker.feed_entry(&encoded) {
            self.emit()?;
        }
        Ok(())
    }

    fn emit(&mut self) -> Result<(), SpikeError> {
        let mut node = Vec::with_capacity(5 + self.buf.len());
        node.push(self.level);
        node.extend_from_slice(&self.count.to_be_bytes());
        node.extend_from_slice(&self.buf);
        let oid = self.store.write_chunk(&node)?;
        self.out.push(ChildOut {
            node: NodeRef {
                last_key: std::mem::take(&mut self.last_key),
                oid,
            },
            reused: None,
        });
        self.buf.clear();
        self.count = 0;
        self.chunker.reset();
        Ok(())
    }
}

impl Store {
    pub(crate) fn read_node(&self, oid: &ObjectId, expect_level: u8) -> Result<Node, SpikeError> {
        let data = self.read_chunk(oid)?;
        let (level, node) = decode_node(&data)?;
        if level != expect_level {
            return Err(corrupt(format!(
                "node {oid} has level {level}, expected {expect_level}"
            )));
        }
        Ok(node)
    }

    fn read_level_entries(&self, oid: &ObjectId, level: u8) -> Result<Vec<Entry>, SpikeError> {
        Ok(match self.read_node(oid, level)? {
            Node::Leaf(entries) => entries
                .into_iter()
                .map(|(key, value)| Entry::Leaf { key, value })
                .collect(),
            Node::Inner(refs) => refs
                .into_iter()
                .map(|r| Entry::Inner {
                    last_key: r.last_key,
                    oid: r.oid,
                })
                .collect(),
        })
    }

    /// Chunk one level from a segment stream. Reused segments are emitted
    /// verbatim when the builder sits at a chunk boundary (and the old
    /// chunk's own trailing boundary is reproducible — i.e. it was not the
    /// level's final chunk, whose cut may have been end-of-stream, unless it
    /// is final here too). Anything else is expanded to entries and
    /// re-chunked; because the chunker state resets at each boundary, the
    /// output is bit-identical to chunking the whole level from scratch.
    fn chunk_level(
        &self,
        level: u8,
        params: ChunkParams,
        old: &[NodeRef],
        segments: Vec<Segment>,
    ) -> Result<Vec<ChildOut>, SpikeError> {
        let mut b = LevelBuilder::new(self, level, params);
        let n = segments.len();
        for (si, seg) in segments.into_iter().enumerate() {
            match seg {
                Segment::Reuse(i) => {
                    let is_final_segment = si + 1 == n;
                    let old_cut_reproducible = i + 1 != old.len() || is_final_segment;
                    if b.at_boundary() && old_cut_reproducible {
                        b.out.push(ChildOut {
                            node: old[i].clone(),
                            reused: Some(i),
                        });
                    } else {
                        for e in self.read_level_entries(&old[i].oid, level)? {
                            b.push_entry(&e)?;
                        }
                    }
                }
                Segment::Fresh(entries) => {
                    for e in &entries {
                        b.push_entry(e)?;
                    }
                }
            }
        }
        if b.count > 0 {
            b.emit()?;
        }
        if b.out.is_empty() && level == 0 {
            // Empty map: a single empty leaf is the canonical root.
            b.emit()?;
        }
        if self.recording() {
            self.record_level_bases(old, &b.out);
        }
        Ok(b.out)
    }

    /// Pair freshly written chunks with the old chunks they replace, for
    /// pack-on-write delta bases (bead acetone-63m.10). Reused children fix
    /// exact positions in the old node list; between two reused anchors, the
    /// fresh new chunks and the skipped old chunks cover the same key span
    /// and are paired positionally (the overwhelmingly common case is one
    /// old chunk rewritten as one new chunk; extra new chunks share the last
    /// old chunk of the region, extra old chunks are dropped). Best-effort
    /// by design: an unpaired chunk is merely stored whole in the pack.
    fn record_level_bases(&self, old: &[NodeRef], children: &[ChildOut]) {
        let pair = |news: &[ObjectId], olds: &[NodeRef]| {
            if olds.is_empty() {
                return;
            }
            for (k, new) in news.iter().enumerate() {
                let base = &olds[k.min(olds.len() - 1)];
                self.record_base(*new, base.oid);
            }
        };
        let mut next_old = 0usize;
        let mut fresh: Vec<ObjectId> = Vec::new();
        for c in children {
            match c.reused {
                Some(i) => {
                    debug_assert!(i >= next_old, "reused indices are increasing");
                    pair(&fresh, &old[next_old.min(i)..i]);
                    fresh.clear();
                    next_old = i + 1;
                }
                None => fresh.push(c.node.oid),
            }
        }
        pair(&fresh, &old[next_old.min(old.len())..]);
    }

    /// Build the levels above the leaves until a single root remains.
    /// `old_levels`/`old_ranges` allow chunk reuse (empty for bulk loads).
    fn build_up(
        &self,
        params: ChunkParams,
        mut children: Vec<ChildOut>,
        old_levels: &[Vec<NodeRef>],
        old_ranges: &[Vec<(usize, usize)>],
    ) -> Result<Root, SpikeError> {
        let mut level: u32 = 1;
        while children.len() > 1 {
            let old: &[NodeRef] = old_levels
                .get(level as usize)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let ranges: &[(usize, usize)] = old_ranges
                .get(level as usize)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let segments = parent_segments(&children, ranges);
            children = self.chunk_level(level as u8, params, old, segments)?;
            level += 1;
        }
        Ok(Root {
            oid: children[0].node.oid,
            height: level,
            params,
        })
    }

    /// Create a map from an unsorted iterator of key/value pairs (duplicate
    /// keys: last one wins) and return its root. Uses the default chunking
    /// parameters.
    pub fn bulk_load(
        &self,
        entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    ) -> Result<Root, SpikeError> {
        self.bulk_load_with(ChunkParams::default(), entries)
    }

    /// `bulk_load` with explicit chunking parameters. Chunk parameters are
    /// format-defining (spec §3.2: fixed at init, recorded in the manifest);
    /// this entry point exists so the property suite can assert both halves
    /// of that — same parameters always agree, different parameters diverge.
    pub fn bulk_load_with(
        &self,
        params: ChunkParams,
        entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    ) -> Result<Root, SpikeError> {
        let mut sorted: Vec<(Vec<u8>, Vec<u8>)> = entries.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut deduped: Vec<Entry> = Vec::with_capacity(sorted.len());
        for (key, value) in sorted {
            match deduped.last_mut() {
                Some(Entry::Leaf { key: k, value: v }) if *k == key => *v = value,
                _ => deduped.push(Entry::Leaf { key, value }),
            }
        }
        let children = self.chunk_level(0, params, &[], vec![Segment::Fresh(deduped)])?;
        self.build_up(params, children, &[], &[])
    }

    /// Load the node lists of every level, top-down, without touching leaf
    /// blobs. `levels[l]` are the level-`l` nodes in key order; for `l >= 1`,
    /// `ranges[l][j]` is the `(start, len)` slice of `levels[l-1]` covered by
    /// node `j`.
    #[allow(clippy::type_complexity)]
    fn load_levels(
        &self,
        root: &Root,
    ) -> Result<(Vec<Vec<NodeRef>>, Vec<Vec<(usize, usize)>>), SpikeError> {
        let height = root.height as usize;
        let mut levels: Vec<Vec<NodeRef>> = vec![Vec::new(); height];
        let mut ranges: Vec<Vec<(usize, usize)>> = vec![Vec::new(); height];
        // The root's last_key is never used for routing; leave it empty.
        levels[height - 1].push(NodeRef {
            last_key: Vec::new(),
            oid: root.oid,
        });
        for l in (1..height).rev() {
            let parents = levels[l].clone();
            for parent in &parents {
                match self.read_node(&parent.oid, l as u8)? {
                    Node::Inner(refs) => {
                        ranges[l].push((levels[l - 1].len(), refs.len()));
                        levels[l - 1].extend(refs);
                    }
                    Node::Leaf(_) => unreachable!("level checked by read_node"),
                }
            }
        }
        Ok((levels, ranges))
    }

    /// Apply a batch of puts/deletes (duplicate keys: last one wins) and
    /// return the new root. Only the affected region of each level is
    /// re-chunked; the result is bit-identical to `bulk_load` of the final
    /// contents.
    pub fn apply_batch(
        &self,
        root: &Root,
        ops: impl IntoIterator<Item = BatchOp>,
    ) -> Result<Root, SpikeError> {
        let params = root.params;
        let mut sorted: Vec<BatchOp> = ops.into_iter().collect();
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

        let (levels, ranges) = self.load_levels(root)?;
        let leaves = &levels[0];

        // Leaf-level segments: untouched leaves are reusable; leaves whose
        // key range intersects the batch are merged with their ops.
        let mut segments: Vec<Segment> = Vec::new();
        let mut fresh: Vec<Entry> = Vec::new();
        let mut op_idx = 0usize;
        for (i, leaf) in leaves.iter().enumerate() {
            let is_last = i + 1 == leaves.len();
            let start = op_idx;
            while op_idx < ops.len() && (is_last || ops[op_idx].key() <= leaf.last_key.as_slice()) {
                op_idx += 1;
            }
            if start == op_idx {
                if !fresh.is_empty() {
                    segments.push(Segment::Fresh(std::mem::take(&mut fresh)));
                }
                segments.push(Segment::Reuse(i));
            } else {
                let old = match self.read_node(&leaf.oid, 0)? {
                    Node::Leaf(entries) => entries,
                    Node::Inner(_) => unreachable!("level checked by read_node"),
                };
                merge_ops(old, &ops[start..op_idx], &mut fresh);
            }
        }
        if !fresh.is_empty() {
            segments.push(Segment::Fresh(fresh));
        }

        let children = self.chunk_level(0, params, leaves, segments)?;
        self.build_up(params, children, &levels, &ranges)
    }

    /// Point lookup.
    pub fn get(&self, root: &Root, key: &[u8]) -> Result<Option<Vec<u8>>, SpikeError> {
        let mut oid = root.oid;
        let mut level = root.height - 1;
        loop {
            match self.read_node(&oid, level as u8)? {
                Node::Inner(refs) => {
                    let idx = refs.partition_point(|r| r.last_key.as_slice() < key);
                    if idx == refs.len() {
                        return Ok(None);
                    }
                    oid = refs[idx].oid;
                    level -= 1;
                }
                Node::Leaf(entries) => {
                    return Ok(entries
                        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                        .ok()
                        .map(|i| entries[i].1.clone()));
                }
            }
        }
    }

    /// Ordered forward scan over `range`.
    pub fn range_scan<'s, R: RangeBounds<Vec<u8>>>(
        &'s self,
        root: &Root,
        range: R,
    ) -> Result<Scan<'s>, SpikeError> {
        let end = clone_bound(range.end_bound());
        let mut scan = Scan {
            store: self,
            stack: Vec::new(),
            leaf: Vec::new(),
            leaf_idx: 0,
            end,
            done: false,
        };

        // Descend to the first leaf that can contain the start bound.
        let start = range.start_bound();
        let mut oid = root.oid;
        let mut level = root.height - 1;
        loop {
            match self.read_node(&oid, level as u8)? {
                Node::Inner(refs) => {
                    let idx = match start {
                        Bound::Included(k) => {
                            refs.partition_point(|r| r.last_key.as_slice() < k.as_slice())
                        }
                        Bound::Excluded(k) => {
                            refs.partition_point(|r| r.last_key.as_slice() <= k.as_slice())
                        }
                        Bound::Unbounded => 0,
                    };
                    if idx == refs.len() {
                        scan.done = true;
                        return Ok(scan);
                    }
                    oid = refs[idx].oid;
                    scan.stack.push((refs, idx));
                    level -= 1;
                }
                Node::Leaf(entries) => {
                    scan.leaf_idx = match start {
                        Bound::Included(k) => {
                            entries.partition_point(|(ek, _)| ek.as_slice() < k.as_slice())
                        }
                        Bound::Excluded(k) => {
                            entries.partition_point(|(ek, _)| ek.as_slice() <= k.as_slice())
                        }
                        Bound::Unbounded => 0,
                    };
                    scan.leaf = entries;
                    return Ok(scan);
                }
            }
        }
    }
}

fn clone_bound(b: Bound<&Vec<u8>>) -> Bound<Vec<u8>> {
    match b {
        Bound::Included(k) => Bound::Included(k.clone()),
        Bound::Excluded(k) => Bound::Excluded(k.clone()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// Merge a sorted leaf-entry list with a sorted, deduplicated op slice.
fn merge_ops(old: Vec<(Vec<u8>, Vec<u8>)>, ops: &[BatchOp], out: &mut Vec<Entry>) {
    let mut old_it = old.into_iter().peekable();
    let mut op_it = ops.iter().peekable();
    loop {
        match (old_it.peek(), op_it.peek()) {
            (Some((ok, _)), Some(op)) => match ok.as_slice().cmp(op.key()) {
                std::cmp::Ordering::Less => {
                    let (key, value) = old_it.next().expect("peeked");
                    out.push(Entry::Leaf { key, value });
                }
                std::cmp::Ordering::Equal => {
                    old_it.next();
                    apply_op(op_it.next().expect("peeked"), out);
                }
                std::cmp::Ordering::Greater => {
                    apply_op(op_it.next().expect("peeked"), out);
                }
            },
            (Some(_), None) => {
                let (key, value) = old_it.next().expect("peeked");
                out.push(Entry::Leaf { key, value });
            }
            (None, Some(_)) => apply_op(op_it.next().expect("peeked"), out),
            (None, None) => break,
        }
    }
}

fn apply_op(op: &BatchOp, out: &mut Vec<Entry>) {
    match op {
        BatchOp::Put(key, value) => out.push(Entry::Leaf {
            key: key.clone(),
            value: value.clone(),
        }),
        BatchOp::Delete(_) => {}
    }
}

/// Derive the parent level's segment stream from a chunked child level: a
/// run of reused children exactly covering one old parent's child range
/// makes that parent reusable; everything else becomes fresh
/// `(last_key, oid)` entries.
fn parent_segments(children: &[ChildOut], ranges: &[(usize, usize)]) -> Vec<Segment> {
    let start_map: HashMap<usize, usize> = ranges
        .iter()
        .enumerate()
        .map(|(p, (s, _))| (*s, p))
        .collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut fresh: Vec<Entry> = Vec::new();
    let mut i = 0usize;
    while i < children.len() {
        if let Some(a) = children[i].reused
            && let Some(&p) = start_map.get(&a)
        {
            let (s, len) = ranges[p];
            debug_assert_eq!(s, a);
            if len > 0
                && i + len <= children.len()
                && (0..len).all(|k| children[i + k].reused == Some(a + k))
            {
                if !fresh.is_empty() {
                    segments.push(Segment::Fresh(std::mem::take(&mut fresh)));
                }
                segments.push(Segment::Reuse(p));
                i += len;
                continue;
            }
        }
        fresh.push(Entry::Inner {
            last_key: children[i].node.last_key.clone(),
            oid: children[i].node.oid,
        });
        i += 1;
    }
    if !fresh.is_empty() {
        segments.push(Segment::Fresh(fresh));
    }
    segments
}

/// Ordered forward iterator over a key range. Yields `Err` once and stops
/// on storage corruption.
pub struct Scan<'s> {
    store: &'s Store,
    /// Inner-node path: (children of that node, index of the child we are in).
    stack: Vec<(Vec<NodeRef>, usize)>,
    leaf: Vec<(Vec<u8>, Vec<u8>)>,
    leaf_idx: usize,
    end: Bound<Vec<u8>>,
    done: bool,
}

impl Scan<'_> {
    fn advance_leaf(&mut self) -> Result<bool, SpikeError> {
        // Move to the next leaf, popping exhausted inner nodes.
        loop {
            let Some((refs, idx)) = self.stack.last_mut() else {
                return Ok(false);
            };
            *idx += 1;
            if *idx < refs.len() {
                break;
            }
            self.stack.pop();
        }
        // Descend leftmost to the next leaf, identifying the leaf level by
        // node type rather than depth arithmetic.
        let mut oid = {
            let (refs, idx) = self.stack.last().expect("non-empty");
            refs[*idx].oid
        };
        loop {
            let data = self.store.read_chunk(&oid)?;
            let (_, node) = decode_node(&data)?;
            match node {
                Node::Inner(refs) => {
                    if refs.is_empty() {
                        return Err(corrupt("empty inner node"));
                    }
                    oid = refs[0].oid;
                    self.stack.push((refs, 0));
                }
                Node::Leaf(entries) => {
                    self.leaf = entries;
                    self.leaf_idx = 0;
                    return Ok(true);
                }
            }
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>), SpikeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if self.leaf_idx < self.leaf.len() {
                let (k, v) = &self.leaf[self.leaf_idx];
                let in_range = match &self.end {
                    Bound::Included(e) => k <= e,
                    Bound::Excluded(e) => k < e,
                    Bound::Unbounded => true,
                };
                if !in_range {
                    self.done = true;
                    return None;
                }
                self.leaf_idx += 1;
                return Some(Ok((k.clone(), v.clone())));
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
