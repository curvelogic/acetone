//! Integration tests for `fsck` (spec §7, ADR-0012): over real bare git
//! repositories, a healthy graph verifies clean and every class of damage
//! — missing chunk, physically and logically corrupt chunk, garbage
//! manifest, non-commit ref, asymmetric edge maps — produces the right
//! finding, distinctly and named, never a panic.

use std::fs;
use std::path::{Path, PathBuf};

use acetone_graph::fsck::{self, FindingKind, MapId, Severity};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::MapRoot;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_prolly::{BatchOp, ChunkParams, Hash, apply_batch, empty, reachable_chunks};
use acetone_store::{ChunkStore, CommitStore, NewCommit, RefStore};

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
fn fsck_runs_on_a_repo_whose_default_workspace_manifest_is_damaged() {
    // acetone-zhp: when the *default* worktree workspace is damaged,
    // `Repository::open` fail-fasts decoding it, so `fsck::check(&repo)` cannot
    // be reached — exactly the repo fsck exists for. `fsck::check_path` opens
    // only the store and reports the damage instead.
    const WORKSPACE_REF: &str = "refs/worktree/acetone/workspace";
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node("Host", "web1"),
        &NodeRecord::new([], Default::default()),
    )
    .expect("put");
    tx.commit("base", &[], None).expect("commit");

    // Corrupt the default workspace ref to a blob that is neither a workspace
    // tree nor a manifest.
    let store = repo.store();
    let current = store
        .read_ref(WORKSPACE_REF)
        .expect("read ref")
        .expect("workspace ref present");
    let garbage = store.put(b"not a workspace tree or manifest").expect("put");
    store
        .write_ref(WORKSPACE_REF, Some(&current), &garbage)
        .expect("overwrite workspace ref");
    let repo_path = repo.store().git_dir().to_path_buf();
    drop(repo);

    // The bug: opening a full Repository now fails-fast on the damaged manifest.
    assert!(
        Repository::open(&repo_path).is_err(),
        "Repository::open fail-fasts on a damaged default workspace manifest"
    );

    // The fix: fsck::check_path still runs and reports the damage as a finding.
    let report = fsck::check_path(&repo_path).expect("fsck must run on a damaged repo");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Manifest
                && matches!(&f.origin, acetone_graph::Origin::Workspace { reference }
                    if reference == WORKSPACE_REF)
        }),
        "expected a Manifest finding for the damaged default workspace, got {:?}",
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

    // Give the edge real endpoint nodes, so the only fault is the asymmetry —
    // otherwise the referential-integrity check (ADR-0028) would also, and
    // correctly, flag the edge as dangling.
    let empty_nodes = base.nodes.to_root(base.chunk_params).expect("root");
    let nodes = apply_batch(
        store,
        &empty_nodes,
        vec![
            BatchOp::Put(
                node("Host", "a").encode().expect("encode"),
                NodeRecord::new([], Default::default())
                    .encode()
                    .expect("encode record"),
            ),
            BatchOp::Put(
                node("Host", "b").encode().expect("encode"),
                NodeRecord::new([], Default::default())
                    .encode()
                    .expect("encode record"),
            ),
        ],
    )
    .expect("apply_batch nodes");

    let mut manifest = base.clone();
    manifest.edges_fwd = MapRoot::from_root(&fwd);
    manifest.nodes = MapRoot::from_root(&nodes);
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
fn shared_root_hash_with_wrong_height_is_not_memoised_clean() {
    // The cross-version memo must key on (root hash, height), not the hash
    // alone: height lives in the manifest, not the content-addressed chunk,
    // so a manifest that pairs a known-good root hash with a WRONG height
    // must still be verified and flagged — not skipped because the hash was
    // seen. The healthy default workspace is checked first (sorts before
    // "zzz-bad") and seeds the memo; the bad workspace reuses its nodes root
    // hash with height + 1.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 2500);
    let base = repo.workspace_manifest().expect("manifest");
    assert!(
        base.nodes.height >= 2,
        "need a multi-level nodes tree for a real height mismatch"
    );

    let mut bad = base.clone();
    bad.nodes = MapRoot {
        hash: base.nodes.hash,
        height: base.nodes.height + 1,
    };
    let blob = repo.store().put(&bad.encode()).expect("put manifest");
    repo.store()
        .write_ref("refs/acetone/workspaces/zzz-bad", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::CorruptChunk
                && f.map == Some(MapId::Nodes)
                && matches!(&f.origin, acetone_graph::Origin::Workspace { reference }
                    if reference == "refs/acetone/workspaces/zzz-bad")
        }),
        "the wrong-height root must be flagged despite sharing the hash, got {:?}",
        report.findings
    );
}

