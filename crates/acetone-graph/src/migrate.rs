//! History rewrite (`acetone migrate`): re-encode every reachable graph
//! version under a format transform and rebuild the commit graph, producing
//! new hashes.
//!
//! This is the Gate D (ADR-0024) escape hatch that makes the format freeze
//! safe: when a future `format_version` bump changes an encoding, `migrate`
//! walks all history, re-encodes each version, and rewrites the commit graph
//! so a repository can be brought forward (new hashes are expected and
//! accepted pre-1.0). See ADR-0025.
//!
//! The engine here is generic over a [`FormatTransform`]. Per the Gate D
//! decision ("engine now, demo at 0.2"), the only transform shipped today is
//! [`Rechunk`] — a **version-preserving** rebuild under new chunk parameters
//! (chunk parameters are manifest data, so the encoding and `format_version`
//! are unchanged) that nonetheless rewrites every root and commit hash, which
//! is exactly what exercises the engine end to end. A real cross-version
//! transform slots into the same engine at the first 0.2 format change.
//!
//! **Fidelity.** Each commit is rebuilt with its message, author and committer
//! — identity *and* timestamp — preserved verbatim ([`GitStore::rewrite_commit`]),
//! parents remapped to the rewritten commits, and a fresh anchor set for the
//! transformed manifest. `git fsck` stays clean.
//!
//! **Annotated tags.** A ref whose target is an annotated-tag object (or a
//! chain of them — git permits nested tags) is rewritten faithfully: the
//! peeled commit is rewritten like any other, then each tag object in the
//! chain is rewritten innermost-first — a **new** tag object preserving the
//! original name, tagger (identity *and* timestamp) and message, pointing at
//! the rewritten target — and the ref swings to the outermost rewritten tag.
//! A **signed** tag refuses the whole migration up front
//! ([`GitStore::rewrite_tag`], [`acetone_store::StoreError::SignedTag`]):
//! rewriting would invalidate the signature and silently dropping it is
//! forbidden. A ref target that peels to anything that is not a commit is
//! still [`GraphError::NotACommit`].
//!
//! **Signed commits — documented limitation.** A `gpgsig` header on a
//! commit signs bytes that a rewrite necessarily changes, so no rewritten
//! commit can keep its signature valid. Unlike a tag — whose stale
//! signature would be *folded into the rewritten content* as corrupt data,
//! hence the refusal above — [`GitStore::rewrite_commit`] simply does not
//! carry extra headers forward: a signed commit is rewritten as a
//! well-formed **unsigned** commit. This loss is inherent to opting into a
//! history rewrite (migrate is the explicit opt-in path, ADR-0048; the
//! default read-old-write-new path never touches existing commits).
//!
//! **Crash safety — the journalled atomic swing.** The rewrite proceeds in
//! phases so that a process death at *any* point leaves the repository fully
//! old, fully new, or *detectably* in-between — never silently mixed:
//!
//! 1. **Objects.** Rewritten commits, rewritten tag objects and the new
//!    workspace tree are written first. These are pure additions: a crash
//!    here leaves every ref untouched (fully old) and only unreferenced
//!    objects behind.
//! 2. **Journal.** Every planned ref swing — branches, tags *and* the
//!    default workspace ref, uniformly — is recorded as `(ref, old, new)` in
//!    a journal blob, and the journal ref
//!    ([`GraphRefNamespace::migrate_journal_ref`](crate::GraphRefNamespace::migrate_journal_ref))
//!    is created pointing at it. A crash before this ref exists: fully old.
//! 3. **Swing.** All swings are applied as **one** batched ref transaction
//!    ([`GitStore::write_refs_atomic`]): every precondition is checked and
//!    every per-ref lock taken before anything moves, so against concurrent
//!    writers the batch is all-or-nothing. Git's file backend cannot make
//!    the final commit step crash-atomic (loose refs are separate files), so
//!    a death in that narrow window can apply a subset — which the journal
//!    makes *detectable*: [`pending_migration`] reports it, and re-running
//!    `acetone migrate` first completes the journalled swing (each ref still
//!    at its old value is moved to its recorded new value; a ref at neither
//!    is refused with the journal kept) before proceeding.
//! 4. **Cleanup.** The journal ref is deleted. A crash before deletion:
//!    recovery finds every ref already at its new value and just cleans up.
//!
//! **Scope / limitations.** Rewrites the graph's branch and tag namespaces
//! (scoped by [`GraphRefNamespace`](crate::GraphRefNamespace), so a co-tenant
//! migrate never touches code refs); a checked-out **detached HEAD** (not
//! under the branch namespace) is left pointing at its now-superseded commit —
//! the CLI does not expose detached HEAD. Requires a clean, non-merging
//! workspace, which the swing moves to the rewritten head atomically with the
//! refs. A completed migration is deterministic and idempotent: re-running
//! produces the same hashes and is a no-op.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use acetone_model::manifest::{Manifest, MapRoot};
use acetone_prolly::{ChunkParams, bulk_load, scan};
use acetone_store::{
    ChunkStore, CommitStore, GitStore, Hash, MAX_TAG_PEEL_DEPTH, RefStore, RefSwing, RewriteCommit,
    TagObject,
};

