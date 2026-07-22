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
    /// are preserved exactly (see the module docs). Safe to interrupt — a
    /// partial run leaves the old representations intact, because pruning
    /// happens only after the new pack is durably `fsync`ed.
    ///
    /// Consolidations are **serialised** through a dedicated lock file
    /// (`<common-dir>/acetone-gc.lock`, the same `gix::lock` mechanism the ref
    /// writer uses) so two runs cannot race the sidecar pack-list
    /// read-modify-write; a second concurrent run backs off and then fails with
    /// [`StoreError::Backend`] rather than corrupting the tracking file. The
    /// lock is deliberately *not* the ref-write lock, so a long consolidation
    /// does not block commits; a commit racing a running consolidation is
    /// harmless — its new objects simply aren't in this run's pack (they stay
    /// loose and are never pruned, since pruning is gated on this run's pack)
    /// and are picked up by the next consolidation. Ordinary readers need no
    /// coordination.
    pub fn consolidate(&self, options: ConsolidateOptions) -> Result<ConsolidateStats, StoreError> {
        // Every ref is a graph root: this is the standalone reading — pack the
        // whole reachable set, no guard. Behaviour is identical to before the
        // graph-scoping seam existed.
        self.consolidate_scoped(options, &|_name| true)
    }

    /// Consolidate, packing only objects reachable from refs the `is_graph_ref`
    /// predicate accepts (ADR-0051 reading B, `acetone-wao`). Objects reachable
    /// from *rejected* (non-graph, e.g. co-tenant code) refs form a **prune
    /// guard**: their loose copies are never deleted, so `gc` leaves storage the
    /// graph does not own exactly as it found it. A standalone repository passes
    /// an accept-all predicate (via [`Self::consolidate`]), for which the guard
    /// is empty and the packed set is the whole reachable set — byte-identical
    /// to the pre-scoping behaviour.
    pub fn consolidate_scoped(
        &self,
        options: ConsolidateOptions,
        is_graph_ref: &dyn Fn(&str) -> bool,
    ) -> Result<ConsolidateStats, StoreError> {
        // Serialise consolidations (and their sidecar updates) on this
        // repository; released when this call returns.
        let _gc_guard = gix::lock::Marker::acquire_to_hold_resource(
            self.repo().common_dir().join("acetone-gc"),
            gix::lock::acquire::Fail::AfterDurationWithBackoff(std::time::Duration::from_secs(5)),
            None,
        )
        .map_err(|e| StoreError::backend("locking for consolidation", e))?;

        // Split the refs into the graph's roots (what we pack) and the rest
        // (the prune guard — objects we must not disturb).
        let (pack_roots, guard_roots) = self.classify_ref_roots(is_graph_ref)?;
        let reachable = self.reachable_from(&pack_roots)?;
        let present: HashSet<ObjectId> = reachable.iter().map(|(oid, _)| *oid).collect();
        let guard: HashSet<ObjectId> = self
            .reachable_from(&guard_roots)?
            .into_iter()
            .map(|(oid, _)| oid)
            .collect();
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

        // Pruning is gated on the OIDs the pack *actually* indexes, never on
        // what we intended to write, so a planning bug can never delete the
        // last copy of an object the pack omitted. As a loud tripwire for such
        // a bug, refuse to prune unless the pack covers the whole reachable set
        // (it must: one entry per reachable object).
        let packed: HashSet<ObjectId> = pack.oids().collect();
        if packed.len() != present.len() {
            return Err(StoreError::corrupt(
                "consolidation pack",
                format!(
                    "pack indexes {} objects but {} are reachable; refusing to prune",
                    packed.len(),
                    present.len()
                ),
            ));
        }

        let stem = format!("pack-{}", pack.trailer);
        self.install_pack(&stem, &pack.bytes, &idx)?;

        let pruned_loose = if options.prune_loose {
            self.prune_loose(&packed, &guard)?
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

    /// Resolve the repository's refs to root object ids, split by the
    /// `is_graph_ref` predicate: refs it accepts seed the set we pack, refs it
    /// rejects seed the prune guard. Symbolic chains are resolved to the named
    /// object (annotated tags are *not* peeled — the tag object is itself
    /// reachable); a dangling ref anchors nothing and is skipped. The predicate
    /// sees each ref's full name (e.g. `refs/heads/main`).
    fn classify_ref_roots(
        &self,
        is_graph_ref: &dyn Fn(&str) -> bool,
    ) -> Result<(Vec<ObjectId>, Vec<ObjectId>), StoreError> {
        let references = self
            .repo()
            .references()
            .map_err(|e| StoreError::backend("listing refs for consolidation", e))?;
        let all = references
            .all()
            .map_err(|e| StoreError::backend("iterating refs for consolidation", e))?;
        let mut pack_roots: Vec<ObjectId> = Vec::new();
        let mut guard_roots: Vec<ObjectId> = Vec::new();
        for reference in all {
            let mut reference = reference
                .map_err(|e| StoreError::corrupt("ref for consolidation", e.to_string()))?;
            let name = reference.name().as_bstr().to_string();
            if let Ok(id) = reference.follow_to_object() {
                let oid = id.detach();
                if is_graph_ref(&name) {
                    pack_roots.push(oid);
                } else {
                    guard_roots.push(oid);
                }
            }
        }
        Ok((pack_roots, guard_roots))
    }

    /// Enumerate every object reachable from `roots`, with its git kind. Walks
    /// the object graph iteratively (commits → tree + parents, trees → entries,
    /// tags → target) using only object decoding, so it needs no extra gix
    /// features and cannot overflow the stack on a hostile deep tree.
    fn reachable_from(
        &self,
        roots: &[ObjectId],
    ) -> Result<Vec<(ObjectId, gix::object::Kind)>, StoreError> {
        use gix::object::Kind;

        let mut stack: Vec<ObjectId> = roots.to_vec();
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

    /// Write a pack and its index into `objects/pack`, durably.
    ///
    /// Because consolidation prunes the old (loose) representations after this
    /// returns, the new pack must survive power loss before any pruning — so
    /// the bytes are `fsync`ed, not merely written into the page cache. git
    /// treats a pack as present only once its `.idx` exists, and then requires
    /// the `.pack`.
    ///
    /// **Idempotent.** The stem is content-addressed (`pack-<trailer>`), so an
    /// already-installed pack — both its `.pack` and its `.idx` present — is
    /// byte-for-byte what this call would produce. Installing it again is a
    /// no-op. This is not merely an optimisation: rewriting the `.pack` in place
    /// would truncate a *live* pack, and once a prior run pruned the loose
    /// sources those bytes are the only copy of their objects. A crash in the
    /// truncate-then-rewrite window would then destroy the repository — so a
    /// second `gc` must never reopen the live pack for writing.
    ///
    /// **Atomic.** A fresh pack is written to a temp file, `fsync`ed, then
    /// published by an atomic rename, so the destination `.pack` is never a torn
    /// or truncated file even transiently. The `.pack` is published and its
    /// directory entry `fsync`ed *before* the `.idx` is written (a pack with no
    /// index is simply ignored, and the objects remain available as loose until
    /// pruning); the index is likewise temp-written, renamed, and the directory
    /// `fsync`ed again. Making pack-entry durability strictly precede idx-entry
    /// durability means no filesystem reordering can persist the index rename
    /// while dropping the pack rename. A crash at any point leaves either no
    /// index (pack ignored, loose sources still unpruned) or a complete, durable
    /// index over a complete, durable pack — never a dangling or torn index, and
    /// never a pack that can evaporate after its loose sources were deleted.
    fn install_pack(&self, stem: &str, pack: &[u8], idx: &[u8]) -> Result<(), StoreError> {
        let dir = self.repo().common_dir().join("objects").join("pack");
        let pack_path = dir.join(format!("{stem}.pack"));
        let idx_path = dir.join(format!("{stem}.idx"));
        let keep_path = dir.join(format!("{stem}.keep"));
        // Content-addressed: if this exact pack is already durably installed,
        // do not touch it (truncating the only copy of pruned objects is fatal).
        // Still ensure the `.keep` marker exists — a prior run may predate it.
        if pack_path.exists() && idx_path.exists() {
            return ensure_keep(&dir, &keep_path);
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| StoreError::backend("creating objects/pack", e))?;
        let pack_tmp = dir.join(format!("{stem}.pack.tmp"));
        write_synced(&pack_tmp, pack, "writing pack")?;
        std::fs::rename(&pack_tmp, &pack_path)
            .map_err(|e| StoreError::backend("publishing pack", e))?;
        // Make the .pack directory entry durable before the .idx exists, so a
        // crash can never leave a published index over an absent pack.
        fsync_dir(&dir)?;
        let idx_tmp = dir.join(format!("{stem}.idx.tmp"));
        write_synced(&idx_tmp, idx, "writing pack index")?;
        std::fs::rename(&idx_tmp, &idx_path)
            .map_err(|e| StoreError::backend("publishing pack index", e))?;
        fsync_dir(&dir)?;
        // Mark the pack `.keep` (ADR-0053): a foreign `git gc`/`git repack`
        // (including git's automatic `gc.auto`, which a co-tenant repo's owner
        // triggers routinely) skips a kept pack, so acetone's content-aware
        // REF_DELTAs (ADR-0011) survive instead of being re-deltified back to a
        // poorly-compressed baseline. Written after the pack is durable; its
        // loss is a missed optimisation, never data loss. acetone manages the
        // kept pack's retirement itself via `supersede_packs`.
        ensure_keep(&dir, &keep_path)?;
        Ok(())
    }

    /// Delete the loose copies of the objects we just packed — except any also
    /// in `guard` (reachable from a non-graph ref), whose loose representation
    /// is left exactly as it was so `gc` never disturbs storage the graph does
    /// not own (ADR-0051 reading B). For a standalone repository `guard` is
    /// empty, so every packed object's loose copy is pruned as before.
    fn prune_loose(
        &self,
        packed: &HashSet<ObjectId>,
        guard: &HashSet<ObjectId>,
    ) -> Result<usize, StoreError> {
        let objects_dir = self.repo().common_dir().join("objects");
        let mut pruned = 0usize;
        for oid in packed {
            if guard.contains(oid) {
                continue;
            }
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
        // A prior pack that cannot be fully superseded (it holds an OID the new
        // pack does not) must stay *and* stay tracked, so a later run can
        // supersede it; only forgetting it would leak it permanently.
        let mut survivors: Vec<String> = Vec::new();
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
                // Retire the superseded pack's `.keep` too, so it does not
                // outlive its pack (ADR-0053).
                remove_if_present(&dir.join(format!("{stem}.keep")))?;
                pruned += 1;
            } else {
                survivors.push(stem.clone());
            }
        }
        survivors.push(new_stem.to_owned());
        self.write_pack_stems(&survivors)?;
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

        // Cap chain length: force a whole object every MAX_CHAIN links, so no
        // retained delta chain grows without bound. Deterministic over a sorted
        // view; iterative so a legitimately long single-chunk history cannot
        // overflow the stack.
        let mut sorted: Vec<ObjectId> = present.iter().copied().collect();
        sorted.sort();
        cap_chains(&mut base, &sorted);

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

/// Cap every delta chain at [`MAX_CHAIN`] links by clearing the base of each
/// object whose distance from its chain root would reach the cap (turning it
/// into a fresh whole "anchor"). `base` is a forest here — cycles were already
/// broken — so each node has at most one outgoing base edge.
///
/// Iterative by construction: for each unresolved object it walks *down* the
/// base chain, collecting the path until it reaches a root or an
/// already-resolved node, then assigns depths walking back *up*. A single
/// chain of length N is therefore O(N) time and O(1) stack, however long a
/// chunk's retained history is (the recursive form overflowed the stack at a
/// few thousand links).
fn cap_chains(base: &mut HashMap<ObjectId, ObjectId>, sorted: &[ObjectId]) {
    let mut depth: HashMap<ObjectId, usize> = HashMap::new();
    for &start in sorted {
        if depth.contains_key(&start) {
            continue;
        }
        // Descend the chain, collecting nodes whose depth is not yet known.
        let mut path: Vec<ObjectId> = Vec::new();
        let mut node = start;
        let (mut below, root_in_path) = loop {
            if let Some(&d) = depth.get(&node) {
                break (d, false); // joined an already-resolved chain at `node`
            }
            path.push(node);
            match base.get(&node).copied() {
                Some(b) => node = b,
                None => break (0, true), // `node` (last pushed) is a chain root
            }
        };
        if root_in_path {
            let root = path.pop().expect("root was pushed");
            depth.insert(root, 0);
            below = 0;
        }
        // Assign depths from the deepest collected node up to `start`, resetting
        // the chain whenever the cap is reached.
        for &node in path.iter().rev() {
            let d = below + 1;
            if d >= MAX_CHAIN {
                base.remove(&node); // whole anchor: reset the chain here
                depth.insert(node, 0);
                below = 0;
            } else {
                depth.insert(node, d);
                below = d;
            }
        }
    }
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

/// The body of a consolidation pack's `.keep` marker. git ignores the content;
/// it records why the pack is kept for a human reading the objects directory.
const KEEP_REASON: &str = "acetone consolidation pack (ADR-0011 delta encoding, ADR-0053 durability).\n\
     Content-aware REF_DELTAs; do not repack — acetone manages this pack's\n\
     lifecycle via its own consolidation (git repack/gc skips kept packs).\n";

/// Ensure `<stem>.keep` exists next to a consolidation pack so a foreign
/// `git gc`/`git repack` leaves it (and its deltas) alone (ADR-0053). Idempotent
/// and cheap; durability is a directory `fsync` (not a temp-and-rename — a torn
/// `.keep` is impossible since git reads only its existence, never its content).
///
/// Called from `install_pack` *after* the pack and index are durable, so a
/// failure here aborts the `consolidate` run **before** any pruning — a safe,
/// recoverable state (the pack is installed and valid; the loose sources are
/// untouched; the next run heals the missing marker and prunes). This is
/// deliberately loud rather than swallowed: silently skipping the marker would
/// let a later `git gc` quietly undo the deltas the marker exists to protect.
fn ensure_keep(dir: &std::path::Path, keep_path: &std::path::Path) -> Result<(), StoreError> {
    if keep_path.exists() {
        return Ok(());
    }
    std::fs::write(keep_path, KEEP_REASON)
        .map_err(|e| StoreError::backend("writing pack .keep", e))?;
    fsync_dir(dir)?;
    Ok(())
}

/// Write `data` to `path` and `fsync` it, so the bytes are on stable storage
/// before the caller relies on them (here, before pruning the loose sources).
fn write_synced(path: &std::path::Path, data: &[u8], what: &'static str) -> Result<(), StoreError> {
    let mut file = std::fs::File::create(path).map_err(|e| StoreError::backend(what, e))?;
    file.write_all(data)
        .map_err(|e| StoreError::backend(what, e))?;
    file.sync_all().map_err(|e| StoreError::backend(what, e))?;
    Ok(())
}

/// `fsync` a directory so a newly created or renamed entry within it survives
/// power loss. On the Unix targets acetone supports, opening the directory and
/// `sync_all`-ing it is the portable way to do this.
fn fsync_dir(dir: &std::path::Path) -> Result<(), StoreError> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| StoreError::backend("fsyncing objects/pack", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::object::Kind;

    /// A distinct blob OID per `n`, so tests can build hint graphs by number.
    fn oid(n: u16) -> ObjectId {
        gix::objs::compute_hash(gix::hash::Kind::Sha1, Kind::Blob, &n.to_be_bytes()).expect("hash")
    }

    /// As [`oid`] but over the full `u32` range, for large-chain tests.
    fn oid_u32(n: u32) -> ObjectId {
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
    fn a_very_long_chain_caps_without_overflowing_the_stack() {
        // A hint chain grows one link per retained version of a chunk, so a
        // long real history reaches here. The recursive capper overflowed the
        // stack in the low thousands; this must complete and stay capped.
        let n: u32 = 100_000;
        let objs: Vec<(ObjectId, Kind)> = (0..n).map(|i| (oid_u32(i), Kind::Blob)).collect();
        let hints: HashMap<ObjectId, ObjectId> =
            (1..n).map(|i| (oid_u32(i), oid_u32(i - 1))).collect();
        let plan = Plan::build(&objs, &hints);
        assert_eq!(plan.order.len(), n as usize);
        // Bases-first, checked in O(n) via a position index (the small-N helper
        // scans linearly and would be O(n^2) here).
        let position: HashMap<ObjectId, usize> = plan
            .order
            .iter()
            .enumerate()
            .map(|(i, o)| (*o, i))
            .collect();
        for (oid, base) in &plan.base {
            assert!(
                position[base] < position[oid],
                "base must precede dependant"
            );
        }
        // No surviving delta chain exceeds the cap.
        for (oid, _) in &objs {
            let mut depth = 0usize;
            let mut cur = *oid;
            while let Some(b) = plan.base.get(&cur) {
                depth += 1;
                assert!(depth <= MAX_CHAIN, "chain exceeds cap");
                cur = *b;
            }
        }
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
