//! `fsck`: verify a repository's chunk reachability and manifest
//! integrity (spec §7, ADR-0012).
//!
//! [`check`] walks every version a repository can reach — each workspace
//! manifest under `refs/acetone/workspaces/*`, and every commit reachable
//! from `refs/heads/*` and `refs/tags/*` (a lightweight tag is walked like
//! a branch; an annotated tag is peeled to its target commit, which is then
//! verified like any other — acetone-8t3). Symbolic refs under every walked
//! namespace are resolved and verified the same way; one that resolves to
//! nothing (dangling or unborn) is named as an advisory rather than
//! silently skipped (acetone-5lo). For each version it confirms:
//!
//! 1. **Manifest integrity**: the manifest bytes exist and decode under
//!    the strict decoder; its map roots have valid heights.
//! 2. **Chunk reachability**: every chunk transitively reachable from each
//!    map root is present in the store and decodes as a valid prolly node
//!    consistent with its position — with **missing** chunks reported
//!    distinctly from **corrupt** ones
//!    ([`acetone_prolly::verify_reachable`]).
//! 3. **Edge-map symmetry** (advisory): the forward and reverse edge maps
//!    describe the same edge set. This invariant (spec §3.3) is maintained
//!    by construction by the Phase 1 write path but not yet *enforced*
//!    against hand-built or foreign repositories, so a violation is a
//!    warning, not a hard failure (ADR-0012).
//! 4. **Index consistency** (advisory): every declared `idx/<name>` map is
//!    exactly what `nodes` implies (Invariant #5), and no declared index is
//!    missing or stale. Repairable with `acetone reindex`.
//! 5. **History-independence spot-check** (error): the primary content maps
//!    are the canonical prolly tree for their contents — rebuilding a map from
//!    what it holds reproduces its root (Invariant #1). A mismatch is damage
//!    or a foreign, non-canonical writer.
//!
//! The result is structured data, not a boolean: a healthy repository
//! yields an empty [`FsckReport`], and every finding names the ref/commit,
//! the map and (for chunk faults) the offending chunk. The library API is
//! deliberately CLI-free; bead acetone-63m.6 wires `acetone fsck` on top.
//!
//! # Totality
//!
//! `check` treats every version it enumerates as untrusted: a manifest
//! that will not decode, a branch tip that is not a commit, a missing
//! workspace blob and every kind of chunk damage become **findings**, not
//! aborts or panics. Only a failure of the enumeration primitives
//! themselves (listing refs) propagates as [`GraphError`].

use std::collections::BTreeSet;
use std::fmt;

use acetone_model::manifest::{Manifest, MapRoot};
use acetone_prolly::{BatchOp, ChunkFaultKind, apply_batch, empty, scan, verify_reachable};
use acetone_store::{ChunkStore, CommitStore, GitStore, Hash, RefStore, StoreError};

use crate::error::GraphError;
use crate::repo::{
    BRANCH_REF_PREFIX, Repository, Snapshot, TAG_REF_PREFIX, WORKSPACE_REF_PREFIX,
    WORKTREE_WORKSPACE_REF,
};

/// How serious a [`Finding`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The version is damaged: data cannot be read back faithfully.
    Error,
    /// A consistency property not yet enforced by the write path is
    /// violated, but the data is structurally intact.
    Advisory,
}

/// What kind of problem a [`Finding`] records (ADR-0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// A manifest blob is missing, the wrong object kind, or does not
    /// decode under the strict decoder.
    Manifest,
    /// A ref target or ancestor reachable from `refs/heads/*` is not a
    /// readable acetone commit.
    Commit,
    /// A map root transitively references a chunk absent from the store.
    MissingChunk,
    /// A chunk exists but is not a valid prolly node at its position, or
    /// the store could not return it.
    CorruptChunk,
    /// A map root records a height outside `1..=MAX_HEIGHT` (in practice
    /// unreachable via the strict decoder; kept so the walk stays total).
    MapRoot,
    /// Advisory: the forward and reverse edge maps disagree.
    EdgeAsymmetry,
    /// Advisory: a declared index map disagrees with what `nodes` implies (a
    /// derived-map divergence, Invariant #5). Repairable with `acetone reindex`.
    IndexInconsistency,
    /// A map's stored prolly tree is not the canonical tree for its contents:
    /// rebuilding the map from what it holds yields a different root
    /// (Invariant #1 — history independence). Real damage or a foreign writer.
    HistoryIndependence,
    /// An edge in `edges_fwd` references an endpoint node absent from `nodes`
    /// (referential integrity, ADR-0028 / Invariant #3). The write path now
    /// rejects this, but an older or foreign-written repository may carry one.
    DanglingEdge,
    /// Advisory: a reachable ref was found but there is nothing to verify
    /// behind it (a symbolic ref whose chain ends at an absent or unborn
    /// ref). The sin fsck must avoid is silence, so the ref is named rather
    /// than skipped.
    Unverified,
}