use crate::error::GraphError;
use crate::refns::GraphRefNamespace;
use crate::repo::{Repository, WORKTREE_WORKSPACE_REF, manifest_chunk_set, summarise};

/// A transform applied to every graph version during a migration: it maps one
/// version's manifest to a new manifest, writing any new chunks to `store`. It
/// MUST be a pure function of the input version so the migration is
/// deterministic (identical repository + transform ⇒ identical new hashes).
pub trait FormatTransform {
    /// Transform `old` into the new manifest, writing new chunks to `store`.
    fn transform(&self, store: &GitStore, old: &Manifest) -> Result<Manifest, GraphError>;
}

/// Rebuild every map under new chunk parameters. Version-preserving — the key
/// and value encodings are unchanged and `format_version` stays the same,
/// because chunk parameters are manifest data — yet it rewrites every prolly
/// root (spec §3.2: changing the chunking changes every hash). A real
/// operation (retuning chunk size) and the vehicle that exercises the engine.
pub struct Rechunk {
    params: ChunkParams,
}

impl Rechunk {
    /// Rebuild every map under `params`.
    pub fn new(params: ChunkParams) -> Self {
        Rechunk { params }
    }

    /// Build a re-chunk transform from raw chunk parameters, validating them —
    /// so callers (e.g. the CLI) need not depend on `acetone-prolly` directly.
    pub fn from_raw(min_bytes: u32, mask_bits: u32, max_bytes: u32) -> Result<Self, GraphError> {
        Ok(Rechunk::new(ChunkParams::new(
            min_bytes, mask_bits, max_bytes,
        )?))
    }

    /// Read a map under its current parameters and rebuild it under the target
    /// parameters, returning the new root. Contents are unchanged, so the
    /// rebuild is history-independent and deterministic.
    fn rechunk_map(
        &self,
        store: &GitStore,
        old_params: ChunkParams,
        map_root: &MapRoot,
    ) -> Result<MapRoot, GraphError> {
        let root = map_root.to_root(old_params)?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for item in scan(store, &root, ..)? {
            let (key, value) = item?;
            entries.push((key.to_vec(), value.to_vec()));
        }
        let new_root = bulk_load(store, self.params, entries)?;
        Ok(MapRoot::from_root(&new_root))
    }
}

impl FormatTransform for Rechunk {
    fn transform(&self, store: &GitStore, old: &Manifest) -> Result<Manifest, GraphError> {
        let mut indexes = BTreeMap::new();
        for (name, map_root) in &old.indexes {
            indexes.insert(
                name.clone(),
                self.rechunk_map(store, old.chunk_params, map_root)?,
            );
        }
        let conflicts = match &old.conflicts {
            Some(c) => Some(self.rechunk_map(store, old.chunk_params, c)?),
            None => None,
        };
        Ok(Manifest {
            chunk_params: self.params,
            schema: self.rechunk_map(store, old.chunk_params, &old.schema)?,
            nodes: self.rechunk_map(store, old.chunk_params, &old.nodes)?,
            edges_fwd: self.rechunk_map(store, old.chunk_params, &old.edges_fwd)?,
            edges_rev: self.rechunk_map(store, old.chunk_params, &old.edges_rev)?,
            indexes,
            conflicts,
        })
    }
}

