//! [`GitStore`]: the reference [`ChunkStore`]/[`RefStore`]/[`CommitStore`]
//! implementation over a real git object database (spec §3.1, ADR-0002).
//!
//! # Trust model
//!
//! Repositories are opened **reduced-trust**, suitable for clones of hostile
//! origin:
//!
//! - [`gix::open::Options::isolated`] loads *no* configuration from the
//!   environment, no system/global/user config files, no git-binary config,
//!   and follows no `include`/`includeIf` directives. Only the repository's
//!   own config file is parsed (gix requires it for correctness, e.g. the
//!   object format).
//! - The trust level is pinned to [`gix::sec::Trust::Reduced`] rather than
//!   derived from filesystem ownership, so even a repository we own is
//!   treated as untrusted: gix then refuses to honour trust-sensitive
//!   repo-local config values (anything naming programs or paths).
//! - The crate compiles gix with a minimal feature set: the `command`
//!   feature (spawning the git binary or config-named programs), network
//!   clients, credential helpers and attribute/filter pipelines are all
//!   disabled, so none of the code paths that execute programs named in
//!   repository config are enabled in this build. (The low-level
//!   `gix-command` crate is still compiled transitively via
//!   `gix-transport`, but no enabled feature invokes it.)
//!
//! What reduced trust does *not* do: it cannot stop gix parsing the
//! repo-local config file itself, and object/ref decoding still runs on
//! untrusted bytes — which is why every decode path here returns `Result`
//! and object sizes are checked against a hard cap before materialisation.

use std::path::Path;

use bytes::Bytes;

use crate::error::StoreError;
use crate::hash::Hash;
use crate::store::{ChunkStore, Commit, CommitStore, NewCommit, RefStore, RewriteCommit};

/// Default hard cap on the size of any single object this store will
/// materialise: 64 MiB.
///
/// Prolly chunks target ~4 KiB (spec §3.2) and manifests are small, so any
/// legitimate acetone object is orders of magnitude below this; a hostile
/// multi-GiB blob is rejected from its header before allocation. Override
/// via [`GitStoreOptions::max_chunk_size`] if a deployment needs more.
pub const DEFAULT_MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// The object format (hash function) of a repository (spec §3.1: SHA-1
/// default, SHA-256 supported).
///
/// Only consulted when *creating* a repository; opening reads the format
/// from the repository itself, and [`Hash`] is opaque either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ObjectFormat {
    /// Git's legacy 160-bit format — the default, maximally interoperable.
    #[default]
    Sha1,
    /// The 256-bit format (`extensions.objectFormat = sha256`).
    Sha256,
}

/// Construction parameters for a [`GitStore`].
///
/// `#[non_exhaustive]`: construct with [`GitStoreOptions::default`] and
/// assign the fields to override, so new options never break callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GitStoreOptions {
    /// Hard cap in bytes on any single object read or written; see
    /// [`DEFAULT_MAX_CHUNK_SIZE`]. Enforced by [`ChunkStore::put`] and, on
    /// every read path, checked against the object header *before* the
    /// object is materialised.
    pub max_chunk_size: u64,
    /// Object format for newly created repositories (ignored on open).
    pub object_format: ObjectFormat,
}

impl Default for GitStoreOptions {
    fn default() -> Self {
        GitStoreOptions {
            max_chunk_size: DEFAULT_MAX_CHUNK_SIZE,
            object_format: ObjectFormat::default(),
        }
    }
}

/// A chunk store backed by the object database of one git repository.
///
/// Chunks are git blobs addressed by their object ID; refs are git refs;
/// version snapshots are git commits. See the module docs for the trust
/// model applied when opening repositories.
///
/// Not `Sync`: gix repositories carry per-instance caches. Open one
/// `GitStore` per thread (they may all point at the same repository on
/// disk; ref updates are atomic across instances and processes).
///
/// # Locking and crash recovery
///
/// [`RefStore::write_ref`] serialises all acetone writers on a repository
/// through a lock file, `<common_dir>/acetone-refs.lock`, held only for
/// the duration of one ref update and removed when the guard drops. If a
/// process is killed while holding it (SIGKILL, power loss), the stale
/// file makes every subsequent `write_ref` back off for ~5 seconds and
/// then fail with [`StoreError::Backend`] rather than hang or corrupt
/// anything. Recovery is manual and safe once no acetone process is
/// running against the repository: delete
/// `<git-dir>/acetone-refs.lock` (for worktrees, in the common/main git
/// dir). Reads are never blocked by this lock.
pub struct GitStore {
    repo: gix::Repository,
    max_chunk_size: u64,
}

impl std::fmt::Debug for GitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitStore")
            .field("git_dir", &self.repo.path())
            .field("max_chunk_size", &self.max_chunk_size)
            .finish()
    }
}

impl GitStore {
    /// Initialise a new bare git repository at `path` and open it as a
    /// store with default options.
    pub fn create(path: &Path) -> Result<Self, StoreError> {
        Self::create_with(path, GitStoreOptions::default())
    }

