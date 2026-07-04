//! Integration tests for `fsck` (spec §7, ADR-0011): over real bare git
//! repositories, a healthy graph verifies clean and every class of damage
//! — missing chunk, physically and logically corrupt chunk, garbage
//! manifest, non-commit ref, asymmetric edge maps — produces the right
//! finding, distinctly and named, never a panic.

use std::fs;
use std::path::{Path, PathBuf};

use acetone_graph::fsck::{self, FindingKind, Severity};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::MapRoot;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_prolly::{BatchOp, Hash, apply_batch, reachable_chunks};
use acetone_store::{ChunkStore, RefStore};

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

fn node(label: &str, key: &str) -> NodeKey {
    NodeKey::new(label, vec![Value::String(key.to_owned())]).expect("valid")
}

/// Insert `n` nodes and commit, so the `nodes` map is a genuine multi-level
/// tree with interior nodes and leaves to damage.
fn commit_many_nodes(repo: &Repository, n: usize) {
    let mut tx = repo.begin_write().expect("begin");
    for i in 0..n {
        tx.put_node(
            &node("Host", &format!("h{i:08}")),
            &NodeRecord::new([], Default::default()),
        )
        .expect("put node");
    }
    tx.commit("populate", &[], None).expect("commit");
}

/// Path of the loose object for `hash` in a bare repository.
fn loose_object_path(repo: &Repository, hash: &Hash) -> PathBuf {
    let hex = hash.to_hex();
    repo.store()
        .common_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..])
}

/// A reachable chunk of the `nodes` map that is not the map root, so
/// damaging it exercises a descent rather than the root itself.
fn a_non_root_nodes_chunk(repo: &Repository) -> Hash {
    let manifest = repo.workspace_manifest().expect("manifest");
    let root = manifest.nodes.to_root(manifest.chunk_params).expect("root");
    let chunks = reachable_chunks(repo.store(), &root).expect("reachable");
    *chunks
        .iter()
        .find(|h| **h != root.hash())
        .expect("nodes map must be multi-level for this test")
}

#[test]
fn empty_repository_is_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.is_clean(),
        "fresh repo has findings: {:?}",
        report.findings
    );
}

#[test]
fn populated_and_committed_repository_is_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 2500);
    // Add uncommitted work too, so the workspace differs from HEAD and both
    // versions are exercised.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node("Host", "extra"),
        &NodeRecord::new([], Default::default()),
    )
    .expect("put");
    tx.save().expect("save");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.is_clean(),
        "healthy repo reported findings: {:?}",
        report.findings
    );
}

#[test]
fn deleted_chunk_is_reported_missing_and_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 2500);
    let victim = a_non_root_nodes_chunk(&repo);
    fs::remove_file(loose_object_path(&repo, &victim)).expect("remove loose object");

    let report = fsck::check(&repo).expect("fsck");
    assert!(report.has_errors());
    let missing: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::MissingChunk)
        .collect();
    assert!(
        missing.iter().any(|f| f.chunk == Some(victim)),
        "expected a MISSING finding naming {victim}, got {:?}",
        report.findings
    );
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.kind == FindingKind::MissingChunk),
        "deletion must not manufacture corruption findings: {:?}",
        report.findings
    );
}

#[test]
fn physically_corrupted_loose_object_is_reported_corrupt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 2500);
    let victim = a_non_root_nodes_chunk(&repo);
    // Overwrite the loose object's on-disk bytes: git's zlib/hash checks
    // fail on read, so the store cannot return the chunk — a corrupt signal.
    let path = loose_object_path(&repo, &victim);
    fs::remove_file(&path).expect("remove");
    fs::write(&path, b"\xff\xff\xff not a valid loose object \x00\x01\x02").expect("write garbage");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::CorruptChunk && f.chunk == Some(victim)),
        "expected a CORRUPT finding naming {victim}, got {:?}",
        report.findings
    );
}

#[test]
fn logically_corrupt_chunk_spliced_into_manifest_is_corrupt() {
    // A valid git blob that is not a valid prolly node, spliced into a
    // hand-built manifest behind a workspace ref: the blob exists, so this
    // is corruption (bad node), not absence.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let garbage = store
        .put(b"a perfectly good blob, not a prolly node")
        .expect("put");
    let mut manifest = repo.workspace_manifest().expect("manifest");
    manifest.nodes = MapRoot {
        hash: garbage,
        height: 1,
    };
    let blob = store.put(&manifest.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/logical", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::CorruptChunk && f.chunk == Some(garbage)),
        "expected a CORRUPT finding naming {garbage}, got {:?}",
        report.findings
    );
}

#[test]
fn garbage_manifest_behind_workspace_ref_is_a_manifest_finding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();
    let blob = store
        .put(b"this is not canonical manifest CBOR")
        .expect("put");
    store
        .write_ref("refs/acetone/workspaces/garbage", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Manifest
                && matches!(&f.origin, acetone_graph::Origin::Workspace { reference }
                    if reference == "refs/acetone/workspaces/garbage")
        }),
        "expected a Manifest finding for the garbage workspace, got {:?}",
        report.findings
    );
}