#[test]
fn reverse_only_edge_is_a_missing_forward_advisory() {
    // Hand-build a manifest whose reverse edge map has an edge the forward
    // map lacks — the mirror of the asymmetry test, exercising the
    // missing_forward direction.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let base = repo.workspace_manifest().expect("manifest");
    let empty_rev = base.edges_rev.to_root(base.chunk_params).expect("root");
    let key = EdgeKey::new(node("Host", "a"), "LINK", node("Host", "b"), Value::Null).expect("key");
    let rev = apply_batch(
        store,
        &empty_rev,
        vec![BatchOp::Put(key.encode_rev().expect("encode"), Vec::new())],
    )
    .expect("apply_batch");
    let mut manifest = base.clone();
    manifest.edges_rev = MapRoot::from_root(&rev);
    let blob = store.put(&manifest.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/revonly", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .advisories()
            .any(|f| f.kind == FindingKind::EdgeAsymmetry
                && f.detail.contains("no matching forward")),
        "expected a missing-forward edge advisory, got {:?}",
        report.findings
    );
    assert!(!report.has_errors(), "asymmetry is advisory, not error");
}

#[test]
fn corrupt_manifest_inside_a_commit_is_a_manifest_finding() {
    // The manifest damage must be caught in commit history too, not only
    // behind a workspace ref: hand-build a commit whose manifest blob is
    // garbage and point a branch at it.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let commit = store
        .create_commit(&NewCommit::new(
            b"garbage bytes that are not a manifest",
            "summary",
            "message",
        ))
        .expect("create commit");
    store
        .write_ref("refs/heads/badmanifest", None, &commit)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Manifest
                && matches!(f.origin, acetone_graph::Origin::Commit { .. })
        }),
        "expected a Manifest finding attributed to the commit, got {:?}",
        report.findings
    );
}

#[test]
fn absent_workspace_manifest_blob_is_a_manifest_finding() {
    // A workspace ref pointing at an object the store does not have (the
    // Ok(None) branch): reported, not skipped or panicked on.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let blob = store.put(b"placeholder manifest").expect("put");
    store
        .write_ref("refs/acetone/workspaces/absent", None, &blob)
        .expect("ref");
    fs::remove_file(loose_object_path(&repo, &blob)).expect("remove");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Manifest
                && matches!(&f.origin, acetone_graph::Origin::Workspace { reference }
                    if reference == "refs/acetone/workspaces/absent")
                && f.detail.contains("absent")
        }),
        "expected an absent-manifest finding, got {:?}",
        report.findings
    );
}

/// Create an annotated tag `name` on `target` with the system git binary
/// (tests may shell out to git; library code never does).
fn git_tag_annotated(repo: &Repository, name: &str, target: &Hash) {
    let git_dir = repo.store().common_dir().to_owned();
    let status = std::process::Command::new("git")
        .args(["-c", "user.name=fsck-test", "-c", "user.email=fsck@test"])
        .arg("-C")
        .arg(&git_dir)
        .args(["tag", "-a", name, "-m", "annotated tag"])
        .arg(target.to_hex())
        .status()
        .expect("run git tag");
    assert!(status.success(), "git tag -a failed");
}

#[test]
fn lightweight_and_annotated_tags_are_both_verified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 40);
    let head = repo.head_commit().expect("head").expect("a commit");

    // A lightweight tag (ref -> commit) is walked like a branch: clean.
    repo.store()
        .write_ref("refs/tags/light", None, &head)
        .expect("tag ref");
    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.is_clean(),
        "lightweight tag on a healthy commit must verify clean: {:?}",
        report.findings
    );

    // An annotated tag (ref -> tag object -> commit) is peeled and its
    // target's manifest verified (acetone-8t3): a healthy target is clean,
    // with no Unverified advisory.
    git_tag_annotated(&repo, "annotated", &head);
    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.is_clean(),
        "an annotated tag on a healthy commit must verify clean: {:?}",
        report.findings
    );
}

#[test]
fn annotated_tag_with_absent_target_commit_is_a_commit_error() {
    // The peel path must surface a damaged target, attributed to the tag
    // ref: an annotated tag whose target commit object has been lost is a
    // Commit error naming that commit, not an advisory or silence.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_many_nodes(&repo, 40);

    // A commit reachable only through the annotated tag.
    let manifest = repo.workspace_manifest().expect("manifest").encode();
    let dangling = repo
        .store()
        .create_commit(&NewCommit::new(&manifest, "s", "tag target"))
        .expect("create commit");
    git_tag_annotated(&repo, "gone", &dangling);
    fs::remove_file(loose_object_path(&repo, &dangling)).expect("remove commit object");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Commit
                && f.detail.contains("absent")
                && matches!(&f.origin, acetone_graph::Origin::Commit { reference, commit }
                    if reference == "refs/tags/gone" && *commit == dangling)
        }),
        "expected a Commit error for the tag's absent target, got {:?}",
        report.findings
    );
}