    /// Initialise a new bare git repository at `path` and open it as a
    /// store.
    ///
    /// [`GitStoreOptions::object_format`] selects the repository's hash
    /// function; SHA-1 repositories carry no `objectFormat` extension for
    /// maximum interoperability, SHA-256 ones declare it as git does.
    pub fn create_with(path: &Path, options: GitStoreOptions) -> Result<Self, StoreError> {
        let create_options = gix::create::Options {
            destination_must_be_empty: None,
            fs_capabilities: None,
            // `None` means legacy SHA-1 with no extensions.objectFormat
            // entry, exactly like `git init`.
            object_hash: match options.object_format {
                ObjectFormat::Sha1 => None,
                ObjectFormat::Sha256 => Some(gix::hash::Kind::Sha256),
            },
        };
        gix::ThreadSafeRepository::init(path, gix::create::Kind::Bare, create_options)
            .map_err(|e| StoreError::backend("initialising repository", e))?;
        // Reopen through the one reduced-trust code path so a freshly
        // created store behaves identically to an opened one.
        Self::open_with(path, options)
    }

    /// Open an existing git repository (bare or not) as a store with
    /// default options.
    ///
    /// The repository is opened reduced-trust (see the module docs): safe
    /// on clones of hostile origin.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with(path, GitStoreOptions::default())
    }

    /// Open an existing git repository as a store.
    pub fn open_with(path: &Path, options: GitStoreOptions) -> Result<Self, StoreError> {
        let open_options = gix::open::Options::isolated().with(gix::sec::Trust::Reduced);
        let repo = gix::open_opts(path, open_options)
            .map_err(|e| StoreError::backend("opening repository", e))?;
        Ok(GitStore {
            repo,
            max_chunk_size: options.max_chunk_size,
        })
    }

    /// The repository's common git directory (shared by all worktrees,
    /// like refs). Repository-wide coordination — e.g. the store's
    /// millisecond-scale `acetone-refs.lock` — belongs here.
    pub fn common_dir(&self) -> &Path {
        self.repo.common_dir()
    }

    /// This worktree's own git directory. For a linked worktree this is
    /// `<common>/worktrees/<id>`; for a repository with no linked
    /// worktrees it coincides with [`common_dir`](Self::common_dir).
    /// Per-worktree coordination — the single-writer lock (ADR-0014) —
    /// belongs here so writers in different worktrees are independent.
    pub fn git_dir(&self) -> &Path {
        self.repo.path()
    }

    /// Build a workspace tree `{.acetone/: {chunks/: <anchor tree>, manifest:
    /// <blob>}}` — the commit-tree shape minus the root README — and return
    /// its object id (ADR-0023). The `.acetone/chunks/` anchor tree makes
    /// git's reachability walk keep every chunk the manifest references, so
    /// uncommitted working state survives a foreign `git gc` (acetone-huo,
    /// amends ADR-0010). The workspace ref then points at this tree instead of
    /// the bare manifest blob.
    pub fn write_workspace_tree(
        &self,
        manifest: &[u8],
        anchors: &[Hash],
    ) -> Result<Hash, StoreError> {
        use gix::objs::tree::{Entry, EntryKind};

        let manifest_id = self.write_blob_capped(manifest, "writing workspace manifest")?;
        let acetone_oid = self.write_acetone_subtree(manifest_id.oid(), anchors)?;
        let entries = vec![Entry {
            mode: EntryKind::Tree.into(),
            filename: ACETONE_DIR.into(),
            oid: acetone_oid,
        }];
        let tree_id = self
            .repo
            .write_object(&gix::objs::Tree { entries })
            .map_err(|e| StoreError::backend("writing workspace tree", e))?
            .detach();
        Ok(Hash::from_oid(tree_id))
    }

    /// Resolve the manifest blob a workspace ref points at. The ref value is
    /// either a workspace tree (huo) whose `.acetone/manifest` entry is the
    /// blob (ADR-0023), or — for a pre-huo workspace — the manifest blob
    /// directly. Lets the graph layer read a workspace uniformly across the
    /// migration.
    pub fn workspace_manifest_hash(&self, ref_value: &Hash) -> Result<Hash, StoreError> {
        let header = self.find_header(ref_value)?.ok_or_else(|| {
            StoreError::corrupt(
                "workspace ref",
                "referenced object is absent from the store",
            )
        })?;
        match header.kind() {
            gix::object::Kind::Blob => Ok(*ref_value),
            gix::object::Kind::Tree => {
                let data = self.read_object_checked(
                    ref_value,
                    &header,
                    gix::object::Kind::Tree,
                    "workspace tree",
                )?;
                let tree = gix::objs::TreeRef::from_bytes(&data, self.repo.object_hash())
                    .map_err(|e| StoreError::corrupt("workspace tree", e.to_string()))?;
                self.root_manifest_hash(&tree, "workspace tree")
            }
            other => Err(StoreError::corrupt(
                "workspace ref",
                format!("unexpected object kind {other:?}"),
            )),
        }
    }

    /// Write one blob, enforcing the size cap; `what` names the blob's
    /// role in error context.
    fn write_blob_capped(&self, data: &[u8], what: &'static str) -> Result<Hash, StoreError> {
        let size = data.len() as u64;
        if size > self.max_chunk_size {
            return Err(StoreError::ObjectTooLarge {
                size,
                limit: self.max_chunk_size,
            });
        }
        let id = self
            .repo
            .write_blob(data)
            .map_err(|e| StoreError::backend(what, e))?;
        Ok(Hash::from_oid(id.detach()))
    }

    /// The underlying repository — crate-internal, for the consolidation
    /// module (ADR-0011), which walks objects and installs packs directly.
    pub(crate) fn repo(&self) -> &gix::Repository {
        &self.repo
    }

    /// The repository's object format, sizing pack OIDs and checksums.
    pub(crate) fn object_hash(&self) -> gix::hash::Kind {
        self.repo.object_hash()
    }

    /// Read any object (of any git kind) after checking its announced size
    /// against the cap — the consolidation read path, which handles commits,
    /// trees, tags and blobs uniformly. `Ok(None)` if absent.
    pub(crate) fn read_any_capped(
        &self,
        hash: &Hash,
    ) -> Result<Option<(gix::object::Kind, Vec<u8>)>, StoreError> {
        let Some(header) = self.find_header(hash)? else {
            return Ok(None);
        };
        if header.size() > self.max_chunk_size {
            return Err(StoreError::ObjectTooLarge {
                size: header.size(),
                limit: self.max_chunk_size,
            });
        }
        let object = self
            .repo
            .find_object(hash.oid())
            .map_err(|e| StoreError::backend("reading object for consolidation", e))?;
        let object = object.detach();
        Ok(Some((object.kind, object.data)))
    }

    /// Look up an object header, translating "not found" to `None`.
    fn find_header(&self, hash: &Hash) -> Result<Option<gix::odb::find::Header>, StoreError> {
        self.repo
            .try_find_header(hash.oid())
            .map_err(|e| StoreError::backend("reading object header", e))
    }

    /// Check an object header against the size cap and expected kind, then
    /// return the object's bytes. Callers have already established the
    /// object exists.
    fn read_object_checked(
        &self,
        hash: &Hash,
        header: &gix::odb::find::Header,
        expected: gix::object::Kind,
        expected_name: &'static str,
    ) -> Result<Vec<u8>, StoreError> {
        if header.kind() != expected {
            return Err(StoreError::WrongObjectKind {
                hash: *hash,
                expected: expected_name,
                actual: header.kind().to_string(),
            });
        }
        if header.size() > self.max_chunk_size {
            return Err(StoreError::ObjectTooLarge {
                size: header.size(),
                limit: self.max_chunk_size,
            });
        }
        let object = self
            .repo
            .find_object(hash.oid())
            .map_err(|e| StoreError::backend("reading object", e))?;
        Ok(object.detach().data)
    }

    /// Build the `chunks/` anchor tree for a commit: a two-level tree of
    /// `<hh>/<rest-of-hex>` entries referencing every anchored chunk blob
    /// (the pattern proven in the Phase 0 spike). Tree entries reference
    /// the existing blobs, so this costs no chunk storage, and sharding by
    /// the first hex byte lets successive versions share unchanged shard
    /// trees. Every anchor is verified to exist as a blob so the resulting
    /// commit is guaranteed connected under `git fsck`.
    fn write_anchor_tree(&self, anchors: &[Hash]) -> Result<gix::ObjectId, StoreError> {
        use gix::objs::tree::{Entry, EntryKind};

        let mut oids: Vec<gix::ObjectId> = Vec::with_capacity(anchors.len());
        for hash in anchors {
            match self.find_header(hash)? {
                None => {
                    return Err(StoreError::InvalidAnchor {
                        hash: *hash,
                        reason: "object is not in the store",
                    });
                }
                Some(header) if header.kind() != gix::object::Kind::Blob => {
                    return Err(StoreError::InvalidAnchor {
                        hash: *hash,
                        reason: "only blobs (chunks) can be anchored",
                    });
                }
                Some(_) => oids.push(hash.oid()),
            }
        }
        oids.sort_unstable();
        oids.dedup();

        // Group into shards; `oids` is sorted, so shards come out in git
        // tree order and so do the entries within each shard.
        let mut shards: Vec<(String, Vec<Entry>)> = Vec::new();
        for oid in oids {
            let hex = oid.to_string();
            let (prefix, rest) = hex.split_at(2);
            let entry = Entry {
                mode: EntryKind::Blob.into(),
                filename: rest.into(),
                oid,
            };
            match shards.last_mut() {
                Some((shard_prefix, entries)) if shard_prefix == prefix => entries.push(entry),
                _ => shards.push((prefix.to_owned(), vec![entry])),
            }
        }

        let mut top_entries = Vec::with_capacity(shards.len());
        for (prefix, entries) in shards {
            let shard_id = self
                .repo
                .write_object(&gix::objs::Tree { entries })
                .map_err(|e| StoreError::backend("writing anchor shard tree", e))?
                .detach();
            top_entries.push(Entry {
                mode: EntryKind::Tree.into(),
                filename: prefix.into(),
                oid: shard_id,
            });
        }
        Ok(self
            .repo
            .write_object(&gix::objs::Tree {
                entries: top_entries,
            })
            .map_err(|e| StoreError::backend("writing anchor tree", e))?
            .detach())
    }

    /// Build the `.acetone/` subtree that holds acetone's machine-readable
    /// state — `{chunks/: <anchor tree> (iff anchors), manifest: <blob>}`
    /// (ADR-0023) — and return its object id. Shared verbatim by commit and
    /// workspace trees, so identical contents dedupe to one tree object.
    fn write_acetone_subtree(
        &self,
        manifest_oid: gix::ObjectId,
        anchors: &[Hash],
    ) -> Result<gix::ObjectId, StoreError> {
        use gix::objs::tree::{Entry, EntryKind};

        // Git tree order: "chunks" < "manifest" byte-wise.
        let mut entries = Vec::new();
        if !anchors.is_empty() {
            entries.push(Entry {
                mode: EntryKind::Tree.into(),
                filename: ANCHORS_ENTRY.into(),
                oid: self.write_anchor_tree(anchors)?,
            });
        }
        entries.push(Entry {
            mode: EntryKind::Blob.into(),
            filename: MANIFEST_ENTRY.into(),
            oid: manifest_oid,
        });
        Ok(self
            .repo
            .write_object(&gix::objs::Tree { entries })
            .map_err(|e| StoreError::backend("writing .acetone subtree", e))?
            .detach())
    }

    /// Write the commit tree `{.acetone/: {chunks/?, manifest}, README.md}`
    /// (ADR-0023) and return its object id. Shared by [`Self::create_commit`]
    /// and [`Self::rewrite_commit`].
    fn write_commit_tree_oid(
        &self,
        manifest: &[u8],
        summary: &str,
        anchors: &[Hash],
    ) -> Result<gix::ObjectId, StoreError> {
        use gix::objs::tree::{Entry, EntryKind};

        let manifest_id = self.write_blob_capped(manifest, "writing manifest")?;
        let summary_id = self.write_blob_capped(summary.as_bytes(), "writing summary")?;
        let acetone_oid = self.write_acetone_subtree(manifest_id.oid(), anchors)?;

        // Root entries must be in git tree order; ".acetone" (a directory,
        // compared as ".acetone/") < "README.md" byte-wise (ADR-0023).
        let entries = vec![
            Entry {
                mode: EntryKind::Tree.into(),
                filename: ACETONE_DIR.into(),
                oid: acetone_oid,
            },
            Entry {
                mode: EntryKind::Blob.into(),
                filename: SUMMARY_ENTRY.into(),
                oid: summary_id.oid(),
            },
        ];
        Ok(self
            .repo
            .write_object(&gix::objs::Tree { entries })
            .map_err(|e| StoreError::backend("writing commit tree", e))?
            .detach())
    }

    /// Rewrite one commit for a history migration (`acetone migrate`),
    /// preserving its message, author and committer (identity **and**
    /// timestamp) verbatim while taking a transformed manifest and
    /// already-rewritten parents. Unlike [`Self::create_commit`] this does not
    /// restamp the time or re-assemble trailers — the message is written as
    /// given — so the rewritten history keeps its dates and authorship. The
    /// tree is still built from the manifest, summary and anchors, and every
    /// anchor is verified present, so the result is `git fsck`-connected.
    pub fn rewrite_commit(&self, spec: &RewriteCommit<'_>) -> Result<Hash, StoreError> {
        let author = git_identity(spec.author)?;
        let committer = git_identity(spec.committer)?;
        let body = spec.message.trim_end();
        if body.is_empty() {
            return Err(StoreError::Corrupt {
                context: "rewritten commit",
                reason: "commit message must not be empty".into(),
            });
        }
        let message = format!("{body}\n");
        let tree_id = self.write_commit_tree_oid(spec.manifest, spec.summary, spec.anchors)?;
        let commit_object = gix::objs::Commit {
            tree: tree_id,
            parents: spec.parents.iter().map(Hash::oid).collect(),
            author,
            committer,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let commit_id = self
            .repo
            .write_object(&commit_object)
            .map_err(|e| StoreError::backend("writing rewritten commit", e))?
            .detach();
        Ok(Hash::from_oid(commit_id))
    }

    /// Resolve the `manifest` blob hash from a commit- or workspace-tree root
    /// by descending into the reserved `.acetone/` directory (ADR-0023).
    /// Unknown sibling entries at the root and within `.acetone/` are ignored
    /// (forward compatibility). `what` names the tree in error context.
    fn root_manifest_hash(
        &self,
        root: &gix::objs::TreeRef<'_>,
        what: &'static str,
    ) -> Result<Hash, StoreError> {
        let acetone = root
            .entries
            .iter()
            .find(|entry| entry.filename == ACETONE_DIR)
            .ok_or_else(|| StoreError::corrupt(what, "no `.acetone` directory in tree"))?;
        if !acetone.mode.is_tree() {
            return Err(StoreError::corrupt(
                what,
                "`.acetone` entry is not a directory",
            ));
        }
        let acetone_hash = Hash::from_oid(acetone.oid.to_owned());
        let header = self
            .find_header(&acetone_hash)?
            .ok_or_else(|| StoreError::corrupt(what, "`.acetone` subtree object is missing"))?;
        let data =
            self.read_object_checked(&acetone_hash, &header, gix::object::Kind::Tree, what)?;
        let sub = gix::objs::TreeRef::from_bytes(&data, self.repo.object_hash())
            .map_err(|e| StoreError::corrupt(what, e.to_string()))?;
        let manifest = sub
            .entries
            .iter()
            .find(|entry| entry.filename == MANIFEST_ENTRY)
            .ok_or_else(|| StoreError::corrupt(what, "no `manifest` entry under `.acetone`"))?;
        if !manifest.mode.is_blob() {
            return Err(StoreError::corrupt(
                what,
                "`.acetone/manifest` entry is not a blob",
            ));
        }
        Ok(Hash::from_oid(manifest.oid.to_owned()))
    }
}