impl FindingKind {
    fn severity(&self) -> Severity {
        match self {
            FindingKind::EdgeAsymmetry
            | FindingKind::IndexInconsistency
            | FindingKind::Unverified => Severity::Advisory,
            _ => Severity::Error,
        }
    }
}

/// The version a finding belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A workspace manifest, named by its full ref.
    Workspace {
        /// The `refs/acetone/workspaces/*` ref.
        reference: String,
    },
    /// A commit reachable from a branch.
    Commit {
        /// The branch ref the commit was reached from.
        reference: String,
        /// The commit's address.
        commit: Hash,
    },
    /// A ref that resolved to no object at all (e.g. a dangling symbolic
    /// ref), so there is no version to attribute the finding to.
    Ref {
        /// The full ref name.
        reference: String,
    },
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Workspace { reference } => write!(f, "workspace {reference}"),
            Origin::Commit { reference, commit } => write!(f, "commit {commit} (via {reference})"),
            Origin::Ref { reference } => write!(f, "ref {reference}"),
        }
    }
}

/// Which map within a manifest a finding concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapId {
    /// The `nodes` map.
    Nodes,
    /// The `schema` map.
    Schema,
    /// The `edges_fwd` map.
    EdgesFwd,
    /// The `edges_rev` map.
    EdgesRev,
    /// A declared index map `idx/<name>`.
    Index(String),
    /// The `conflicts` map (present only mid-merge).
    Conflicts,
}

impl fmt::Display for MapId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapId::Nodes => f.write_str("nodes"),
            MapId::Schema => f.write_str("schema"),
            MapId::EdgesFwd => f.write_str("edges_fwd"),
            MapId::EdgesRev => f.write_str("edges_rev"),
            MapId::Index(name) => write!(f, "index {name}"),
            MapId::Conflicts => f.write_str("conflicts"),
        }
    }
}

/// One problem `fsck` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// How serious it is.
    pub severity: Severity,
    /// What kind of problem.
    pub kind: FindingKind,
    /// The version it belongs to.
    pub origin: Origin,
    /// The map within the manifest, when the problem is map-specific.
    pub map: Option<MapId>,
    /// The offending chunk, for chunk-level faults.
    pub chunk: Option<Hash>,
    /// Human-readable detail.
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Advisory => "advisory",
        };
        write!(f, "[{sev}] {}", self.origin)?;
        if let Some(map) = &self.map {
            write!(f, " / {map}")?;
        }
        if let Some(chunk) = &self.chunk {
            write!(f, " / chunk {chunk}")?;
        }
        write!(f, ": {}", self.detail)
    }
}

/// The result of an [`fsck`](check) run: an empty [`Self::findings`] means
/// the repository is intact.
#[derive(Debug, Clone, Default)]
pub struct FsckReport {
    /// Every problem found, in discovery order.
    pub findings: Vec<Finding>,
}

impl FsckReport {
    /// Whether the repository is entirely clean — no findings of any
    /// severity, advisories included.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Whether any finding is an [`Severity::Error`] (damage, as opposed to
    /// an advisory).
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    /// The error-severity findings.
    pub fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
    }

    /// The advisory-severity findings.
    pub fn advisories(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Advisory)
    }

    fn push(&mut self, kind: FindingKind, origin: &Origin, map: Option<MapId>, detail: String) {
        self.findings.push(Finding {
            severity: kind.severity(),
            kind,
            origin: origin.clone(),
            map,
            chunk: None,
            detail,
        });
    }
}

/// The memo identity of one map root: `(chunk hash, height)`. The hash alone
/// is not enough — see [`Verified::roots`].
type RootKey = (Hash, u32);

/// The [`RootKey`] of a manifest map root.
fn root_key(root: &MapRoot) -> RootKey {
    (root.hash, root.height)
}

