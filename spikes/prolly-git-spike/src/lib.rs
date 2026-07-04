//! THROWAWAY SPIKE (bead acetone-28x.2): a prolly map over the git object
//! database, de-risking Decision 1 Option A (git ODB as chunk store).
//!
//! - Chunks are git blobs; a chunk's address IS its git OID (single
//!   addressing scheme).
//! - Content-defined chunking (gear hash, ~4 KiB target) makes tree shape a
//!   pure function of map contents: identical contents yield identical root
//!   OIDs regardless of operation order (history independence).
//! - A version is committed as a real git commit whose tree carries the
//!   root manifest plus a sharded `chunks/` tree referencing every chunk,
//!   so the data survives `git gc`, `git clone` and push/pull.
//!
//! Not production code. See `docs/acetone-03-roadmap.md` Phase 0.

pub mod chunker;
pub mod pack;
mod tree;

use std::path::Path;

use gix::ObjectId;

use chunker::ChunkParams;
pub use tree::Scan;

/// Chunk writes observed while recording is enabled (see
/// [`Store::start_recording`]) — the raw material for pack-on-write
/// (bead acetone-63m.10): which chunks are new, and which old chunk each
/// one replaces (its chosen delta base).
#[derive(Debug, Default, Clone)]
pub struct WriteRecord {
    /// Every chunk OID handed to the ODB while recording, in write order
    /// (includes manifest blobs written by `commit_root`).
    pub written: Vec<ObjectId>,
    /// Chosen delta base per written chunk: the predecessor chunk it
    /// replaces at the same tree level (new OID -> old OID).
    pub bases: std::collections::HashMap<ObjectId, ObjectId>,
}

/// Root of one map version: everything needed to read it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// Git OID of the root chunk.
    pub oid: ObjectId,
    /// Number of levels (1 = the root is a leaf).
    pub height: u32,
    /// Chunking parameters the tree was built with.
    pub params: ChunkParams,
}

/// One mutation in a batch. Within a batch, duplicate keys resolve to the
/// last op; deleting an absent key is a no-op.
#[derive(Debug, Clone)]
pub enum BatchOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