impl ChunkStore for GitStore {
    fn put(&self, data: &[u8]) -> Result<Hash, StoreError> {
        self.write_blob_capped(data, "writing chunk")
    }

    fn get(&self, hash: &Hash) -> Result<Option<Bytes>, StoreError> {
        match self.find_header(hash)? {
            None => Ok(None),
            Some(header) => {
                let data =
                    self.read_object_checked(hash, &header, gix::object::Kind::Blob, "blob")?;
                Ok(Some(Bytes::from(data)))
            }
        }
    }

    fn max_chunk_size(&self) -> u64 {
        self.max_chunk_size
    }
}

/// Validate a ref name against git ref-format rules, additionally requiring
/// the `refs/` namespace. This is the only door through which a ref name —
/// always an untrusted string — enters the store; no filesystem path is
/// ever derived from an unvalidated name.
fn validated_ref_name(name: &str) -> Result<gix::refs::FullName, StoreError> {
    if !name.starts_with("refs/") {
        return Err(StoreError::InvalidRefName {
            name: name.to_owned(),
            reason: "acetone refs must be full names under refs/".into(),
        });
    }
    gix::refs::FullName::try_from(name).map_err(|e| StoreError::InvalidRefName {
        name: name.to_owned(),
        reason: e.to_string(),
    })
}