/// Cross-version memoisation, so verifying deep history stays close to
/// O(distinct chunks) rather than O(history × tree): a map root, whole
/// manifest, or content-level check input set verified once is not
/// re-walked when a later version reuses it (acetone-7fe).
///
/// The content-level memos (`edge_symmetry`, `referential`, `canonical`,
/// `indexes`) record their input roots **before** the check runs, like
/// `manifests`: a shared *faulty* input set is therefore attributed only to
/// the first origin that reaches it — an under-attribution of the same
/// fault across origins, never a missed fault — and the O(distinct inputs)
/// bound holds even on damaged repositories.
#[derive(Default)]
struct Verified {
    /// Map roots confirmed to reach only intact chunks, keyed by
    /// **`(chunk hash, height)`**. The hash alone is not enough: what
    /// `verify_reachable` checks depends on the height too (it sets the
    /// root's expected level to `height - 1`), and the height lives in the
    /// manifest, not in the content-addressed chunk — so a manifest that
    /// pairs a known-good root hash with a *wrong* height must still be
    /// verified, and rejected, rather than skipped. (Chunk parameters are
    /// fixed per repository and do not affect the structural walk, so they
    /// are not part of the key.)
    roots: BTreeSet<RootKey>,
    /// Manifest blob hashes already fully checked. Sound to key on the blob
    /// hash alone: the manifest bytes fix every `(root hash, height)` pair
    /// it names. A shared *damaged* manifest is therefore attributed only to
    /// the first origin that reaches it — an under-attribution of the same
    /// fault across origins, never a missed fault.
    manifests: BTreeSet<Hash>,
    /// `(edges_fwd, edges_rev)` root pairs whose symmetry advisory has
    /// already run: the check's outcome is a pure function of the two maps'
    /// contents, so an unchanged pair is not re-materialised for every
    /// commit that shares it (the deep-history amplification of
    /// acetone-7fe).
    edge_symmetry: BTreeSet<(RootKey, RootKey)>,
    /// `(nodes, edges_fwd)` root pairs whose referential-integrity check
    /// has already run — it too materialises both maps in full.
    referential: BTreeSet<(RootKey, RootKey)>,
    /// Map roots whose canonical-tree (history-independence) rebuild has
    /// already run, keyed by root **and chunk parameters**: the canonical
    /// tree for the same contents differs under different parameters, and
    /// parameters live in the manifest, so a hostile manifest pairing a
    /// known root with different parameters must still be rebuilt.
    canonical: BTreeSet<(RootKey, acetone_prolly::ChunkParams)>,
    /// Index consistency inputs already checked: the check is a pure
    /// function of the schema (index definitions), the nodes map, and the
    /// named index map.
    indexes: BTreeSet<(RootKey, RootKey, RootKey, String)>,
}

/// Verify a repository's chunk reachability and manifest integrity.
///
/// Checks every workspace manifest (`refs/acetone/workspaces/*`) and every
/// commit reachable from `refs/heads/*` and `refs/tags/*`. See the module
/// docs for the finding taxonomy and totality guarantees. A clean
/// repository returns an [`FsckReport`] with no findings.
pub fn check(repo: &Repository) -> Result<FsckReport, GraphError> {
    check_store(repo.store())
}

/// Verify the repository at `path` without first constructing a
/// [`Repository`] — so `fsck` runs even when the default workspace manifest is
/// **damaged or absent**, which is exactly when the diagnostic is needed
/// (acetone-zhp). [`Repository::open`] fail-fasts by decoding that manifest, so
/// `check(&repo)` cannot be reached on a broken repository; this opens only the
/// underlying [`GitStore`] and runs the same checks, reporting the damage as
/// [`Finding`]s rather than erroring at open.
pub fn check_path(path: &std::path::Path) -> Result<FsckReport, GraphError> {
    let store = GitStore::open_discovering(path)?;
    check_store(&store)
}

/// The store-level fsck used by both [`check`] and [`check_path`]. It needs only
/// the chunk/ref/commit store — never a decoded workspace manifest — so it is
/// robust to a damaged workspace.
fn check_store(store: &GitStore) -> Result<FsckReport, GraphError> {
    let mut report = FsckReport::default();
    let mut verified = Verified::default();
    check_workspaces(store, &mut verified, &mut report)?;
    check_commit_tips(store, BRANCH_REF_PREFIX, false, &mut verified, &mut report)?;
    check_commit_tips(store, TAG_REF_PREFIX, true, &mut verified, &mut report)?;
    Ok(report)
}

