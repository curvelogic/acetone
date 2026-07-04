//! `fsck`: verify a repository's chunk reachability and manifest
//! integrity (spec §7, ADR-0011).
//!
//! [`check`] walks every version a repository can reach — each workspace
//! manifest under `refs/acetone/workspaces/*`, and every commit reachable
//! from `refs/heads/*` — and confirms, for each:
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
//!    warning, not a hard failure (ADR-0011).
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
use acetone_prolly::{ChunkFaultKind, verify_reachable};
use acetone_store::{ChunkStore, CommitStore, GitStore, Hash, RefStore};

use crate::error::GraphError;
use crate::repo::{BRANCH_REF_PREFIX, Repository, Snapshot, WORKSPACE_REF_PREFIX};

/// How serious a [`Finding`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The version is damaged: data cannot be read back faithfully.
    Error,
    /// A consistency property not yet enforced by the write path is
    /// violated, but the data is structurally intact.
    Advisory,
}

/// What kind of problem a [`Finding`] records (ADR-0011).
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
}

impl FindingKind {
    fn severity(&self) -> Severity {
        match self {
            FindingKind::EdgeAsymmetry => Severity::Advisory,
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
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Workspace { reference } => write!(f, "workspace {reference}"),
            Origin::Commit { reference, commit } => write!(f, "commit {commit} (via {reference})"),
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

/// Verify a repository's chunk reachability and manifest integrity.
///
/// Checks every workspace manifest (`refs/acetone/workspaces/*`) and every
/// commit reachable from `refs/heads/*`. See the module docs for the
/// finding taxonomy and totality guarantees. A clean repository returns an
/// [`FsckReport`] with no findings.
pub fn check(repo: &Repository) -> Result<FsckReport, GraphError> {
    let store = repo.store();
    let mut report = FsckReport::default();
    check_workspaces(store, &mut report)?;
    check_commits(store, &mut report)?;
    Ok(report)
}

/// Verify every workspace manifest. Workspace refs point straight at a
/// manifest blob (ADR-0010), so this reads the blob and checks the
/// manifest directly.
fn check_workspaces(store: &GitStore, report: &mut FsckReport) -> Result<(), GraphError> {
    for (reference, hash) in store.list_refs(WORKSPACE_REF_PREFIX)? {
        let origin = Origin::Workspace { reference };
        match store.get(&hash) {
            Ok(Some(bytes)) => check_manifest(store, &origin, &bytes, report),
            Ok(None) => report.push(
                FindingKind::Manifest,
                &origin,
                None,
                format!("workspace manifest blob {hash} is absent from the store"),
            ),
            Err(err) => report.push(
                FindingKind::Manifest,
                &origin,
                None,
                format!("workspace manifest blob {hash} could not be read: {err}"),
            ),
        }
    }
    Ok(())
}

/// Verify every commit reachable from a branch. Follows all parents,
/// deduplicating commits so shared history is checked once.
fn check_commits(store: &GitStore, report: &mut FsckReport) -> Result<(), GraphError> {
    let mut seen = BTreeSet::new();
    for (reference, tip) in store.list_refs(BRANCH_REF_PREFIX)? {
        let mut stack = vec![tip];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let origin = Origin::Commit {
                reference: reference.clone(),
                commit: id,
            };
            match store.read_commit(&id) {
                Ok(Some(commit)) => {
                    check_manifest(store, &origin, &commit.manifest, report);
                    stack.extend(commit.parents);
                }
                Ok(None) => report.push(
                    FindingKind::Commit,
                    &origin,
                    None,
                    format!("commit {id} is referenced by history but absent from the store"),
                ),
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

/// Decode one manifest and verify every map it references.
fn check_manifest(store: &GitStore, origin: &Origin, bytes: &[u8], report: &mut FsckReport) {
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

    verify_map(
        store,
        origin,
        MapId::Nodes,
        &manifest.nodes,
        &manifest,
        report,
    );
    verify_map(
        store,
        origin,
        MapId::Schema,
        &manifest.schema,
        &manifest,
        report,
    );
    verify_map(
        store,
        origin,
        MapId::EdgesFwd,
        &manifest.edges_fwd,
        &manifest,
        report,
    );
    verify_map(
        store,
        origin,
        MapId::EdgesRev,
        &manifest.edges_rev,
        &manifest,
        report,
    );
    for (name, root) in &manifest.indexes {
        verify_map(
            store,
            origin,
            MapId::Index(name.clone()),
            root,
            &manifest,
            report,
        );
    }
    if let Some(conflicts) = &manifest.conflicts {
        verify_map(
            store,
            origin,
            MapId::Conflicts,
            conflicts,
            &manifest,
            report,
        );
    }

    check_edge_symmetry(store, origin, &manifest, report);
}

/// Verify chunk reachability for one map root.
fn verify_map(
    store: &GitStore,
    origin: &Origin,
    map: MapId,
    map_root: &MapRoot,
    manifest: &Manifest,
    report: &mut FsckReport,
) {
    let root = match map_root.to_root(manifest.chunk_params) {
        Ok(root) => root,
        Err(err) => {
            report.push(
                FindingKind::MapRoot,
                origin,
                Some(map),
                format!("map root does not reconstruct: {err}"),
            );
            return;
        }
    };
    for fault in verify_reachable(store, &root) {
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
}

/// Advisory check: the forward and reverse edge maps must describe the same
/// edge set (spec §3.3). Each edge is identified by its canonical forward
/// key, so a reverse entry is matched to the forward entry for the same
/// edge regardless of which map it came from.
///
/// Structural corruption of either edge map has already been reported by
/// [`verify_map`]; if scanning the maps fails here (the same corruption),
/// the advisory is simply skipped rather than double-reported.
fn check_edge_symmetry(
    store: &GitStore,
    origin: &Origin,
    manifest: &Manifest,
    report: &mut FsckReport,
) {
    let snapshot = Snapshot::new(store, manifest.clone());
    let (forward, reverse) = match (snapshot.edges(), snapshot.reverse_edge_keys()) {
        (Ok(forward), Ok(reverse)) => (forward, reverse),
        _ => return,
    };

    let mut forward_ids = BTreeSet::new();
    for (key, _) in &forward {
        if let Ok(id) = key.encode_fwd() {
            forward_ids.insert(id);
        }
    }
    let mut reverse_ids = BTreeSet::new();
    for key in &reverse {
        if let Ok(id) = key.encode_fwd() {
            reverse_ids.insert(id);
        }
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