/// Map a gix ref-edit failure, distinguishing a lost compare-and-swap from
/// backend trouble.
fn map_ref_edit_error(name: &str, error: gix::reference::edit::Error) -> StoreError {
    use gix::refs::file::transaction::prepare::Error as Prepare;
    match &error {
        gix::reference::edit::Error::FileTransactionPrepare(
            Prepare::MustNotExist { .. }
            | Prepare::MustExist { .. }
            | Prepare::ReferenceOutOfDate { .. }
            | Prepare::DeleteReferenceMustExist { .. },
        ) => StoreError::CasFailed {
            name: name.to_owned(),
        },
        _ => StoreError::backend("updating ref", error),
    }
}

impl RefStore for GitStore {
    fn read_ref(&self, name: &str) -> Result<Option<Hash>, StoreError> {
        let full_name = validated_ref_name(name)?;
        let reference = self
            .repo
            .try_find_reference(full_name.as_bstr())
            .map_err(|e| StoreError::backend("reading ref", e))?;
        match reference {
            None => Ok(None),
            Some(reference) => match reference.target() {
                gix::refs::TargetRef::Object(oid) => Ok(Some(Hash::from_oid(oid.to_owned()))),
                gix::refs::TargetRef::Symbolic(_) => Err(StoreError::SymbolicRef {
                    name: name.to_owned(),
                }),
            },
        }
    }