/// Verify every workspace manifest. Workspace refs point straight at a
/// manifest blob (ADR-0010), so this reads the blob and checks the
/// manifest directly.
///
/// Post-ADR-0014 the current worktree's workspace is a single per-worktree
/// ref (`refs/worktree/acetone/workspace`), checked by name; any legacy
/// shared `refs/acetone/workspaces/*` refs (a not-yet-migrated repository)
/// are still enumerated and checked.
fn check_workspaces(
    store: &GitStore,
    verified: &mut Verified,
    report: &mut FsckReport,
) -> Result<(), GraphError> {
    let mut refs: Vec<(String, Hash)> = store.list_refs(WORKSPACE_REF_PREFIX)?;
    // Symbolic workspace refs are invisible to list_refs (acetone-5lo):
    // resolve each and verify what it names; a dangling one is named as an
    // advisory rather than silently skipped.
    for (reference, target) in store.list_symbolic_refs(WORKSPACE_REF_PREFIX)? {
        push_resolved_symref(
            store,
            reference,
            &target,
            FindingKind::Manifest,
            &mut refs,
            report,
        );
    }
    match store.read_ref(WORKTREE_WORKSPACE_REF) {
        Ok(Some(hash)) => refs.push((WORKTREE_WORKSPACE_REF.to_owned(), hash)),
        Ok(None) => {}
        // A foreign tool can make even the per-worktree workspace ref
        // symbolic; before acetone-5lo this aborted the whole fsck run.
        Err(StoreError::SymbolicRef { .. }) => push_resolved_symref(
            store,
            WORKTREE_WORKSPACE_REF.to_owned(),
            "(symbolic)",
            FindingKind::Manifest,
            &mut refs,
            report,
        ),
        Err(err) => return Err(err.into()),
    }
    for (reference, ref_hash) in refs {
        let origin = Origin::Workspace { reference };
        // The ref points at a workspace tree (huo) whose `manifest` entry is
        // the blob, or — for a pre-huo workspace — the manifest blob
        // directly; the store resolves both.
        let manifest_hash = match store.workspace_manifest_hash(&ref_hash) {
            Ok(hash) => hash,
            Err(err) => {
                report.push(
                    FindingKind::Manifest,
                    &origin,
                    None,
                    format!("workspace ref {ref_hash} does not resolve to a manifest: {err}"),
                );
                continue;
            }
        };
        match store.get(&manifest_hash) {
            Ok(Some(bytes)) => {
                check_manifest(store, &origin, manifest_hash, &bytes, verified, report)
            }
            Ok(None) => report.push(
                FindingKind::Manifest,
                &origin,
                None,
                format!("workspace manifest blob {manifest_hash} is absent from the store"),
            ),
            Err(err) => report.push(
                FindingKind::Manifest,
                &origin,
                None,
                format!("workspace manifest blob {manifest_hash} could not be read: {err}"),
            ),
        }
    }
    Ok(())
}

/// Resolve one symbolic ref and either queue it for verification alongside
/// the direct refs, or report it: a dangling chain is a named
/// [`FindingKind::Unverified`] advisory (the sin fsck must avoid is
/// silence), and a resolution failure (a cycle, or hostile nesting) is an
/// error finding of `failure_kind` — the walk's own kind, `Manifest` for
/// workspaces and `Commit` for branch/tag tips.
fn push_resolved_symref(
    store: &GitStore,
    reference: String,
    target: &str,
    failure_kind: FindingKind,
    refs: &mut Vec<(String, Hash)>,
    report: &mut FsckReport,
) {
    match store.resolve_symref(&reference) {
        Ok(Some(hash)) => refs.push((reference, hash)),
        Ok(None) => {
            let origin = Origin::Ref { reference };
            report.push(
                FindingKind::Unverified,
                &origin,
                None,
                format!(
                    "symbolic ref (-> {target}) resolves to no object (dangling or \
                     unborn), so there is nothing to verify"
                ),
            );
        }
        Err(err) => {
            let origin = Origin::Ref { reference };
            report.push(
                failure_kind,
                &origin,
                None,
                format!("symbolic ref could not be resolved: {err}"),
            );
        }
    }
}

