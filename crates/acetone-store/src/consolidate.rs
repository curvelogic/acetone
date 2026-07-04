//! Periodic garbage-collecting consolidation: acetone's own `gc` (ADR-0011,
//! bead acetone-63m.13).
//!
//! Per-commit writes land as loose objects (git-clean, cheap). The retention
//! win — recovering the ~7.1× that git's own pack heuristics leave on the
//! table for content-addressed chunks — comes from periodically rewriting the
//! reachable object set into **one self-contained pack** whose entries are
//! REF_DELTAs against acetone's hand-chosen predecessors
//! ([`GitStore::record_base_hints`]).
//!
//! # Representation-only (normative)
//!
//! Consolidation never changes any object's bytes, so every object ID — and
//! therefore every prolly-tree root hash and the invariants above it — is
//! preserved exactly. Two mechanisms make this robust rather than merely
//! intended: the pack writer validates every delta against the true object
//! bytes before emitting it ([`crate::pack`]), and pruning deletes a stored
//! representation only after confirming the object is in the freshly written
//! pack. A wrong or stale base hint can only cost delta quality, never
//! correctness.
//!
//! # No git subprocess
//!
//! The store opens repositories reduced-trust with gix's process-spawning
//! features disabled, so consolidation shells out to nothing: it builds the
//! pack and a native v2 index itself and writes them straight into
//! `objects/pack`. Bases are only chosen from within the packed set, so the
//! pack is self-contained and needs no `git index-pack --fix-thin` pass.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;

use gix::ObjectId;

use crate::error::StoreError;
use crate::git::GitStore;
use crate::hash::Hash;
use crate::pack::{self, PackEntry};

/// Whole anchor cadence: a delta chain is broken with a whole object every
/// this many links, bounding the cost of a point read (ADR-0011). The
/// validation note's depth-100 chains read correctly but are the extreme.
const MAX_CHAIN: usize = 32;

/// Sidecar file (under the common git dir) recording `(new, base)` predecessor
/// hints for consolidation. A local optimisation cache, not a ref and not
/// transferred by clone/push; losing it costs delta quality, never
/// correctness.
const HINTS_FILE: &str = "acetone-pack-bases";

/// Sidecar file listing the pack stems consolidation has written, so a later
/// run can supersede them.
const PACKS_FILE: &str = "acetone-consolidation-packs";

/// Tuning for [`GitStore::consolidate`].
///
/// `#[non_exhaustive]`: build with [`ConsolidateOptions::default`] and assign
/// fields, so new knobs never break callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConsolidateOptions {
    /// Delete loose object files once they are safely in the new pack.
    pub prune_loose: bool,
    /// Delete earlier consolidation packs this store wrote, once every one of
    /// their objects is present in the new pack.
    pub prune_superseded_packs: bool,
}

impl Default for ConsolidateOptions {
    fn default() -> Self {
        ConsolidateOptions {
            prune_loose: true,
            prune_superseded_packs: true,
        }
    }
}

/// What one [`GitStore::consolidate`] run did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsolidateStats {
    /// Reachable objects written into the pack.
    pub objects: usize,
    /// Objects stored as REF_DELTAs against a chosen base.
    pub deltas: usize,
    /// Objects stored whole.
    pub whole: usize,
    /// Size of the written pack in bytes (excluding the index).
    pub pack_bytes: u64,
    /// Loose object files deleted.
    pub pruned_loose: usize,
    /// Prior consolidation packs deleted.
    pub pruned_packs: usize,
}

/// A topological emission plan: the order to write objects in (every base
/// before its dependents) and each object's chosen delta base.
struct Plan {
    order: Vec<ObjectId>,
    base: HashMap<ObjectId, ObjectId>,
}