    /// Atomic compare-and-swap ref update (see [`RefStore::write_ref`] for
    /// the contract).
    ///
    /// Writers are serialised through `<common_dir>/acetone-refs.lock`,
    /// released when this call returns. If a previous process died while
    /// holding it, this call backs off for ~5 seconds and then fails with
    /// [`StoreError::Backend`]; once no acetone process is running against
    /// the repository, deleting that file recovers (see the
    /// [`GitStore`] docs on locking and crash recovery).
    fn write_ref(&self, name: &str, expected: Option<&Hash>, new: &Hash) -> Result<(), StoreError> {
        use gix::refs::Target;
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};

        let full_name = validated_ref_name(name)?;

        // Serialise all acetone ref writers on this repository. gix 0.85
        // evaluates transaction preconditions (MustNotExist and friends)
        // against a read taken *before* it acquires the per-ref lock, so
        // racing writers could all observe the stale value and all pass the
        // precondition — losing the compare-and-swap guarantee. Holding one
        // store-level lock around the whole edit makes the read-check-write
        // sequence atomic for every writer that goes through this crate.
        // A non-acetone writer (e.g. `git update-ref`) racing inside gix's
        // own read-to-lock window remains theoretically possible until the
        // check moves under the lock upstream. The lock lives in the common
        // dir (shared by all worktrees, like refs themselves) and is
        // released on drop; a stale lock from a killed process makes
        // writers back off and fail with a Backend error rather than hang.
        let _writer_guard = gix::lock::Marker::acquire_to_hold_resource(
            self.repo.common_dir().join("acetone-refs"),
            gix::lock::acquire::Fail::AfterDurationWithBackoff(std::time::Duration::from_secs(5)),
            None,
        )
        .map_err(|e| StoreError::backend("locking refs for compare-and-swap", e))?;
        // gix's `MustNotExist` treats a *value-equal* edit as a no-op success
        // rather than a failure, which breaks the create-CAS contract: creating a
        // ref that already exists must fail even when it already holds `new`
        // (callers rely on the error to detect a lost create race). Under the
        // writer guard this read is atomic with the write, so enforce it here.
        if expected.is_none() && self.read_ref(name)?.is_some() {
            return Err(StoreError::CasFailed {
                name: name.to_string(),
            });
        }
        let precondition = match expected {
            None => PreviousValue::MustNotExist,
            Some(hash) => PreviousValue::MustExistAndMatch(Target::Object(hash.oid())),
        };
        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "acetone: write_ref".into(),
                },
                expected: precondition,
                new: Target::Object(new.oid()),
            },
            name: full_name,
            deref: false,
        };
        self.repo
            .edit_reference(edit)
            .map_err(|e| map_ref_edit_error(name, e))?;
        Ok(())
    }

    fn delete_ref(&self, name: &str) -> Result<(), StoreError> {
        use gix::refs::Target;
        use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};

        let full_name = validated_ref_name(name)?;
        // Nothing to delete: an absent ref is a no-op success (idempotent).
        let Some(current) = self.read_ref(name)? else {
            return Ok(());
        };
        // Serialise with other ref writers (see `write_ref` for why).
        let _writer_guard = gix::lock::Marker::acquire_to_hold_resource(
            self.repo.common_dir().join("acetone-refs"),
            gix::lock::acquire::Fail::AfterDurationWithBackoff(std::time::Duration::from_secs(5)),
            None,
        )
        .map_err(|e| StoreError::backend("locking refs for delete", e))?;
        let edit = RefEdit {
            change: Change::Delete {
                // Delete only if it still holds the value we just read, so a
                // concurrent update is not silently discarded.
                expected: PreviousValue::MustExistAndMatch(Target::Object(current.oid())),
                log: RefLog::AndReference,
            },
            name: full_name,
            deref: false,
        };
        self.repo
            .edit_reference(edit)
            .map_err(|e| map_ref_edit_error(name, e))?;
        Ok(())
    }

    fn read_head(&self) -> Result<Option<String>, StoreError> {
        let head = self
            .repo
            .head()
            .map_err(|e| StoreError::backend("reading HEAD", e))?;
        Ok(match head.kind {
            gix::head::Kind::Symbolic(reference) => Some(reference.name.as_bstr().to_string()),
            gix::head::Kind::Unborn(full_name) => Some(full_name.as_bstr().to_string()),
            gix::head::Kind::Detached { .. } => None,
        })
    }

    fn set_head(&self, ref_name: &str) -> Result<(), StoreError> {
        use gix::refs::Target;
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};

        let target = validated_ref_name(ref_name)?;
        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "acetone: set_head".into(),
                },
                expected: PreviousValue::Any,
                new: Target::Symbolic(target),
            },
            name: gix::refs::FullName::try_from("HEAD").expect("HEAD is a valid ref name"),
            deref: false,
        };
        self.repo
            .edit_reference(edit)
            .map_err(|e| StoreError::backend("setting HEAD", e))?;
        Ok(())
    }

    fn list_refs(&self, prefix: &str) -> Result<Vec<(String, Hash)>, StoreError> {
        // The same validation door as every other ref name; a prefix is a
        // ref-namespace name.
        if !prefix.starts_with("refs/") {
            return Err(StoreError::InvalidRefName {
                name: prefix.to_owned(),
                reason: "ref prefixes must be full names under refs/".into(),
            });
        }
        let platform = self
            .repo
            .references()
            .map_err(|e| StoreError::backend("listing refs", e))?;
        let iter = platform
            .prefixed(prefix)
            .map_err(|e| StoreError::backend("listing refs", e))?;
        let mut out = Vec::new();
        for reference in iter {
            // The iteration error is an unsized boxed error; rewrap it so
            // it fits the sized bound of `backend`.
            let reference = reference
                .map_err(|e| StoreError::backend("listing refs", std::io::Error::other(e)))?;
            if let gix::refs::TargetRef::Object(oid) = reference.target() {
                out.push((
                    reference.name().as_bstr().to_string(),
                    Hash::from_oid(oid.to_owned()),
                ));
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Reserved directory holding acetone's machine-readable commit- and
/// workspace-tree entries (`manifest`, `chunks/`), keeping the tree root free
/// for co-tenant files such as a user's own `README.md` (ADR-0023). Chosen so
/// it sorts before `README.md` at the root (`'.'` 0x2E < `'R'` 0x52).
const ACETONE_DIR: &str = ".acetone";
/// Name of the manifest blob within the `.acetone/` directory (spec §3.5).
const MANIFEST_ENTRY: &str = "manifest";
/// Name of the human-readable summary blob at the commit-tree **root**;
/// `README.md` so hosting UIs render it (kept at the root deliberately —
/// ADR-0023).
const SUMMARY_ENTRY: &str = "README.md";
/// Name of the chunk-anchor tree within `.acetone/`: `.acetone/chunks/<hh>/<hex>`
/// entries reference every anchored chunk so git's reachability walk keeps
/// them alive and transfers them.
const ANCHORS_ENTRY: &str = "chunks";

/// Validate one trailer pair against the git trailer format.
/// Validate a commit-message trailer `(token, value)` against git's trailer
/// rules (token `[A-Za-z0-9][A-Za-z0-9-]*`; value a single non-empty line with
/// no control characters and no leading/trailing whitespace). Exposed so a
/// caller can reject bad trailer values *before* staging any work, rather than
/// discovering them when `create_commit` assembles the message.
pub fn validate_trailer(token: &str, value: &str) -> Result<(), StoreError> {
    let invalid = |reason: &str| StoreError::InvalidTrailer {
        token: token.to_owned(),
        reason: reason.to_owned(),
    };
    let mut chars = token.chars();
    match chars.next() {
        None => return Err(invalid("token must not be empty")),
        Some(c) if !c.is_ascii_alphanumeric() => {
            return Err(invalid("token must start with an ASCII letter or digit"));
        }
        Some(_) => {}
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(invalid(
            "token may contain only ASCII letters, digits and '-'",
        ));
    }
    if value.is_empty() {
        return Err(invalid("value must not be empty"));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(invalid(
            "value must be a single line without control characters",
        ));
    }
    if value != value.trim() {
        return Err(invalid(
            "value must not have leading or trailing whitespace (git trims it on read)",
        ));
    }
    Ok(())
}

/// Assemble the stored commit message: the message proper plus, if any, a
/// final paragraph of trailers in git trailer format.
fn assemble_message(message: &str, trailers: &[(String, String)]) -> Result<String, StoreError> {
    for (token, value) in trailers {
        validate_trailer(token, value)?;
    }
    let body = message.trim_end();
    if body.is_empty() {
        return Err(StoreError::Corrupt {
            context: "new commit",
            reason: "commit message must not be empty".into(),
        });
    }
    if trailers.is_empty() {
        return Ok(format!("{body}\n"));
    }
    let mut out = String::with_capacity(body.len() + 64 * trailers.len());
    out.push_str(body);
    out.push_str("\n\n");
    for (token, value) in trailers {
        out.push_str(token);
        out.push_str(": ");
        out.push_str(value);
        out.push('\n');
    }
    Ok(out)
}

/// Validate a name/email pair for a commit identity (git-native rules:
/// non-empty name, no angle brackets or control characters).
fn validate_identity_fields(name: &str, email: &str) -> Result<(), StoreError> {
    let invalid = |reason: &str| StoreError::InvalidSignature {
        reason: reason.to_owned(),
    };
    if name.trim().is_empty() {
        return Err(invalid("name must not be empty"));
    }
    for (field, text) in [("name", name), ("email", email)] {
        if text.chars().any(|c| c == '<' || c == '>' || c.is_control()) {
            return Err(invalid(&format!(
                "{field} must not contain angle brackets or control characters"
            )));
        }
    }
    Ok(())
}

/// Validate and convert an author/committer identity, stamping the time as
/// *now* (for freshly created commits).
fn git_signature(sig: &crate::store::Signature) -> Result<gix::actor::Signature, StoreError> {
    validate_identity_fields(&sig.name, &sig.email)?;
    Ok(gix::actor::Signature {
        name: sig.name.as_str().into(),
        email: sig.email.as_str().into(),
        time: gix::date::Time::now_utc(),
    })
}

/// Convert a stored [`Identity`] to a git signature, **preserving** its
/// timestamp (for history rewrites).
fn git_identity(id: &crate::store::Identity) -> Result<gix::actor::Signature, StoreError> {
    validate_identity_fields(&id.name, &id.email)?;
    Ok(gix::actor::Signature {
        name: id.name.as_str().into(),
        email: id.email.as_str().into(),
        time: gix::date::Time {
            seconds: id.time_seconds,
            offset: id.time_offset_seconds,
        },
    })
}

impl CommitStore for GitStore {
    fn create_commit(&self, commit: &NewCommit<'_>) -> Result<Hash, StoreError> {
        let message = assemble_message(commit.message, commit.trailers)?;
        let signature = git_signature(&commit.author)?;
        let tree_id =
            self.write_commit_tree_oid(commit.manifest, commit.summary, commit.anchors)?;

        let commit_object = gix::objs::Commit {
            tree: tree_id,
            parents: commit.parents.iter().map(Hash::oid).collect(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let commit_id = self
            .repo
            .write_object(&commit_object)
            .map_err(|e| StoreError::backend("writing commit", e))?
            .detach();
        Ok(Hash::from_oid(commit_id))
    }

    fn read_commit(&self, id: &Hash) -> Result<Option<Commit>, StoreError> {
        let Some(header) = self.find_header(id)? else {
            return Ok(None);
        };
        let data = self.read_object_checked(id, &header, gix::object::Kind::Commit, "commit")?;
        let commit = gix::objs::CommitRef::from_bytes(&data, self.repo.object_hash())
            .map_err(|e| StoreError::corrupt("commit object", e.to_string()))?;

        let parents: Vec<Hash> = commit.parents().map(Hash::from_oid).collect();
        let author = identity_from_ref(
            commit
                .author()
                .map_err(|e| StoreError::corrupt("commit author", e.to_string()))?,
            "commit author",
        )?;
        let committer = identity_from_ref(
            commit
                .committer()
                .map_err(|e| StoreError::corrupt("commit committer", e.to_string()))?,
            "commit committer",
        )?;
        let message = String::from_utf8_lossy(commit.message).into_owned();
        let trailers: Vec<(String, String)> = commit
            .message()
            .body()
            .map(|body| {
                body.trailers()
                    .map(|trailer| {
                        (
                            String::from_utf8_lossy(trailer.token).into_owned(),
                            String::from_utf8_lossy(&trailer.value).into_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let tree_hash = Hash::from_oid(commit.tree());
        let tree_header = self
            .find_header(&tree_hash)?
            .ok_or_else(|| StoreError::corrupt("commit tree", "tree object is missing"))?;
        let tree_data =
            self.read_object_checked(&tree_hash, &tree_header, gix::object::Kind::Tree, "tree")?;
        let tree = gix::objs::TreeRef::from_bytes(&tree_data, self.repo.object_hash())
            .map_err(|e| StoreError::corrupt("commit tree", e.to_string()))?;

        let manifest_hash = self.root_manifest_hash(&tree, "commit tree")?;
        let manifest = self
            .get(&manifest_hash)?
            .ok_or_else(|| StoreError::corrupt("commit manifest", "manifest blob is missing"))?;

        Ok(Some(Commit {
            id: *id,
            manifest,
            message,
            trailers,
            parents,
            author,
            committer,
        }))
    }
}

/// Convert a git signature reference read from a commit into a stored
/// [`Identity`], parsing its timestamp. `what` names the field in errors.
fn identity_from_ref(
    sig: gix::actor::SignatureRef<'_>,
    what: &'static str,
) -> Result<crate::store::Identity, StoreError> {
    let time = sig
        .time()
        .map_err(|e| StoreError::corrupt(what, format!("unparsable timestamp: {e}")))?;
    Ok(crate::store::Identity {
        name: String::from_utf8_lossy(sig.name).into_owned(),
        email: String::from_utf8_lossy(sig.email).into_owned(),
        time_seconds: time.seconds,
        time_offset_seconds: time.offset,
    })
}

#[cfg(test)]
mod tests {
    use super::{assemble_message, validate_trailer, validated_ref_name};

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(t, v)| (t.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn message_without_trailers_is_normalised_to_one_newline() {
        assert_eq!(assemble_message("subject", &[]).unwrap(), "subject\n");
        assert_eq!(assemble_message("subject\n\n\n", &[]).unwrap(), "subject\n");
    }

    #[test]
    fn trailers_form_the_final_paragraph() {
        let message = assemble_message(
            "subject\n\nbody paragraph.",
            &pairs(&[("Acetone-Source", "s3://x"), ("Acetone-Extractor", "t 1.0")]),
        )
        .unwrap();
        assert_eq!(
            message,
            "subject\n\nbody paragraph.\n\nAcetone-Source: s3://x\nAcetone-Extractor: t 1.0\n"
        );
    }

    #[test]
    fn empty_message_is_rejected_even_with_trailers() {
        assert!(assemble_message("", &pairs(&[("T", "v")])).is_err());
        assert!(assemble_message("  \n", &[]).is_err());
    }

    #[test]
    fn trailer_validation_rules() {
        assert!(validate_trailer("Acetone-Source", "value").is_ok());
        assert!(validate_trailer("X0", "v v v").is_ok());
        for (token, value) in [
            ("", "v"),
            ("-x", "v"),
            ("a b", "v"),
            ("a:b", "v"),
            ("ünïcode", "v"),
            ("T", ""),
            ("T", "line\nbreak"),
            ("T", "\tleading"),
            ("T", "trailing "),
        ] {
            assert!(
                validate_trailer(token, value).is_err(),
                "should reject {token:?}: {value:?}"
            );
        }
    }

    #[test]
    fn ref_name_validation_requires_refs_namespace() {
        assert!(validated_ref_name("refs/acetone/workspaces/default").is_ok());
        assert!(validated_ref_name("HEAD").is_err());
        assert!(validated_ref_name("refs/acetone/../escape").is_err());
    }
}