/// Verify every commit reachable from the refs under `prefix` — direct refs
/// and resolved symbolic refs alike (acetone-5lo) — following all parents
/// and deduplicating commits so shared history is checked once.
///
/// When `is_tag` is set, a ref pointing at a git tag *object* rather than a
/// commit (an annotated tag) is peeled to its target, which is then
/// verified like any other reachable commit (acetone-8t3); a tag that
/// cannot be peeled is an error finding, and a peeled target that is not a
/// commit is reported exactly like a lightweight tag on a non-commit.
fn check_commit_tips(
    store: &GitStore,
    prefix: &str,
    is_tag: bool,
    verified: &mut Verified,
    report: &mut FsckReport,
) -> Result<(), GraphError> {
    let mut tips: Vec<(String, Hash)> = store.list_refs(prefix)?;
    for (reference, target) in store.list_symbolic_refs(prefix)? {
        push_resolved_symref(
            store,
            reference,
            &target,
            FindingKind::Commit,
            &mut tips,
            report,
        );
    }
    let mut seen = BTreeSet::new();
    for (reference, tip) in tips {
        let mut stack = vec![(tip, true)];
        while let Some((id, is_tip)) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let origin = Origin::Commit {
                reference: reference.clone(),
                commit: id,
            };
            match store.read_commit(&id) {
                Ok(Some(commit)) => {
                    check_manifest(
                        store,
                        &origin,
                        commit.id,
                        &commit.manifest,
                        verified,
                        report,
                    );
                    stack.extend(commit.parents.into_iter().map(|p| (p, false)));
                }
                Ok(None) => report.push(
                    FindingKind::Commit,
                    &origin,
                    None,
                    format!("commit {id} is referenced by history but absent from the store"),
                ),
                // An annotated tag's tip is a tag object, not a commit: peel
                // it and verify its target (acetone-8t3). The peeled id joins
                // the same walk, so an absent or non-commit target is
                // reported through the ordinary branches above/below, and a
                // target already seen via another ref is not re-walked.
                Err(StoreError::WrongObjectKind { actual, .. })
                    if is_tag && is_tip && actual == "tag" =>
                {
                    match store.peel_tag(&id) {
                        Ok(target) => stack.push((target, false)),
                        Err(err) => report.push(
                            FindingKind::Commit,
                            &origin,
                            None,
                            format!(
                                "{reference} is an annotated tag that could not be peeled: {err}"
                            ),
                        ),
                    }
                }
                Err(err) => report.push(
                    FindingKind::Commit,
                    &origin,
                    None,
                    format!("commit {id} is not a readable acetone commit: {err}"),
                ),
            }
        }
    }
    Ok(())
}