#[test]
fn non_commit_branch_tip_is_a_commit_finding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();
    // Point a branch at a blob, not a commit.
    let blob = store.put(b"definitely not a commit object").expect("put");
    store
        .write_ref("refs/heads/bogus", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::Commit),
        "expected a Commit finding for the non-commit branch tip, got {:?}",
        report.findings
    );
}

#[test]
fn asymmetric_edge_maps_are_an_advisory_not_an_error() {
    // Hand-build a manifest whose forward edge map has an edge the reverse
    // map does not, bypassing Transaction (which maintains symmetry).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let base = repo.workspace_manifest().expect("manifest");
    let empty_fwd = base.edges_fwd.to_root(base.chunk_params).expect("root");
    let key = EdgeKey::new(node("Host", "a"), "LINK", node("Host", "b"), Value::Null).expect("key");
    let fwd = apply_batch(
        store,
        &empty_fwd,
        vec![BatchOp::Put(
            key.encode_fwd().expect("encode"),
            EdgeRecord::default().encode().expect("encode record"),
        )],
    )
    .expect("apply_batch");

    let mut manifest = base.clone();
    manifest.edges_fwd = MapRoot::from_root(&fwd);
    let blob = store.put(&manifest.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/asym", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    let advisories: Vec<_> = report.advisories().collect();
    assert!(
        advisories
            .iter()
            .any(|f| f.kind == FindingKind::EdgeAsymmetry),
        "expected an EdgeAsymmetry advisory, got {:?}",
        report.findings
    );
    // The chunks are all structurally sound, so no error-severity finding
    // may come from the asymmetric workspace.
    assert!(
        report
            .findings
            .iter()
            .filter(
                |f| matches!(&f.origin, acetone_graph::Origin::Workspace { reference }
                if reference == "refs/acetone/workspaces/asym")
            )
            .all(|f| f.severity == Severity::Advisory),
        "asymmetry must be advisory, not error: {:?}",
        report.findings
    );
}

#[test]
fn undecodable_edge_entries_surface_as_advisory_not_silence() {
    // A structurally valid edge chunk (verify_reachable is happy) whose
    // *value* is not a valid edge record: Snapshot::edges() fails to decode.
    // fsck must surface this, not silently pass the repository as clean.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let base = repo.workspace_manifest().expect("manifest");
    let empty_fwd = base.edges_fwd.to_root(base.chunk_params).expect("root");
    let key = EdgeKey::new(node("Host", "a"), "LINK", node("Host", "b"), Value::Null).expect("key");
    let fwd = apply_batch(
        store,
        &empty_fwd,
        vec![BatchOp::Put(
            key.encode_fwd().expect("encode"),
            b"this is not a valid edge record".to_vec(),
        )],
    )
    .expect("apply_batch");
    let mut manifest = base.clone();
    manifest.edges_fwd = MapRoot::from_root(&fwd);
    let blob = store.put(&manifest.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/badedge", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        !report.is_clean(),
        "an undecodable edge entry must not read as clean"
    );
    assert!(
        report.advisories().any(|f| {
            f.kind == FindingKind::EdgeAsymmetry
                && matches!(&f.origin, acetone_graph::Origin::Workspace { reference }
                    if reference == "refs/acetone/workspaces/badedge")
        }),
        "expected an edge advisory for the bad-edge workspace, got {:?}",
        report.findings
    );
    assert!(
        !report.has_errors(),
        "the structurally sound chunk is not an error-severity finding: {:?}",
        report.findings
    );
}

#[test]
fn commit_history_versions_are_verified() {
    // Damage a chunk that only a *historical* commit references (not the
    // current workspace), and confirm fsck still catches it by walking
    // history.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 2500);
    let historical_chunk = a_non_root_nodes_chunk(&repo);

    // Advance the workspace far away by replacing all nodes, and commit, so
    // the old chunk is no longer in the current workspace — only in history.
    let mut tx = repo.begin_write().expect("begin");
    for i in 0..2500 {
        tx.delete_node(&node("Host", &format!("h{i:08}")))
            .expect("delete");
    }
    tx.put_node(
        &node("Other", "x"),
        &NodeRecord::new([], Default::default()),
    )
    .expect("put");
    tx.commit("replace", &[], None).expect("commit");

    // The historical chunk is no longer reachable from the workspace.
    let current = {
        let manifest = repo.workspace_manifest().expect("manifest");
        let root = manifest.nodes.to_root(manifest.chunk_params).expect("root");
        reachable_chunks(repo.store(), &root).expect("reachable")
    };
    assert!(
        !current.contains(&historical_chunk),
        "test precondition: chunk must be gone from the workspace"
    );

    fs::remove_file(loose_object_path(&repo, &historical_chunk)).expect("remove");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::MissingChunk
                && f.chunk == Some(historical_chunk)
                && matches!(f.origin, acetone_graph::Origin::Commit { .. })
        }),
        "history walk must catch the damaged historical chunk, got {:?}",
        report.findings
    );
}