impl BatchOp {
    pub fn key(&self) -> &[u8] {
        match self {
            BatchOp::Put(k, _) => k,
            BatchOp::Delete(k) => k,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpikeError {
    /// Any error surfaced by gix (stringified — spike-grade granularity).
    #[error("git error: {0}")]
    Git(String),
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("reference not found: {0}")]
    RefNotFound(String),
    #[error("bad manifest: {0}")]
    BadManifest(String),
}

fn giterr(e: impl std::fmt::Display) -> SpikeError {
    SpikeError::Git(e.to_string())
}

/// A prolly map store over the git object database of one repository.
pub struct Store {
    repo: gix::Repository,
    chunks_written: std::cell::Cell<u64>,
    recorder: std::cell::RefCell<Option<WriteRecord>>,
}

const MANIFEST_FORMAT: &str = "prolly-git-spike-v0";

impl Store {
    /// Initialise a new bare git repository at `path` and open it as a store.
    pub fn create(path: &Path) -> Result<Self, SpikeError> {
        let repo = gix::init_bare(path).map_err(giterr)?;
        Ok(Store {
            repo,
            chunks_written: std::cell::Cell::new(0),
            recorder: std::cell::RefCell::new(None),
        })
    }

    /// Open an existing git repository (bare or not) as a store.
    pub fn open(path: &Path) -> Result<Self, SpikeError> {
        let repo = gix::open(path).map_err(giterr)?;
        Ok(Store {
            repo,
            chunks_written: std::cell::Cell::new(0),
            recorder: std::cell::RefCell::new(None),
        })
    }

    /// Start recording chunk writes and their chosen delta bases (for
    /// pack-on-write, bead acetone-63m.10). Any previous recording is
    /// discarded.
    pub fn start_recording(&self) {
        *self.recorder.borrow_mut() = Some(WriteRecord::default());
    }

    /// Stop recording and return what was captured since
    /// [`Store::start_recording`].
    pub fn take_recording(&self) -> Option<WriteRecord> {
        self.recorder.borrow_mut().take()
    }

    pub(crate) fn recording(&self) -> bool {
        self.recorder.borrow().is_some()
    }

    pub(crate) fn record_base(&self, new: ObjectId, base: ObjectId) {
        if new == base {
            return; // an unchanged chunk needs no delta
        }
        if let Some(rec) = self.recorder.borrow_mut().as_mut() {
            rec.bases.entry(new).or_insert(base);
        }
    }

    /// Number of chunk blobs serialised and handed to the ODB so far.
    /// Reused chunks bypass this — the counter is how tests observe write
    /// amplification (and how the benchmark bead can measure it).
    pub fn chunks_written(&self) -> u64 {
        self.chunks_written.get()
    }

    /// Write one chunk as a git blob; the returned OID is its address.
    pub(crate) fn write_chunk(&self, data: &[u8]) -> Result<ObjectId, SpikeError> {
        self.chunks_written.set(self.chunks_written.get() + 1);
        let oid = self.repo.write_blob(data).map_err(giterr)?.detach();
        if let Some(rec) = self.recorder.borrow_mut().as_mut() {
            rec.written.push(oid);
        }
        Ok(oid)
    }

    /// Read one chunk back by OID.
    pub(crate) fn read_chunk(&self, oid: &ObjectId) -> Result<Vec<u8>, SpikeError> {
        let obj = self.repo.find_object(*oid).map_err(giterr)?;
        if obj.kind != gix::object::Kind::Blob {
            return Err(SpikeError::Corrupt(format!(
                "object {oid} is a {}, expected blob",
                obj.kind
            )));
        }
        Ok(obj.detach().data)
    }

    /// Commit `root` onto `ref_name` (e.g. `refs/spike/run-1`) as a real git
    /// commit. The commit's tree holds the manifest and a sharded `chunks/`
    /// tree naming every chunk of this version, so all data is reachable
    /// from the ref (survives `git gc`/`clone`/push). If the ref exists its
    /// tip becomes the parent. Returns the commit OID.
    pub fn commit_root(
        &self,
        root: &Root,
        ref_name: &str,
        message: &str,
    ) -> Result<ObjectId, SpikeError> {
        use gix::objs::tree::{Entry, EntryKind};

        let manifest_oid = self.write_chunk(format_manifest(root).as_bytes())?;
        let chunks_tree = self.write_chunks_tree(root)?;

        let root_tree = gix::objs::Tree {
            entries: vec![
                Entry {
                    mode: EntryKind::Tree.into(),
                    filename: "chunks".into(),
                    oid: chunks_tree,
                },
                Entry {
                    mode: EntryKind::Blob.into(),
                    filename: "manifest".into(),
                    oid: manifest_oid,
                },
            ],
        };
        let tree_id = self.repo.write_object(&root_tree).map_err(giterr)?.detach();

        let parents: Vec<ObjectId> = match self.repo.try_find_reference(ref_name).map_err(giterr)? {
            Some(mut r) => vec![r.peel_to_id().map_err(giterr)?.detach()],
            None => Vec::new(),
        };

        let signature = gix::actor::Signature {
            name: "prolly-git-spike".into(),
            email: "spike@acetone.invalid".into(),
            time: gix::date::Time::now_utc(),
        };
        let commit_id = self
            .repo
            .commit_as(
                signature.to_ref(&mut Default::default()),
                signature.to_ref(&mut Default::default()),
                ref_name,
                message,
                tree_id,
                parents,
            )
            .map_err(giterr)?;
        Ok(commit_id.detach())
    }

    /// Read the manifest back from the tip commit of `ref_name`.
    pub fn read_manifest(&self, ref_name: &str) -> Result<Root, SpikeError> {
        let mut reference = self
            .repo
            .try_find_reference(ref_name)
            .map_err(giterr)?
            .ok_or_else(|| SpikeError::RefNotFound(ref_name.to_string()))?;
        let commit_id = reference.peel_to_id().map_err(giterr)?;
        let commit = commit_id
            .object()
            .map_err(giterr)?
            .try_into_commit()
            .map_err(giterr)?;
        let tree = commit.tree().map_err(giterr)?;
        let entry = tree
            .find_entry("manifest")
            .ok_or_else(|| SpikeError::BadManifest("no manifest entry in commit tree".into()))?;
        let data = entry.object().map_err(giterr)?.detach().data;
        parse_manifest(&data)
    }

    /// Enumerate every chunk OID of a version (root, internals, leaves) by
    /// walking internal nodes only, and build a two-level tree
    /// `chunks/<hh>/<rest-of-hex>` referencing them all.
    fn write_chunks_tree(&self, root: &Root) -> Result<ObjectId, SpikeError> {
        use gix::objs::tree::{Entry, EntryKind};

        let mut all: Vec<ObjectId> = vec![root.oid];
        let mut frontier: Vec<(ObjectId, u32)> = vec![(root.oid, root.height - 1)];
        while let Some((oid, level)) = frontier.pop() {
            if level == 0 {
                continue;
            }
            match self.read_node(&oid, level as u8)? {
                tree::Node::Inner(refs) => {
                    for r in refs {
                        all.push(r.oid);
                        if level > 1 {
                            frontier.push((r.oid, level - 1));
                        }
                    }
                }
                tree::Node::Leaf(_) => unreachable!("level checked by read_node"),
            }
        }
        all.sort();
        all.dedup();

        // Shard by the first hex byte so unchanged shards share tree
        // objects between successive commits.
        let mut shards: Vec<(String, Vec<Entry>)> = Vec::new();
        for oid in all {
            let hex = oid.to_string();
            let (prefix, rest) = hex.split_at(2);
            if shards.last().map(|(p, _)| p.as_str()) != Some(prefix) {
                shards.push((prefix.to_string(), Vec::new()));
            }
            shards.last_mut().expect("just pushed").1.push(Entry {
                mode: EntryKind::Blob.into(),
                filename: rest.into(),
                oid,
            });
        }
        let mut top_entries = Vec::with_capacity(shards.len());
        for (prefix, entries) in shards {
            let shard_tree = gix::objs::Tree { entries };
            let shard_id = self
                .repo
                .write_object(&shard_tree)
                .map_err(giterr)?
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
            .map_err(giterr)?
            .detach())
    }
}

fn format_manifest(root: &Root) -> String {
    format!(
        "format: {MANIFEST_FORMAT}\nroot: {}\nheight: {}\nchunk_min_bytes: {}\nchunk_mask_bits: {}\nchunk_max_bytes: {}\n",
        root.oid, root.height, root.params.min_bytes, root.params.mask_bits, root.params.max_bytes
    )
}

fn parse_manifest(data: &[u8]) -> Result<Root, SpikeError> {
    let bad = |m: &str| SpikeError::BadManifest(m.to_string());
    let text = std::str::from_utf8(data).map_err(|_| bad("manifest is not UTF-8"))?;
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(": ") {
            fields.insert(k.to_string(), v.to_string());
        }
    }
    if fields.get("format").map(String::as_str) != Some(MANIFEST_FORMAT) {
        return Err(bad("unknown manifest format"));
    }
    let get = |k: &str| {
        fields
            .get(k)
            .ok_or_else(|| bad(&format!("missing field {k}")))
    };
    let num = |k: &str| -> Result<u64, SpikeError> {
        get(k)?
            .parse()
            .map_err(|_| bad(&format!("bad number in {k}")))
    };
    Ok(Root {
        oid: ObjectId::from_hex(get("root")?.as_bytes()).map_err(|_| bad("bad root oid"))?,
        height: num("height")? as u32,
        params: ChunkParams {
            min_bytes: num("chunk_min_bytes")? as usize,
            mask_bits: num("chunk_mask_bits")? as u32,
            max_bytes: num("chunk_max_bytes")? as usize,
        },
    })
}