/// Decode one manifest (identified by its blob `hash`) and verify every map
/// it references. A manifest already fully checked in this run is skipped.
fn check_manifest(
    store: &GitStore,
    origin: &Origin,
    hash: Hash,
    bytes: &[u8],
    verified: &mut Verified,
    report: &mut FsckReport,
) {
    if !verified.manifests.insert(hash) {
        return;
    }
    let manifest = match Manifest::decode(bytes) {
        Ok(manifest) => manifest,
        Err(err) => {
            report.push(
                FindingKind::Manifest,
                origin,
                None,
                format!("manifest does not decode: {err}"),
            );
            return;
        }
    };

    let nodes_ok = verify_map(
        store,
        origin,
        MapId::Nodes,
        &manifest.nodes,
        &manifest,
        verified,
        report,
    );
    verify_map(
        store,
        origin,
        MapId::Schema,
        &manifest.schema,
        &manifest,
        verified,
        report,
    );
    let fwd_ok = verify_map(
        store,
        origin,
        MapId::EdgesFwd,
        &manifest.edges_fwd,
        &manifest,
        verified,
        report,
    );
    let rev_ok = verify_map(
        store,
        origin,
        MapId::EdgesRev,
        &manifest.edges_rev,
        &manifest,
        verified,
        report,
    );
    let mut sound_indexes = Vec::new();
    for (name, root) in &manifest.indexes {
        if verify_map(
            store,
            origin,
            MapId::Index(name.clone()),
            root,
            &manifest,
            verified,
            report,
        ) {
            sound_indexes.push(name.clone());
        }
    }
    if let Some(conflicts) = &manifest.conflicts {
        verify_map(
            store,
            origin,
            MapId::Conflicts,
            conflicts,
            &manifest,
            verified,
            report,
        );
    }

    // Only run the edge-symmetry advisory when both edge maps are
    // structurally sound — otherwise verify_map has already reported the
    // real (error-severity) corruption, and a "could not check symmetry"
    // advisory would just be noise. Memoised by the (edges_fwd, edges_rev)
    // root pair (acetone-7fe): the outcome is a pure function of the two
    // maps' contents, so a pair shared across versions is materialised and
    // compared once, attributed to the first origin that reaches it.
    if fwd_ok
        && rev_ok
        && verified
            .edge_symmetry
            .insert((root_key(&manifest.edges_fwd), root_key(&manifest.edges_rev)))
    {
        check_edge_symmetry(store, origin, &manifest, report);
    }

    // Referential integrity (ADR-0028, Invariant #3): every forward edge must
    // have both its endpoint nodes present in `nodes`. Gated on both maps being
    // structurally sound so a corruption verify_map already reported is not
    // double-counted. Memoised by the (nodes, edges_fwd) root pair — it too
    // materialises both maps in full (acetone-7fe).
    if nodes_ok
        && fwd_ok
        && verified
            .referential
            .insert((root_key(&manifest.nodes), root_key(&manifest.edges_fwd)))
    {
        check_referential_integrity(store, origin, &manifest, report);
    }

    // Index consistency (spec §3.3, Invariant #5): each declared index must be
    // exactly reproducible from `nodes`. Only checked for indexes whose map is
    // structurally sound and when the nodes map is sound — otherwise verify_map
    // has already reported the real corruption.
    if nodes_ok {
        // A schema-declared index with no `idx/<name>` map at all is missing
        // entirely (the mirror of a map with no declaration).
        if let Ok(entries) = Snapshot::new(store, manifest.clone()).schema_entries() {
            let (index_defs, _) = crate::index::schema_index_info(&entries);
            for (name, _) in &index_defs {
                if !manifest.indexes.contains_key(name) {
                    report.push(
                        FindingKind::IndexInconsistency,
                        origin,
                        Some(MapId::Index(name.clone())),
                        format!("index {name:?} is declared but has no map; run `acetone reindex`"),
                    );
                }
            }
        }
        for name in &sound_indexes {
            // Memoised by (schema, nodes, index map, name): the check is a
            // pure function of those inputs (acetone-7fe).
            let Some(index_root) = manifest.indexes.get(name) else {
                continue;
            };
            if verified.indexes.insert((
                root_key(&manifest.schema),
                root_key(&manifest.nodes),
                root_key(index_root),
                name.clone(),
            )) {
                check_index_consistency(store, origin, &manifest, name, report);
            }
        }
    }

    // History-independence spot-checks (spec §7, Invariant #1): the primary
    // content maps must be the canonical prolly tree for their contents.
    // Gated on structural soundness — a corrupt tree is reported above.
    // Memoised by (root, chunk params), since the canonical tree for the
    // same contents differs under different parameters (acetone-7fe).
    let params = manifest.chunk_params;
    if nodes_ok
        && verified
            .canonical
            .insert((root_key(&manifest.nodes), params))
    {
        check_canonical(store, params, &manifest.nodes, MapId::Nodes, origin, report);
    }
    if fwd_ok
        && verified
            .canonical
            .insert((root_key(&manifest.edges_fwd), params))
    {
        check_canonical(
            store,
            params,
            &manifest.edges_fwd,
            MapId::EdgesFwd,
            origin,
            report,
        );
    }
}

/// Verify a map's stored root is the canonical prolly tree for its contents:
/// scan what it holds, rebuild it from scratch, and compare roots. A mismatch
/// is a history-independence violation (Invariant #1) — the tree was not built
/// canonically (damage or a foreign writer). Returns silently on a read error
/// (verify_map has already reported the structural fault).
fn check_canonical(
    store: &GitStore,
    params: acetone_prolly::ChunkParams,
    map_root: &MapRoot,
    map_id: MapId,
    origin: &Origin,
    report: &mut FsckReport,
) {
    let Ok(root) = map_root.to_root(params) else {
        return;
    };
    let mut ops = Vec::new();
    match scan(store, &root, ..) {
        Ok(items) => {
            for item in items {
                match item {
                    Ok((key, value)) => ops.push(BatchOp::Put(key.to_vec(), value.to_vec())),
                    Err(_) => return,
                }
            }
        }
        Err(_) => return,
    }
    let Ok(empty_root) = empty(store, params) else {
        return;
    };
    let Ok(rebuilt) = apply_batch(store, &empty_root, ops) else {
        return;
    };
    if rebuilt.hash() != root.hash() {
        report.push(
            FindingKind::HistoryIndependence,
            origin,
            Some(map_id),
            "map root is not the canonical prolly tree for its contents \
             (history-independence violation, Invariant #1)"
                .to_owned(),
        );
    }
}