impl GitStore {
    /// Record predecessor hints `(new_chunk, base)` for a future
    /// [`consolidate`](GitStore::consolidate).
    ///
    /// These come from the prolly layer, which knows each rewritten chunk's
    /// predecessor at write time (bead acetone-63m.13). They are appended to a
    /// **local** sidecar cache — not a ref, never transferred — so losing them
    /// only makes a later consolidation store more objects whole. Idempotent
    /// in effect: consolidation keeps the last base recorded for each new
    /// object and ignores hints whose endpoints are not both reachable.
    pub fn record_base_hints(&self, hints: &[(Hash, Hash)]) -> Result<(), StoreError> {
        if hints.is_empty() {
            return Ok(());
        }
        let path = self.sidecar_path(HINTS_FILE);
        let mut buf = String::with_capacity(hints.len() * 84);
        for (new, base) in hints {
            if new == base {
                continue; // a self-hint would only ever be a no-op
            }
            buf.push_str(&new.to_hex());
            buf.push(' ');
            buf.push_str(&base.to_hex());
            buf.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StoreError::backend("opening base-hint log", e))?;
        file.write_all(buf.as_bytes())
            .map_err(|e| StoreError::backend("appending base-hint log", e))?;
        Ok(())
    }

    /// Rewrite the reachable object set into one self-contained pack whose
    /// entries are REF_DELTAs against the recorded predecessor hints, then
    /// prune the superseded loose objects and prior consolidation packs.
    ///
    /// Representation-only: every object's bytes, and therefore its address,
    /// are preserved exactly (see the module docs). Safe to call at any time;
    /// safe to interrupt (a partial run leaves the old representations intact
    /// because pruning happens only after the new pack is durably written).
    pub fn consolidate(&self, options: ConsolidateOptions) -> Result<ConsolidateStats, StoreError> {
        let reachable = self.reachable_objects()?;
        let present: HashSet<ObjectId> = reachable.iter().map(|(oid, _)| *oid).collect();
        let hints = self.load_hints(&present)?;
        let plan = Plan::build(&reachable, &hints);

        // Stream object reads into the pack in plan order; capture the first
        // read error so a bad object surfaces as itself rather than as a
        // count mismatch.
        let mut read_error: Option<StoreError> = None;
        let reader = EntryReader {
            store: self,
            order: plan.order.iter(),
            base: &plan.base,
            error: &mut read_error,
        };
        let count = plan.order.len();
        let pack = pack::write_pack(self.object_hash(), count, reader)
            .map_err(|e| StoreError::corrupt("consolidation pack", e))?;
        if let Some(err) = read_error {
            return Err(err);
        }
        let idx = pack::write_idx(self.object_hash(), &pack)
            .map_err(|e| StoreError::corrupt("consolidation index", e))?;

        let stem = format!("pack-{}", pack.trailer);
        self.install_pack(&stem, &pack.bytes, &idx)?;

        let packed: HashSet<ObjectId> = present.iter().copied().collect();
        let pruned_loose = if options.prune_loose {
            self.prune_loose(&packed)?
        } else {
            0
        };
        let pruned_packs = if options.prune_superseded_packs {
            self.supersede_packs(&stem, &packed)?
        } else {
            self.record_pack_stem(&stem)?;
            0
        };

        Ok(ConsolidateStats {
            objects: count,
            deltas: pack.deltas,
            whole: pack.whole,
            pack_bytes: pack.bytes.len() as u64,
            pruned_loose,
            pruned_packs,
        })
    }

