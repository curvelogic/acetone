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
use crate::store::{ChunkStore, Commit, CommitStore, NewCommit, RefStore};

/// Default hard cap on the size of any single object this store will
/// materialise: 64 MiB.
///
/// Prolly chunks target ~4 KiB (spec §3.2) and manifests are small, so any
/// legitimate acetone object is orders of magnitude below this; a hostile
/// multi-GiB blob is rejected from its header before allocation. Override
/// via [`GitStoreOptions::max_chunk_size`] if a deployment needs more.
pub const DEFAULT_MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Construction parameters for a [`GitStore`].
#[derive(Debug, Clone)]
pub struct GitStoreOptions {
    /// Hard cap in bytes on any single object read or written; see
    /// [`DEFAULT_MAX_CHUNK_SIZE`]. Enforced by [`ChunkStore::put`] and, on
    /// every read path, checked against the object header *before* the
    /// object is materialised.
    pub max_chunk_size: u64,
}

impl Default for GitStoreOptions {
    fn default() -> Self {
        GitStoreOptions {
            max_chunk_size: DEFAULT_MAX_CHUNK_SIZE,
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
    pub fn create_with(path: &Path, options: GitStoreOptions) -> Result<Self, StoreError> {
        gix::init_bare(path).map_err(|e| StoreError::backend("initialising repository", e))?;
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

    /// Write one blob, enforcing the size cap.
    fn write_blob_capped(&self, data: &[u8], _what: &'static str) -> Result<Hash, StoreError> {
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
            .map_err(|e| StoreError::backend("writing blob", e))?;
        Ok(Hash::from_oid(id.detach()))
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
}

impl ChunkStore for GitStore {
    fn put(&self, data: &[u8]) -> Result<Hash, StoreError> {
        self.write_blob_capped(data, "chunk")
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
}

/// Name of the manifest blob within a commit tree (spec §3.5).
const MANIFEST_ENTRY: &str = "manifest";
/// Name of the human-readable summary blob within a commit tree; `README.md`
/// so hosting UIs render it.
const SUMMARY_ENTRY: &str = "README.md";

/// Validate one trailer pair against the git trailer format.
fn validate_trailer(token: &str, value: &str) -> Result<(), StoreError> {
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

/// Validate and convert an author/committer identity.
fn git_signature(sig: &crate::store::Signature) -> Result<gix::actor::Signature, StoreError> {
    let invalid = |reason: &str| StoreError::InvalidSignature {
        reason: reason.to_owned(),
    };
    if sig.name.trim().is_empty() {
        return Err(invalid("name must not be empty"));
    }
    for (field, text) in [("name", &sig.name), ("email", &sig.email)] {
        if text.chars().any(|c| c == '<' || c == '>' || c.is_control()) {
            return Err(invalid(&format!(
                "{field} must not contain angle brackets or control characters"
            )));
        }
    }
    Ok(gix::actor::Signature {
        name: sig.name.as_str().into(),
        email: sig.email.as_str().into(),
        time: gix::date::Time::now_utc(),
    })
}

impl CommitStore for GitStore {
    fn create_commit(&self, commit: &NewCommit<'_>) -> Result<Hash, StoreError> {
        use gix::objs::tree::{Entry, EntryKind};

        let message = assemble_message(commit.message, commit.trailers)?;
        let signature = git_signature(&commit.author)?;
        let manifest_id = self.write_blob_capped(commit.manifest, "manifest")?;
        let summary_id = self.write_blob_capped(commit.summary.as_bytes(), "summary")?;

        // Entries must be in git tree order; "README.md" < "manifest"
        // byte-wise, so this literal order is already sorted.
        let tree = gix::objs::Tree {
            entries: vec![
                Entry {
                    mode: EntryKind::Blob.into(),
                    filename: SUMMARY_ENTRY.into(),
                    oid: summary_id.oid(),
                },
                Entry {
                    mode: EntryKind::Blob.into(),
                    filename: MANIFEST_ENTRY.into(),
                    oid: manifest_id.oid(),
                },
            ],
        };
        let tree_id = self
            .repo
            .write_object(&tree)
            .map_err(|e| StoreError::backend("writing commit tree", e))?
            .detach();

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

        let manifest_entry = tree
            .entries
            .iter()
            .find(|entry| entry.filename == MANIFEST_ENTRY)
            .ok_or_else(|| {
                StoreError::corrupt("commit tree", "no `manifest` entry in commit tree")
            })?;
        if !manifest_entry.mode.is_blob() {
            return Err(StoreError::corrupt(
                "commit tree",
                "`manifest` entry is not a blob",
            ));
        }
        let manifest_hash = Hash::from_oid(manifest_entry.oid.to_owned());
        let manifest = self
            .get(&manifest_hash)?
            .ok_or_else(|| StoreError::corrupt("commit manifest", "manifest blob is missing"))?;

        Ok(Some(Commit {
            id: *id,
            manifest,
            message,
            trailers,
            parents,
        }))
    }
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