/// Advisory check that a declared index map matches what `nodes` implies,
/// using the exact same entry-key function the write path uses (so only a
/// genuine divergence trips it). Repairable with `acetone reindex`.
fn check_index_consistency(
    store: &GitStore,
    origin: &Origin,
    manifest: &Manifest,
    name: &str,
    report: &mut FsckReport,
) {
    let snapshot = Snapshot::new(store, manifest.clone());
    let entries = match snapshot.schema_entries() {
        Ok(e) => e,
        Err(_) => return, // schema decode issues are reported elsewhere
    };
    let (index_defs, label_keys) = crate::index::schema_index_info(&entries);
    let Some((_, def)) = index_defs.iter().find(|(n, _)| n == name) else {
        // A `idx/<name>` map with no declaring schema entry is stale.
        report.push(
            FindingKind::IndexInconsistency,
            origin,
            Some(MapId::Index(name.to_owned())),
            format!("index {name:?} has a map but no schema declaration; run `acetone reindex`"),
        );
        return;
    };

    // Expected entry keys, recomputed from nodes.
    let expected: BTreeSet<Vec<u8>> = match snapshot.nodes() {
        Ok(nodes) => nodes
            .iter()
            .filter_map(|(key, record)| {
                crate::index::index_entry_key(key, Some(record), def, &label_keys)
            })
            .collect(),
        Err(_) => return,
    };
    // Actual entry keys, from the index map.
    let actual: BTreeSet<Vec<u8>> = match manifest.indexes.get(name) {
        Some(root) => match root.to_root(manifest.chunk_params) {
            Ok(root) => match acetone_prolly::scan(store, &root, ..) {
                Ok(scan) => {
                    let mut set = BTreeSet::new();
                    for item in scan {
                        match item {
                            Ok((key, _)) => {
                                set.insert(key.to_vec());
                            }
                            Err(_) => return,
                        }
                    }
                    set
                }
                Err(_) => return,
            },
            Err(_) => return,
        },
        None => BTreeSet::new(),
    };

    let missing = expected.difference(&actual).count();
    let extra = actual.difference(&expected).count();
    if missing != 0 || extra != 0 {
        report.push(
            FindingKind::IndexInconsistency,
            origin,
            Some(MapId::Index(name.to_owned())),
            format!(
                "index {name:?} disagrees with nodes: {missing} missing, \
                 {extra} stale entr{}; run `acetone reindex`",
                if missing + extra == 1 { "y" } else { "ies" }
            ),
        );
    }
}

/// Verify chunk reachability for one map root, returning `true` if it was
/// clean. A `(root hash, height)` confirmed clean earlier in this run is
/// trusted without re-walking — see [`Verified::roots`] for why the height
/// is part of the key.
fn verify_map(
    store: &GitStore,
    origin: &Origin,
    map: MapId,
    map_root: &MapRoot,
    manifest: &Manifest,
    verified: &mut Verified,
    report: &mut FsckReport,
) -> bool {
    let root_key = (map_root.hash, map_root.height);
    if verified.roots.contains(&root_key) {
        return true;
    }
    let root = match map_root.to_root(manifest.chunk_params) {
        Ok(root) => root,
        Err(err) => {
            report.push(
                FindingKind::MapRoot,
                origin,
                Some(map),
                format!("map root does not reconstruct: {err}"),
            );
            return false;
        }
    };
    let faults = verify_reachable(store, &root);
    if faults.is_empty() {
        verified.roots.insert(root_key);
        return true;
    }
    for fault in faults {
        let kind = match fault.kind {
            ChunkFaultKind::Missing => FindingKind::MissingChunk,
            ChunkFaultKind::Corrupt => FindingKind::CorruptChunk,
        };
        report.findings.push(Finding {
            severity: kind.severity(),
            kind,
            origin: origin.clone(),
            map: Some(map.clone()),
            chunk: Some(fault.hash),
            detail: fault.reason,
        });
    }
    false
}