    /// Enumerate every object reachable from the repository's refs, with its
    /// git kind. Walks the object graph iteratively (commits → tree + parents,
    /// trees → entries, tags → target) using only object decoding, so it needs
    /// no extra gix features and cannot overflow the stack on a hostile deep
    /// tree.
    fn reachable_objects(&self) -> Result<Vec<(ObjectId, gix::object::Kind)>, StoreError> {
        use gix::object::Kind;

        let mut stack: Vec<ObjectId> = Vec::new();
        let references = self
            .repo()
            .references()
            .map_err(|e| StoreError::backend("listing refs for consolidation", e))?;
        let all = references
            .all()
            .map_err(|e| StoreError::backend("iterating refs for consolidation", e))?;
        for reference in all {
            let mut reference = reference
                .map_err(|e| StoreError::corrupt("ref for consolidation", e.to_string()))?;
            // Resolve symbolic chains to the object the ref names (without
            // peeling annotated tags — the tag object is itself reachable).
            // A dangling ref anchors nothing; skip it rather than fail the gc.
            if let Ok(id) = reference.follow_to_object() {
                stack.push(id.detach());
            }
        }

        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut out: Vec<(ObjectId, Kind)> = Vec::new();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let hash = Hash::from_oid(oid);
            let Some((kind, data)) = self.read_any_capped(&hash)? else {
                // A ref target or tag reference that is not actually present.
                // Nothing to pack; leave it out (git would report it broken,
                // but consolidation must not panic on a damaged repository).
                continue;
            };
            match kind {
                Kind::Commit => {
                    let commit =
                        gix::objs::CommitRef::from_bytes(&data, oid.kind()).map_err(|e| {
                            StoreError::corrupt("commit for consolidation", e.to_string())
                        })?;
                    stack.push(commit.tree());
                    stack.extend(commit.parents());
                }
                Kind::Tree => {
                    let tree = gix::objs::TreeRef::from_bytes(&data, oid.kind()).map_err(|e| {
                        StoreError::corrupt("tree for consolidation", e.to_string())
                    })?;
                    for entry in tree.entries {
                        stack.push(entry.oid.to_owned());
                    }
                }
                Kind::Tag => {
                    let tag = gix::objs::TagRef::from_bytes(&data, oid.kind())
                        .map_err(|e| StoreError::corrupt("tag for consolidation", e.to_string()))?;
                    stack.push(tag.target());
                }
                Kind::Blob => {}
            }
            out.push((oid, kind));
        }
        Ok(out)
    }

    /// Read the base-hint sidecar, keeping only hints whose endpoints are both
    /// reachable (a base outside the packed set could not be a self-contained
    /// pack's delta base). Last hint wins per new object.
    fn load_hints(
        &self,
        present: &HashSet<ObjectId>,
    ) -> Result<HashMap<ObjectId, ObjectId>, StoreError> {
        let path = self.sidecar_path(HINTS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => return Err(StoreError::backend("reading base-hint log", e)),
        };
        let mut hints: HashMap<ObjectId, ObjectId> = HashMap::new();
        for line in text.lines() {
            // Parse defensively: a torn final line or stray content is skipped,
            // never fatal.
            let Some((new_hex, base_hex)) = line.split_once(' ') else {
                continue;
            };
            let (Ok(new), Ok(base)) = (Hash::from_hex(new_hex), Hash::from_hex(base_hex)) else {
                continue;
            };
            let (new, base) = (new.oid(), base.oid());
            if new != base && present.contains(&new) && present.contains(&base) {
                hints.insert(new, base);
            }
        }
        Ok(hints)
    }

    /// Write a pack and its index into `objects/pack`.
    ///
    /// git treats a pack as present only once its `.idx` exists, and then
    /// requires the `.pack`. So the `.pack` is written in full first (a pack
    /// with no index is simply ignored, and pruning happens only after this
    /// returns, so the objects remain available as loose), and the index is
    /// published last via an atomic rename — a reader therefore never sees a
    /// dangling index or a torn one, whatever moment a crash lands on.
    fn install_pack(&self, stem: &str, pack: &[u8], idx: &[u8]) -> Result<(), StoreError> {
        let dir = self.repo().common_dir().join("objects").join("pack");
        std::fs::create_dir_all(&dir)
            .map_err(|e| StoreError::backend("creating objects/pack", e))?;
        std::fs::write(dir.join(format!("{stem}.pack")), pack)
            .map_err(|e| StoreError::backend("writing pack", e))?;
        let idx_tmp = dir.join(format!("{stem}.idx.tmp"));
        std::fs::write(&idx_tmp, idx).map_err(|e| StoreError::backend("writing pack index", e))?;
        std::fs::rename(&idx_tmp, dir.join(format!("{stem}.idx")))
            .map_err(|e| StoreError::backend("publishing pack index", e))?;
        Ok(())
    }

    /// Delete loose object files for objects now in the new pack. Gated on
    /// membership in `packed`, so nothing is deleted that was not preserved.
    fn prune_loose(&self, packed: &HashSet<ObjectId>) -> Result<usize, StoreError> {
        let objects_dir = self.repo().common_dir().join("objects");
        let mut pruned = 0usize;
        for oid in packed {
            let hex = oid.to_string();
            let (shard, rest) = hex.split_at(2);
            let path = objects_dir.join(shard).join(rest);
            match std::fs::remove_file(&path) {
                Ok(()) => pruned += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::backend("pruning loose object", e)),
            }
        }
        Ok(pruned)
    }

    /// Delete prior consolidation packs whose every object is present in the
    /// new pack, then record the new pack as the current one. The all-objects
    /// gate means a prior pack is removed only once everything it held is
    /// safely re-stored.
    fn supersede_packs(
        &self,
        new_stem: &str,
        packed: &HashSet<ObjectId>,
    ) -> Result<usize, StoreError> {
        let dir = self.repo().common_dir().join("objects").join("pack");
        let prior = self.read_pack_stems()?;
        let mut pruned = 0usize;
        for stem in &prior {
            if stem == new_stem {
                continue;
            }
            let idx_path = dir.join(format!("{stem}.idx"));
            let contained = match std::fs::read(&idx_path) {
                Ok(bytes) => idx_oids(&bytes, self.object_hash())?
                    .into_iter()
                    .all(|oid| packed.contains(&oid)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true, // already gone
                Err(e) => return Err(StoreError::backend("reading prior pack index", e)),
            };
            if contained {
                remove_if_present(&dir.join(format!("{stem}.pack")))?;
                remove_if_present(&idx_path)?;
                pruned += 1;
            }
        }
        self.write_pack_stems(&[new_stem.to_owned()])?;
        Ok(pruned)
    }

    /// Append a pack stem to the sidecar list (when pack pruning is off, we
    /// still track our packs so a later run can supersede them).
    fn record_pack_stem(&self, stem: &str) -> Result<(), StoreError> {
        let mut stems = self.read_pack_stems()?;
        if !stems.iter().any(|s| s == stem) {
            stems.push(stem.to_owned());
        }
        self.write_pack_stems(&stems)
    }

    fn read_pack_stems(&self) -> Result<Vec<String>, StoreError> {
        let path = self.sidecar_path(PACKS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(t) => Ok(t
                .lines()
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(StoreError::backend("reading consolidation-pack list", e)),
        }
    }

    fn write_pack_stems(&self, stems: &[String]) -> Result<(), StoreError> {
        let path = self.sidecar_path(PACKS_FILE);
        let body = stems.join("\n");
        std::fs::write(&path, body)
            .map_err(|e| StoreError::backend("writing consolidation-pack list", e))
    }

    fn sidecar_path(&self, name: &str) -> PathBuf {
        self.repo().common_dir().join(name)
    }
}

/// Streams objects into the pack writer in plan order, reading each object
/// (and its base) on demand so memory stays bounded. The first read error is
/// stashed and stops the stream; the caller surfaces it.
struct EntryReader<'a> {
    store: &'a GitStore,
    order: std::slice::Iter<'a, ObjectId>,
    base: &'a HashMap<ObjectId, ObjectId>,
    error: &'a mut Option<StoreError>,
}