#[test]
fn symbolic_ref_only_route_to_damage_is_walked() {
    // acetone-5lo: a symbolic ref under refs/heads/* whose (resolved) commit
    // is reachable through no direct ref must still be verified. The damage
    // here — a garbage manifest — is only reachable via the symref chain
    // refs/heads/alias -> refs/acetone/hidden/tip, so before the fix fsck
    // reported nothing at all.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();

    let commit = store
        .create_commit(&NewCommit::new(
            b"garbage bytes that are not a manifest",
            "s",
            "hidden damage",
        ))
        .expect("create commit");
    // The direct ref lives outside every namespace fsck walks directly.
    store
        .write_ref("refs/acetone/hidden/tip", None, &commit)
        .expect("hidden ref");
    store
        .set_head("refs/heads/alias", "refs/acetone/hidden/tip")
        .expect("symbolic ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.findings.iter().any(|f| {
            f.kind == FindingKind::Manifest
                && matches!(&f.origin, acetone_graph::Origin::Commit { reference, .. }
                    if reference == "refs/heads/alias")
        }),
        "the symref-only route to the damaged manifest must be walked, got {:?}",
        report.findings
    );
}

#[test]
fn dangling_symbolic_ref_is_a_named_advisory() {
    // A symbolic ref whose chain ends at an absent ref has nothing to
    // verify; fsck names it (Unverified advisory) rather than staying
    // silent, and it is not an error.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    repo.store()
        .set_head("refs/heads/dangle", "refs/heads/nonexistent")
        .expect("symbolic ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.advisories().any(|f| {
            f.kind == FindingKind::Unverified
                && matches!(&f.origin, acetone_graph::Origin::Ref { reference }
                    if reference == "refs/heads/dangle")
        }),
        "expected an Unverified advisory naming the dangling symref, got {:?}",
        report.findings
    );
    assert!(
        !report.has_errors(),
        "a dangling symref is not damage: {:?}",
        report.findings
    );
}