/// Advisory check: the forward and reverse edge maps must describe the same
/// edge set (spec §3.3). Each edge is identified by its canonical forward
/// key, so a reverse entry is matched to the forward entry for the same
/// edge regardless of which map it came from.
///
/// [`verify_map`] has already confirmed both edge maps are structurally
/// sound *prolly trees*, but a structurally valid chunk can still hold a
/// byte string that is not a valid edge key or record. Full semantic
/// validation of map contents is a later-phase concern (ADR-0012), so when
/// an edge entry fails to decode this surfaces it as a clearly-labelled
/// advisory — "could not check symmetry" — rather than silently passing
/// the repository as clean.
/// Referential integrity (ADR-0028, Invariant #3): every edge in `edges_fwd`
/// must have both endpoint nodes present in `nodes`. The write path now rejects
/// dangling edges, but an older or foreign-written repository may carry one, and
/// fsck must name it rather than stay silent. The caller gates this on both maps
/// being structurally sound (verify_map has otherwise reported the real fault).
fn check_referential_integrity(
    store: &GitStore,
    origin: &Origin,
    manifest: &Manifest,
    report: &mut FsckReport,
) {
    let snapshot = Snapshot::new(store, manifest.clone());
    let (Ok(nodes), Ok(edges)) = (snapshot.nodes(), snapshot.edges()) else {
        return;
    };
    let present: BTreeSet<Vec<u8>> = nodes
        .iter()
        .filter_map(|(key, _)| key.encode().ok())
        .collect();
    for (edge, _) in &edges {
        for (role, endpoint) in [("source", edge.src()), ("target", edge.dst())] {
            let Ok(enc) = endpoint.encode() else { continue };
            if !present.contains(&enc) {
                report.push(
                    FindingKind::DanglingEdge,
                    origin,
                    Some(MapId::EdgesFwd),
                    format!(
                        "edge :{} from {}{:?} to {}{:?} has no {} node",
                        edge.rtype(),
                        edge.src().label(),
                        edge.src().key(),
                        edge.dst().label(),
                        edge.dst().key(),
                        role,
                    ),
                );
            }
        }
    }
}

fn check_edge_symmetry(
    store: &GitStore,
    origin: &Origin,
    manifest: &Manifest,
    report: &mut FsckReport,
) {
    let snapshot = Snapshot::new(store, manifest.clone());
    let forward = match snapshot.edges() {
        Ok(forward) => forward,
        Err(err) => {
            report.push(
                FindingKind::EdgeAsymmetry,
                origin,
                Some(MapId::EdgesFwd),
                format!(
                    "forward edge entries could not be decoded as edges, so symmetry \
                     was not checked: {err}"
                ),
            );
            return;
        }
    };
    let reverse = match snapshot.reverse_edge_keys() {
        Ok(reverse) => reverse,
        Err(err) => {
            report.push(
                FindingKind::EdgeAsymmetry,
                origin,
                Some(MapId::EdgesRev),
                format!(
                    "reverse edge entries could not be decoded as edges, so symmetry \
                     was not checked: {err}"
                ),
            );
            return;
        }
    };

    // Canonical identity of each edge, from either map. A key that decoded
    // but will not re-encode is a contradiction in the model layer; surface
    // it as an advisory rather than dropping it, which would understate the
    // edge set and could hide a real asymmetry.
    let mut reencode_failures = 0usize;
    let mut forward_ids = BTreeSet::new();
    for (key, _) in &forward {
        match key.encode_fwd() {
            Ok(id) => {
                forward_ids.insert(id);
            }
            Err(_) => reencode_failures += 1,
        }
    }
    let mut reverse_ids = BTreeSet::new();
    for key in &reverse {
        match key.encode_fwd() {
            Ok(id) => {
                reverse_ids.insert(id);
            }
            Err(_) => reencode_failures += 1,
        }
    }
    if reencode_failures > 0 {
        report.push(
            FindingKind::EdgeAsymmetry,
            origin,
            None,
            format!(
                "{reencode_failures} edge key(s) decoded but did not re-encode, so the \
                 symmetry comparison is incomplete"
            ),
        );
    }

    let missing_reverse = forward_ids.difference(&reverse_ids).count();
    let missing_forward = reverse_ids.difference(&forward_ids).count();
    if missing_reverse > 0 {
        report.push(
            FindingKind::EdgeAsymmetry,
            origin,
            Some(MapId::EdgesRev),
            format!(
                "{missing_reverse} forward edge(s) have no matching reverse entry \
                 (edges_fwd and edges_rev must be symmetric, spec §3.3)"
            ),
        );
    }
    if missing_forward > 0 {
        report.push(
            FindingKind::EdgeAsymmetry,
            origin,
            Some(MapId::EdgesFwd),
            format!(
                "{missing_forward} reverse edge(s) have no matching forward entry \
                 (edges_fwd and edges_rev must be symmetric, spec §3.3)"
            ),
        );
    }
}