impl Iterator for EntryReader<'_> {
    type Item = PackEntry;

    fn next(&mut self) -> Option<PackEntry> {
        if self.error.is_some() {
            return None;
        }
        let oid = *self.order.next()?;
        match self.read_entry(oid) {
            Ok(entry) => Some(entry),
            Err(e) => {
                *self.error = Some(e);
                None
            }
        }
    }
}

impl EntryReader<'_> {
    fn read_entry(&self, oid: ObjectId) -> Result<PackEntry, StoreError> {
        let (kind, data) = self
            .store
            .read_any_capped(&Hash::from_oid(oid))?
            .ok_or_else(|| {
                StoreError::corrupt(
                    "consolidation",
                    "reachable object vanished mid-consolidation",
                )
            })?;
        let base = match self.base.get(&oid) {
            Some(boid) => {
                let (_, bdata) = self
                    .store
                    .read_any_capped(&Hash::from_oid(*boid))?
                    .ok_or_else(|| {
                        StoreError::corrupt("consolidation", "chosen delta base vanished")
                    })?;
                Some((*boid, bdata))
            }
            None => None,
        };
        Ok(PackEntry {
            oid,
            kind,
            data,
            base,
        })
    }
}

impl Plan {
    /// Order `objects` so every chosen base precedes its dependents, choosing
    /// each object's base from `hints` (restricted to objects present here).
    /// Cycles and chains longer than [`MAX_CHAIN`] are broken by dropping the
    /// base (the object is then stored whole). `objects` is a set, so the
    /// duplicate-OID hazard from the streaming spike (a chunk OID reappearing
    /// after a boundary shift) collapses to a single node here.
    fn build(
        objects: &[(ObjectId, gix::object::Kind)],
        hints: &HashMap<ObjectId, ObjectId>,
    ) -> Plan {
        let present: HashSet<ObjectId> = objects.iter().map(|(oid, _)| *oid).collect();

        // Provisional base: the hint, if its endpoint is present and distinct.
        let mut base: HashMap<ObjectId, ObjectId> = HashMap::new();
        for (oid, _) in objects {
            if let Some(b) = hints.get(oid)
                && *b != *oid
                && present.contains(b)
            {
                base.insert(*oid, *b);
            }
        }

        // Break cycles: follow base pointers colouring the current path; a
        // back-edge to a node in the path drops that node's base. Iterative to
        // stay safe on long chains.
        let mut colour: HashMap<ObjectId, u8> = HashMap::new(); // 0 unseen,1 in-path,2 done
        for (start, _) in objects {
            if colour.get(start).copied().unwrap_or(0) != 0 {
                continue;
            }
            let mut path: Vec<ObjectId> = Vec::new();
            let mut node = *start;
            loop {
                match colour.get(&node).copied().unwrap_or(0) {
                    1 => {
                        // Back-edge: `node` is on the current path, so the edge
                        // that led here closes a cycle — drop it.
                        if let Some(&prev) = path.last() {
                            base.remove(&prev);
                        }
                        break;
                    }
                    2 => break, // joined an already-resolved chain
                    _ => {}
                }
                colour.insert(node, 1);
                path.push(node);
                match base.get(&node) {
                    Some(&next) => node = next,
                    None => break,
                }
            }
            for n in path {
                colour.insert(n, 2);
            }
        }

        // Cap chain length: walking each object down to its (post-cycle-break)
        // root, force a whole object every MAX_CHAIN links. Memoised depth.
        let mut depth: HashMap<ObjectId, usize> = HashMap::new();
        // Deterministic pass over a sorted view so capping is reproducible.
        let mut sorted: Vec<ObjectId> = present.iter().copied().collect();
        sorted.sort();
        for oid in &sorted {
            resolve_depth(*oid, &mut base, &mut depth);
        }

        // Emit bases-first: roots (no base) in sorted order, then dependents
        // as their base becomes available (Kahn over the forest).
        let mut dependents: BTreeMap<ObjectId, Vec<ObjectId>> = BTreeMap::new();
        let mut roots: Vec<ObjectId> = Vec::new();
        for oid in &sorted {
            match base.get(oid) {
                Some(b) => dependents.entry(*b).or_default().push(*oid),
                None => roots.push(*oid),
            }
        }
        for children in dependents.values_mut() {
            children.sort();
        }
        let mut order: Vec<ObjectId> = Vec::with_capacity(sorted.len());
        let mut queue: std::collections::VecDeque<ObjectId> = roots.into_iter().collect();
        while let Some(oid) = queue.pop_front() {
            order.push(oid);
            if let Some(children) = dependents.get(&oid) {
                for child in children {
                    queue.push_back(*child);
                }
            }
        }
        debug_assert_eq!(order.len(), sorted.len(), "every object emitted once");

        Plan { order, base }
    }
}