/// What a migration rewrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateReport {
    /// Number of commits rewritten.
    pub commits_rewritten: usize,
    /// Number of refs (branches + tags) repointed.
    pub refs_updated: usize,
    /// Number of distinct annotated-tag objects rewritten (a nested chain
    /// counts each object once, however many refs reach it).
    pub tags_rewritten: usize,
}

/// First line of the migration journal blob; versions the format.
const JOURNAL_HEADER: &str = "acetone-migrate-journal v1";

/// The planned ref swings of an in-flight migration, journalled before the
/// swing is performed so an interrupted migration is detectable and
/// recoverable (see the module docs on crash safety). Read one back with
/// [`pending_migration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateJournal {
    /// Every planned swing: branches, tags and the workspace ref, uniformly.
    pub swings: Vec<RefSwing>,
}

impl MigrateJournal {
    /// Serialise the journal: a header line, then one
    /// `<old-hex|-> <new-hex> <name>` line per swing (`-` marks a create —
    /// no old value). Git ref names cannot contain spaces, so the format
    /// splits unambiguously.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::from(JOURNAL_HEADER);
        out.push('\n');
        for swing in &self.swings {
            match &swing.expected {
                Some(old) => out.push_str(&old.to_hex()),
                None => out.push('-'),
            }
            out.push(' ');
            out.push_str(&swing.new.to_hex());
            out.push(' ');
            out.push_str(&swing.name);
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Parse a journal blob written by [`Self::encode`]. Any deviation is a
    /// [`GraphError::Migrate`] — a malformed journal must be surfaced, not
    /// silently treated as "no migration in flight".
    pub fn decode(data: &[u8]) -> Result<Self, GraphError> {
        let text = std::str::from_utf8(data)
            .map_err(|_| GraphError::Migrate("migration journal is not UTF-8".into()))?;
        let mut lines = text.lines();
        if lines.next() != Some(JOURNAL_HEADER) {
            return Err(GraphError::Migrate(
                "migration journal has an unrecognised header".into(),
            ));
        }
        let mut swings = Vec::new();
        for line in lines {
            let (old, rest) = line.split_once(' ').ok_or_else(|| {
                GraphError::Migrate(format!("malformed migration journal line {line:?}"))
            })?;
            let (new, name) = rest.split_once(' ').ok_or_else(|| {
                GraphError::Migrate(format!("malformed migration journal line {line:?}"))
            })?;
            let expected = match old {
                "-" => None,
                hex => Some(Hash::from_hex(hex)?),
            };
            swings.push(RefSwing {
                name: name.to_owned(),
                expected,
                new: Hash::from_hex(new)?,
            });
        }
        Ok(MigrateJournal { swings })
    }
}

/// The journal of a migration interrupted mid-swing, if the repository
/// carries one — `None` for a healthy repository. A `Some` repository is in
/// a *detectably* in-between state: some journalled refs may already point
/// at their rewritten values. Re-running `acetone migrate`
/// ([`rewrite_history`]) completes the swing and clears the journal.
pub fn pending_migration(repo: &Repository) -> Result<Option<MigrateJournal>, GraphError> {
    let journal_ref = repo.namespace().migrate_journal_ref();
    let Some(blob) = repo.store().read_ref(journal_ref)? else {
        return Ok(None);
    };
    let data = repo.store().get(&blob)?.ok_or_else(|| {
        GraphError::Migrate(format!(
            "migration journal blob {blob} is absent from the store"
        ))
    })?;
    Ok(Some(MigrateJournal::decode(&data)?))
}

/// Whether a migration of the graph behind `namespace` may legitimately swing
/// the ref `name`. The allow-list is exactly what [`rewrite_history`] plans:
/// the graph's own branches, tags and private namespace
/// ([`GraphRefNamespace::owns_ref`]), plus the per-worktree workspace pointer
/// [`WORKTREE_WORKSPACE_REF`] — the one legitimate swing target `owns_ref` does
/// not own in the co-tenant layout (it sits outside the graph's private
/// prefixes). Any other ref in a journal — the user's code branch, another
/// co-tenant graph's refs — is foreign and must never be applied by recovery
/// (acetone-w9uu).
fn migrate_may_swing(namespace: &GraphRefNamespace, name: &str) -> bool {
    namespace.owns_ref(name) || name == WORKTREE_WORKSPACE_REF
}

/// Validate every journalled swing before any of it is applied — the security
/// boundary for a migrate journal, which is untrusted on-disk state that may
/// have been crafted (acetone-w9uu). A journal is refused, and **kept**, if any
/// swing:
///
/// 1. names a ref this graph may not swing ([`migrate_may_swing`]) — e.g. a
///    crafted journal naming the user's `refs/heads/main` in co-tenant mode,
///    which a routine `migrate` would otherwise repoint before its guards; or
/// 2. moves a ref to a target that names no object in the store — defence in
///    depth, since [`Hash::from_hex`] accepts any 40-hex string, so a bogus
///    hash could otherwise dangle a ref.
///
/// The refusal returns before any ref moves, so the repository and the journal
/// are left exactly as found — the same refuse-and-keep semantics as a ref
/// moved externally. The same check runs when [`rewrite_history`] first
/// journals its freshly-planned swings, so a legitimate run and a recovered run
/// share one validation point.
fn validate_journal_swings(repo: &Repository, swings: &[RefSwing]) -> Result<(), GraphError> {
    let namespace = repo.namespace();
    for swing in swings {
        if !migrate_may_swing(namespace, &swing.name) {
            return Err(GraphError::Migrate(format!(
                "migration journal names ref {:?}, which is not owned by this graph — refusing to \
                 apply it. A migration only ever swings the graph's own branches, tags and \
                 workspace, so a journal naming a foreign ref is not one this graph wrote. \
                 Resolve it by hand, then delete {:?} to clear the journal",
                swing.name,
                namespace.migrate_journal_ref(),
            )));
        }
        if !repo.store().contains_object(&swing.new)? {
            return Err(GraphError::Migrate(format!(
                "migration journal swings ref {:?} to {}, which names no object in the store — \
                 refusing to dangle the ref. Resolve it by hand, then delete {:?} to clear the \
                 journal",
                swing.name,
                swing.new.to_hex(),
                namespace.migrate_journal_ref(),
            )));
        }
    }
    Ok(())
}

/// Complete the ref swing of an interrupted migration, if one is journalled:
/// every journalled ref still at its old value is moved to its recorded new
/// value (in one batched transaction), then the journal is cleared. A ref at
/// **neither** journalled value was moved by something else while the
/// migration lay interrupted — that is refused, keeping the journal, because
/// completing the swing would discard the foreign update. Returns whether a
/// journal was found and recovered.
fn recover_pending(repo: &Repository) -> Result<bool, GraphError> {
    let Some(journal) = pending_migration(repo)? else {
        return Ok(false);
    };
    // The journal is untrusted on-disk state: refuse (and keep) any journal
    // that names a foreign ref or a non-existent target before applying a
    // single swing (acetone-w9uu).
    validate_journal_swings(repo, &journal.swings)?;
    let store = repo.store();
    let mut remaining: Vec<RefSwing> = Vec::new();
    for swing in &journal.swings {
        let current = store.read_ref(&swing.name)?;
        if current.as_ref() == Some(&swing.new) {
            continue; // already swung before the interruption
        }
        if current == swing.expected {
            remaining.push(swing.clone());
        } else {
            return Err(GraphError::Migrate(format!(
                "interrupted migration: ref {:?} now points at {} — neither its pre-migration \
                 value ({}) nor its journalled rewrite ({}); it was moved while the migration \
                 lay interrupted. Resolve the ref by hand, then delete {:?} to clear the journal",
                swing.name,
                current.map_or_else(|| "nothing".to_owned(), |h| h.to_hex()),
                swing
                    .expected
                    .map_or_else(|| "created".to_owned(), |h| h.to_hex()),
                swing.new.to_hex(),
                repo.namespace().migrate_journal_ref(),
            )));
        }
    }
    store.write_refs_atomic(&remaining)?;
    store.delete_ref(repo.namespace().migrate_journal_ref())?;
    Ok(true)
}

/// One ref to migrate: its old target, the chain of annotated-tag objects
/// from the ref to the commit (outermost first; empty for a ref pointing
/// straight at a commit), and the peeled commit.
struct RefPlan {
    name: String,
    old_target: Hash,
    tag_chain: Vec<(Hash, TagObject)>,
    commit: Hash,
}

/// Rewrite all history reachable from the graph's branch and tag namespaces
/// under `transform`, producing new hashes, and repoint every such ref — and
/// the default workspace — at the rewritten commits (rewriting any
/// annotated-tag objects along the way).
///
/// Requires a clean, non-merging workspace. All ref swings, the workspace
/// included, are journalled and applied as one batched transaction — see the
/// module docs on crash safety. If a previous migration was interrupted
/// mid-swing, this first completes it from the journal. Deterministic: the
/// same repository and transform always yield the same new hashes, so
/// re-running is idempotent.
pub fn rewrite_history(
    repo: &Repository,
    transform: &dyn FormatTransform,
) -> Result<MigrateReport, GraphError> {
    // Complete any interrupted migration first: the crashed state legitimately
    // reads as dirty (refs swung, workspace lagging), so recovery must run
    // before the guards. Recovery-then-rerun leans on transform idempotence:
    // after the journalled swing completes, THIS invocation's transform runs
    // over the already-migrated history, which is a no-op for an idempotent
    // transform like `Rechunk` (same params ⇒ same hashes). A future
    // non-idempotent transform (one whose double application differs from a
    // single one) must not be re-applied blindly here — it would need to
    // detect already-current commits or record the transform in the journal.
    recover_pending(repo)?;

    // The rewrite resets the workspace, so refuse to run over uncommitted or
    // mid-merge state.
    if repo.workspace_manifest()?.conflicts.is_some() {
        return Err(GraphError::MergeInProgress);
    }
    if repo.is_dirty()? {
        return Err(GraphError::DirtyWorkspace);
    }

    let store = repo.store();
    let namespace = repo.namespace();
    let mut refs: Vec<(String, Hash)> = Vec::new();
    refs.extend(store.list_refs(namespace.branch_prefix())?);
    refs.extend(store.list_refs(namespace.tag_prefix())?);

    // Peel every ref through any annotated-tag chain to its commit, refusing
    // up front — before any object is written or ref moved — anything the
    // migration could not rewrite completely (safe-by-default): a signed tag,
    // a target that is not a commit, a pathological tag chain.
    let plans: Vec<RefPlan> = refs
        .iter()
        .map(|(name, target)| peel_ref(store, name, *target))
        .collect::<Result<_, _>>()?;

    let order = topo_order(store, &plans)?;

    // Rewrite each commit, parents-first, remapping parents to their rewrites.
    let mut mapping: HashMap<Hash, Hash> = HashMap::new();
    for old_id in &order {
        let commit = store.read_commit(old_id)?.ok_or_else(|| {
            GraphError::Migrate(format!("reachable commit {old_id} vanished mid-rewrite"))
        })?;
        let old_manifest = Manifest::decode(&commit.manifest)?;
        let new_manifest = transform.transform(store, &old_manifest)?;
        let manifest_bytes = new_manifest.encode();
        let anchors = manifest_chunk_set(store, &new_manifest)?;
        let summary = summarise(store, &new_manifest)?;
        let new_parents: Vec<Hash> = commit
            .parents
            .iter()
            .map(|p| {
                mapping.get(p).copied().ok_or_else(|| {
                    GraphError::Migrate(format!("parent {p} was not rewritten before its child"))
                })
            })
            .collect::<Result<_, _>>()?;
        let mut spec = RewriteCommit::new(
            &manifest_bytes,
            &summary,
            &commit.message,
            &commit.author,
            &commit.committer,
        );
        spec.parents = &new_parents;
        spec.anchors = &anchors;
        let new_id = store.rewrite_commit(&spec)?;
        mapping.insert(*old_id, new_id);
    }

    // Rewrite annotated-tag chains innermost-first (objects only — no ref
    // moves yet) and compute each ref's new target. Chains are memoised per
    // tag object, so a tag reachable through several refs is rewritten once
    // (and counted once).
    let mut tag_mapping: HashMap<Hash, Hash> = HashMap::new();
    let mut swings: Vec<RefSwing> = Vec::new();
    for plan in &plans {
        let mut new_target = *mapping.get(&plan.commit).ok_or_else(|| {
            GraphError::Migrate(format!(
                "ref {:?} target {} was not rewritten",
                plan.name, plan.commit
            ))
        })?;
        for (old_tag_id, tag) in plan.tag_chain.iter().rev() {
            new_target = match tag_mapping.get(old_tag_id) {
                Some(new_id) => *new_id,
                None => {
                    let new_id = store.rewrite_tag(tag, &new_target)?;
                    tag_mapping.insert(*old_tag_id, new_id);
                    new_id
                }
            };
        }
        if new_target != plan.old_target {
            swings.push(RefSwing {
                name: plan.name.clone(),
                expected: Some(plan.old_target),
                new: new_target,
            });
        }
    }

    // The default workspace follows the rewritten head in the same swing, so
    // the repository moves in one step (the workspace was clean, i.e. equal
    // to the old head). A *virtual* workspace — no per-worktree or legacy
    // ref materialised, e.g. a fresh `git worktree add` (acetone-ayq) — needs
    // no swing and stays virtual: it reads the checked-out commit's manifest,
    // so it follows the branch swing automatically. When only the legacy
    // shared ref exists, the swing materialises the per-worktree ref at the
    // rewritten head (the same shadowing upgrade every workspace write
    // performs, ADR-0014).
    if let Some(old_head) = repo.head_commit()?
        && repo.workspace_ref_target()?.is_some()
    {
        let new_head = *mapping.get(&old_head).ok_or_else(|| {
            GraphError::Migrate(format!("head commit {old_head} was not rewritten"))
        })?;
        let manifest_hash = repo.commit_manifest_hash(&new_head)?;
        let tree = repo.workspace_tree_for(&manifest_hash)?;
        let current = store.read_ref(WORKTREE_WORKSPACE_REF)?;
        if current != Some(tree) {
            swings.push(RefSwing {
                name: WORKTREE_WORKSPACE_REF.to_owned(),
                expected: current,
                new: tree,
            });
        }
    }

    // Journal, swing atomically, clear the journal (module docs, phases 2-4).
    if !swings.is_empty() {
        // Same validation the recovery path applies, so a legitimate run and a
        // recovered run share one point of truth for "what may migrate swing".
        // These freshly-planned swings are trusted by construction; the check
        // is a cheap internal invariant guard (acetone-w9uu).
        validate_journal_swings(repo, &swings)?;
        let journal = MigrateJournal {
            swings: swings.clone(),
        };
        let blob = store.put(&journal.encode())?;
        let journal_ref = namespace.migrate_journal_ref();
        store.write_ref(journal_ref, None, &blob)?;
        store.write_refs_atomic(&swings)?;
        store.delete_ref(journal_ref)?;
    }

    Ok(MigrateReport {
        commits_rewritten: order.len(),
        refs_updated: refs.len(),
        tags_rewritten: tag_mapping.len(),
    })
}

/// Peel one ref through its (possibly empty) chain of annotated-tag objects
/// to the commit behind it, collecting the chain for rewriting. Refusals are
/// deliberate and total, before the migration writes anything:
/// a signed tag anywhere in the chain ([`acetone_store::StoreError::SignedTag`],
/// with the tag's recorded name), a chain deeper than [`MAX_TAG_PEEL_DEPTH`],
/// or a peeled target that is not a readable commit
/// ([`GraphError::NotACommit`]).
fn peel_ref(store: &GitStore, name: &str, target: Hash) -> Result<RefPlan, GraphError> {
    let mut tag_chain: Vec<(Hash, TagObject)> = Vec::new();
    let mut current = target;
    loop {
        if tag_chain.len() >= MAX_TAG_PEEL_DEPTH {
            return Err(GraphError::Migrate(format!(
                "ref {name:?} is an annotated-tag chain deeper than {MAX_TAG_PEEL_DEPTH} levels"
            )));
        }
        match store.read_tag(&current)? {
            Some(tag) => {
                if tag.signed {
                    return Err(GraphError::Migrate(format!(
                        "ref {name:?} is (or wraps) the signed tag {:?}: rewriting it would \
                         invalidate the signature. Delete or replace the tag, then re-run \
                         migrate",
                        tag.name
                    )));
                }
                let next = tag.target;
                tag_chain.push((current, tag));
                current = next;
            }
            None => break,
        }
    }
    // Anything that is not readable as a commit here — a blob, a tree, or a
    // genuinely damaged object — aborts the whole migration before any
    // object is rewritten or any ref is swung.
    if !matches!(store.read_commit(&current), Ok(Some(_))) {
        return Err(GraphError::NotACommit {
            name: name.to_owned(),
        });
    }
    Ok(RefPlan {
        name: name.to_owned(),
        old_target: target,
        tag_chain,
        commit: current,
    })
}

/// Collect every commit reachable from the plans' peeled commits and return
/// them in a topological order with parents before children.
fn topo_order(store: &GitStore, plans: &[RefPlan]) -> Result<Vec<Hash>, GraphError> {
    // Reachable set with each commit's parents.
    let mut parents_of: HashMap<Hash, Vec<Hash>> = HashMap::new();
    let mut stack: Vec<Hash> = plans.iter().map(|plan| plan.commit).collect();
    while let Some(h) = stack.pop() {
        if parents_of.contains_key(&h) {
            continue;
        }
        let commit = store
            .read_commit(&h)?
            .ok_or_else(|| GraphError::Migrate(format!("reachable commit {h} is absent")))?;
        for parent in &commit.parents {
            stack.push(*parent);
        }
        parents_of.insert(h, commit.parents);
    }

    // Kahn's algorithm; a sorted ready-set makes the order deterministic.
    let mut children: HashMap<Hash, Vec<Hash>> = HashMap::new();
    let mut indegree: HashMap<Hash, usize> = HashMap::new();
    for (h, parents) in &parents_of {
        indegree.insert(*h, parents.len());
        for parent in parents {
            children.entry(*parent).or_default().push(*h);
        }
    }
    let mut ready: BTreeSet<Hash> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(h, _)| *h)
        .collect();
    let mut order = Vec::with_capacity(parents_of.len());
    while let Some(h) = ready.iter().next().copied() {
        ready.remove(&h);
        order.push(h);
        if let Some(cs) = children.get(&h) {
            for c in cs {
                let d = indegree
                    .get_mut(c)
                    .expect("every child has an indegree entry");
                *d -= 1;
                if *d == 0 {
                    ready.insert(*c);
                }
            }
        }
    }
    if order.len() != parents_of.len() {
        return Err(GraphError::Migrate(
            "commit graph has a cycle (impossible for git history)".into(),
        ));
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> Hash {
        Hash::from_bytes(&[byte; 20]).expect("hash")
    }

    #[test]
    fn journal_round_trips_including_creates() {
        let journal = MigrateJournal {
            swings: vec![
                RefSwing {
                    name: "refs/heads/main".into(),
                    expected: Some(hash(1)),
                    new: hash(2),
                },
                RefSwing {
                    name: "refs/worktree/acetone/workspace".into(),
                    expected: None, // a create
                    new: hash(3),
                },
            ],
        };
        let decoded = MigrateJournal::decode(&journal.encode()).expect("decode");
        assert_eq!(decoded, journal);
    }

    #[test]
    fn journal_rejects_unknown_headers_and_malformed_lines() {
        assert!(MigrateJournal::decode(b"acetone-migrate-journal v999\n").is_err());
        assert!(MigrateJournal::decode(b"").is_err());
        assert!(MigrateJournal::decode(b"\xff\xfe").is_err());
        let missing_field = format!("{JOURNAL_HEADER}\nonly-one-field\n");
        assert!(MigrateJournal::decode(missing_field.as_bytes()).is_err());
        let bad_hex = format!("{JOURNAL_HEADER}\nzz zz refs/heads/main\n");
        assert!(MigrateJournal::decode(bad_hex.as_bytes()).is_err());
    }
}