#[test]
fn shared_edge_map_pair_across_versions_is_checked_once() {
    // acetone-7fe: the edge-symmetry advisory is memoised by the
    // (edges_fwd, edges_rev) root pair, so a history of commits sharing one
    // (asymmetric) edge-map pair produces exactly ONE advisory — attributed
    // to the first version that reaches it — instead of re-scanning (and
    // re-reporting) every commit.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();
    let base = repo.workspace_manifest().expect("manifest");
    let params = base.chunk_params;

    // One forward-only edge (asymmetric), endpoints present in every
    // version's nodes map so no dangling-edge error muddies the count.
    let key = EdgeKey::new(node("Host", "a"), "LINK", node("Host", "b"), Value::Null).expect("key");
    let fwd = apply_batch(
        store,
        &base.edges_fwd.to_root(params).expect("root"),
        vec![BatchOp::Put(
            key.encode_fwd().expect("encode"),
            EdgeRecord::default().encode().expect("encode record"),
        )],
    )
    .expect("apply_batch fwd");

    let endpoint_ops = || {
        vec![
            BatchOp::Put(
                node("Host", "a").encode().expect("encode"),
                NodeRecord::new([], Default::default())
                    .encode()
                    .expect("record"),
            ),
            BatchOp::Put(
                node("Host", "b").encode().expect("encode"),
                NodeRecord::new([], Default::default())
                    .encode()
                    .expect("record"),
            ),
        ]
    };

    // Three commits whose manifests differ (distinct nodes maps) but share
    // the same (edges_fwd, edges_rev) pair.
    let mut parent: Option<Hash> = None;
    for i in 0..3 {
        let mut ops = endpoint_ops();
        ops.push(BatchOp::Put(
            node("Host", &format!("extra{i}")).encode().expect("encode"),
            NodeRecord::new([], Default::default())
                .encode()
                .expect("record"),
        ));
        let nodes = apply_batch(store, &base.nodes.to_root(params).expect("root"), ops)
            .expect("apply_batch nodes");
        let mut manifest = base.clone();
        manifest.nodes = MapRoot::from_root(&nodes);
        manifest.edges_fwd = MapRoot::from_root(&fwd);
        let bytes = manifest.encode();
        let mut new = NewCommit::new(&bytes, "s", "asymmetric history");
        let parents: Vec<Hash> = parent.into_iter().collect();
        new.parents = &parents;
        parent = Some(store.create_commit(&new).expect("create commit"));
    }
    store
        .write_ref("refs/heads/asymhist", None, &parent.expect("tip"))
        .expect("branch ref");

    let report = fsck::check(&repo).expect("fsck");
    let asymmetry: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::EdgeAsymmetry)
        .collect();
    assert_eq!(
        asymmetry.len(),
        1,
        "the shared edge pair must be checked once across the three commits, got {:?}",
        report.findings
    );
    assert!(
        !report.has_errors(),
        "asymmetry is advisory; nothing else may be wrong: {:?}",
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

#[test]
fn non_canonical_map_is_a_history_independence_error() {
    // A `nodes` map whose prolly tree was built with different chunk
    // parameters than the manifest declares is structurally valid (verify_map
    // passes) but not the canonical tree for its contents — a history-
    // independence violation (Invariant #1) the spot-check must catch.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();
    let base = repo.workspace_manifest().expect("manifest");

    let ops: Vec<BatchOp> = (0..300i64)
        .map(|i| {
            let key = node("N", &format!("{i:08}"));
            BatchOp::Put(
                key.encode().expect("encode"),
                NodeRecord::new([], Default::default())
                    .encode()
                    .expect("rec"),
            )
        })
        .collect();

    // A smaller mean chunk size gives different content-defined boundaries.
    let alt_params = ChunkParams::new(64, 6, 512).expect("params");
    let alt_root = apply_batch(
        store,
        &empty(store, alt_params).expect("empty"),
        ops.clone(),
    )
    .expect("alt tree");
    let canonical = apply_batch(store, &empty(store, base.chunk_params).expect("empty"), ops)
        .expect("canonical tree");
    assert_ne!(
        alt_root.hash(),
        canonical.hash(),
        "the two parameter sets must produce different trees for this test"
    );

    // Point the manifest's `nodes` at the non-canonical tree, keeping its own
    // (default) chunk parameters, and expose it as a workspace.
    let mut manifest = base.clone();
    manifest.nodes = MapRoot::from_root(&alt_root);
    let blob = store.put(&manifest.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/noncanon", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::HistoryIndependence),
        "expected a HistoryIndependence finding, got {:?}",
        report.findings
    );
    assert!(report.has_errors(), "a non-canonical map must be an error");
}

#[test]
fn a_dangling_edge_is_a_referential_integrity_error() {
    // U7 (pre-0.1 review / ADR-0028): an edge whose endpoint node is absent from
    // `nodes` is structural damage. fsck must name it as an error, not stay
    // silent. Hand-build a manifest (bypassing the write path, which now rejects
    // this): node "a" exists, "b" does not, and a symmetric edge a-[:LINK]->b.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let store = repo.store();
    let base = repo.workspace_manifest().expect("manifest");
    let params = base.chunk_params;

    let nodes = apply_batch(
        store,
        &base.nodes.to_root(params).expect("root"),
        vec![BatchOp::Put(
            node("Host", "a").encode().expect("encode"),
            NodeRecord::new([], Default::default())
                .encode()
                .expect("encode record"),
        )],
    )
    .expect("apply_batch nodes");

    let key = EdgeKey::new(node("Host", "a"), "LINK", node("Host", "b"), Value::Null).expect("key");
    let fwd = apply_batch(
        store,
        &base.edges_fwd.to_root(params).expect("root"),
        vec![BatchOp::Put(
            key.encode_fwd().expect("encode"),
            EdgeRecord::default().encode().expect("encode record"),
        )],
    )
    .expect("apply_batch fwd");
    // Mirror into edges_rev so the only finding is the dangling edge, not an
    // asymmetry advisory.
    let rev = apply_batch(
        store,
        &base.edges_rev.to_root(params).expect("root"),
        vec![BatchOp::Put(key.encode_rev().expect("encode"), Vec::new())],
    )
    .expect("apply_batch rev");

    let mut manifest = base.clone();
    manifest.nodes = MapRoot::from_root(&nodes);
    manifest.edges_fwd = MapRoot::from_root(&fwd);
    manifest.edges_rev = MapRoot::from_root(&rev);
    let blob = store.put(&manifest.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/dangling", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    let danglers: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::DanglingEdge)
        .collect();
    assert_eq!(
        danglers.len(),
        1,
        "expected one dangling-edge error, got {:?}",
        report.findings
    );
    assert_eq!(danglers[0].severity, Severity::Error);
    assert!(
        danglers[0].detail.contains("has no target node"),
        "detail: {}",
        danglers[0].detail
    );
}