/// Following `base` from `oid` to its root, capping chains at [`MAX_CHAIN`] by
/// clearing the base of every MAX_CHAIN-th link. Memoised in `depth`.
fn resolve_depth(
    oid: ObjectId,
    base: &mut HashMap<ObjectId, ObjectId>,
    depth: &mut HashMap<ObjectId, usize>,
) -> usize {
    if let Some(d) = depth.get(&oid) {
        return *d;
    }
    let d = match base.get(&oid).copied() {
        None => 0,
        Some(b) => {
            let bd = resolve_depth(b, base, depth);
            if bd + 1 >= MAX_CHAIN {
                base.remove(&oid); // whole anchor: reset the chain here
                0
            } else {
                bd + 1
            }
        }
    };
    depth.insert(oid, d);
    d
}

/// Parse the object IDs out of a v2 pack index (`\377tOc`, version 2). Used to
/// gate pack pruning on "every object is elsewhere". Returns a typed error on
/// anything malformed rather than panicking (indices are untrusted on a
/// hostile clone).
fn idx_oids(bytes: &[u8], hash_kind: gix::hash::Kind) -> Result<Vec<ObjectId>, StoreError> {
    let corrupt = |why: &str| StoreError::corrupt("pack index", why.to_string());
    let hash_len = hash_kind.len_in_bytes();
    if bytes.len() < 8 || bytes[0..4] != [0xff, b't', b'O', b'c'] || bytes[4..8] != [0, 0, 0, 2] {
        return Err(corrupt("not a version-2 index"));
    }
    let fanout_end = 8 + 256 * 4;
    if bytes.len() < fanout_end {
        return Err(corrupt("truncated fanout"));
    }
    let count = u32::from_be_bytes([
        bytes[fanout_end - 4],
        bytes[fanout_end - 3],
        bytes[fanout_end - 2],
        bytes[fanout_end - 1],
    ]) as usize;
    let table_end = fanout_end
        .checked_add(
            count
                .checked_mul(hash_len)
                .ok_or_else(|| corrupt("oid table overflow"))?,
        )
        .ok_or_else(|| corrupt("oid table overflow"))?;
    if bytes.len() < table_end {
        return Err(corrupt("truncated oid table"));
    }
    let mut oids = Vec::with_capacity(count);
    for i in 0..count {
        let start = fanout_end + i * hash_len;
        oids.push(
            ObjectId::try_from(&bytes[start..start + hash_len])
                .map_err(|_| corrupt("bad oid in table"))?,
        );
    }
    Ok(oids)
}

fn remove_if_present(path: &std::path::Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::backend("removing superseded pack file", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::object::Kind;

    /// A distinct blob OID per `n`, so tests can build hint graphs by number.
    fn oid(n: u16) -> ObjectId {
        gix::objs::compute_hash(gix::hash::Kind::Sha1, Kind::Blob, &n.to_be_bytes()).expect("hash")
    }

    fn objects(ns: &[u16]) -> Vec<(ObjectId, Kind)> {
        ns.iter().map(|n| (oid(*n), Kind::Blob)).collect()
    }

    fn hint_map(edges: &[(u16, u16)]) -> HashMap<ObjectId, ObjectId> {
        edges.iter().map(|(a, b)| (oid(*a), oid(*b))).collect()
    }

    /// Position of an OID in the emitted order.
    fn pos(order: &[ObjectId], target: ObjectId) -> usize {
        order.iter().position(|o| *o == target).expect("emitted")
    }

    /// Every chosen base must be emitted before the object that deltas against
    /// it — the invariant a self-contained pack depends on.
    fn assert_bases_first(plan: &Plan) {
        for (oid, base) in &plan.base {
            assert!(
                pos(&plan.order, *base) < pos(&plan.order, *oid),
                "base {base} must precede dependant {oid}"
            );
        }
    }

    #[test]
    fn chain_orders_bases_first_regardless_of_input_order() {
        // c deltas on b, b on a; feed the objects in the *reverse* order so
        // the plan must reorder to put a first.
        let objs = objects(&[3, 2, 1]);
        let hints = hint_map(&[(2, 1), (3, 2)]);
        let plan = Plan::build(&objs, &hints);
        assert_eq!(plan.order.len(), 3);
        assert_bases_first(&plan);
        assert!(pos(&plan.order, oid(1)) < pos(&plan.order, oid(2)));
        assert!(pos(&plan.order, oid(2)) < pos(&plan.order, oid(3)));
    }

    #[test]
    fn duplicate_oid_in_input_collapses_to_one_node() {
        // The streaming spike could see the same chunk OID twice (a boundary
        // shifting back); the set-based plan must still emit each once.
        let mut objs = objects(&[1, 2]);
        objs.push((oid(2), Kind::Blob)); // duplicate
        let hints = hint_map(&[(2, 1)]);
        let plan = Plan::build(&objs, &hints);
        assert_eq!(plan.order.len(), 2, "duplicate collapses");
        assert_bases_first(&plan);
    }

    #[test]
    fn a_hint_cycle_is_broken_and_every_object_emitted() {
        // a->b->a: one edge must be dropped so the pack has no delta cycle.
        let objs = objects(&[1, 2]);
        let hints = hint_map(&[(1, 2), (2, 1)]);
        let plan = Plan::build(&objs, &hints);
        assert_eq!(plan.order.len(), 2);
        assert_bases_first(&plan);
        assert_eq!(plan.base.len(), 1, "exactly one edge survives");
    }

    #[test]
    fn a_base_pointing_at_a_missing_object_is_dropped() {
        // Hint b->99 where 99 is not in the packed set: b must be stored whole.
        let objs = objects(&[1, 2]);
        let hints = hint_map(&[(2, 99)]);
        let plan = Plan::build(&objs, &hints);
        assert!(!plan.base.contains_key(&oid(2)), "unreachable base dropped");
        assert_bases_first(&plan);
    }

    #[test]
    fn long_chains_are_capped_with_whole_anchors() {
        // A chain longer than MAX_CHAIN: o0 <- o1 <- ... <- oN. After capping,
        // no surviving delta chain exceeds MAX_CHAIN links, so at least one
        // interior object is forced whole.
        let n = MAX_CHAIN as u16 + 5;
        let objs = objects(&(0..=n).collect::<Vec<_>>());
        let edges: Vec<(u16, u16)> = (1..=n).map(|i| (i, i - 1)).collect();
        let plan = Plan::build(&objs, &hint_map(&edges));
        assert_eq!(plan.order.len(), (n + 1) as usize);
        assert_bases_first(&plan);
        // Walk every object to its root; depth must never exceed MAX_CHAIN.
        for (oid, _) in &objs {
            let mut depth = 0usize;
            let mut cur = *oid;
            while let Some(b) = plan.base.get(&cur) {
                depth += 1;
                assert!(depth <= MAX_CHAIN, "chain exceeds cap at {oid}");
                cur = *b;
            }
        }
        assert!(
            plan.base.len() < n as usize,
            "capping must break at least one link"
        );
    }

    #[test]
    fn idx_oids_round_trips_our_own_index() {
        use crate::pack::{PackEntry, write_idx, write_pack};
        let kind = gix::hash::Kind::Sha1;
        let datas: [&[u8]; 3] = [b"alpha", b"beta-beta", b"gamma!!"];
        let entries: Vec<PackEntry> = datas
            .iter()
            .map(|d| PackEntry {
                oid: gix::objs::compute_hash(kind, Kind::Blob, d).expect("hash"),
                kind: Kind::Blob,
                data: d.to_vec(),
                base: None,
            })
            .collect();
        let mut expected: Vec<ObjectId> = entries.iter().map(|e| e.oid).collect();
        expected.sort();
        let pack = write_pack(kind, entries.len(), entries).expect("pack");
        let idx = write_idx(kind, &pack).expect("idx");
        assert_eq!(idx_oids(&idx, kind).expect("parse"), expected);
    }

    #[test]
    fn idx_oids_rejects_a_non_v2_index() {
        assert!(idx_oids(b"not an index", gix::hash::Kind::Sha1).is_err());
        assert!(idx_oids(&[], gix::hash::Kind::Sha1).is_err());
    }
}
