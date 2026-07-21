//! Co-tenant mode (ADR-0050, acetone-mgf): an acetone graph living inside an
//! ordinary code repository, on its own ref namespace. This is Phase 8 exit
//! criterion 1 — a graph on its own ref inside a repo that also holds code,
//! with code branches and graph branches coexisting untouched.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use acetone_graph::repo::{InitOptions, Repository};
use acetone_graph::{Rechunk, rewrite_history};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, SchemaEntry};
use acetone_prolly::ChunkParams;
use acetone_store::RefStore;

/// Run `git -C <dir> <args>`, asserting success, returning trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // A deterministic identity so committing needs no ambient git config.
        .args([
            "-c",
            "user.name=Code Dev",
            "-c",
            "user.email=dev@example.invalid",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn node(key: i64) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(key)]).expect("key")
}

/// Create a code repo with one commit on `main`, and return (project dir,
/// tempdir guard, code commit hash, code blob hash).
fn code_repo() -> (std::path::PathBuf, tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("tmp");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).expect("mkdir");
    git(&project, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(project.join("code.txt"), "unique source content").expect("write");
    git(&project, &["add", "code.txt"]);
    git(&project, &["commit", "-m", "code: initial"]);
    let commit = git(&project, &["rev-parse", "refs/heads/main"]);
    let blob = git(&project, &["rev-parse", "HEAD:code.txt"]);
    (project, dir, commit, blob)
}

/// Seed a co-tenant graph with a schema and `n` committed nodes.
fn seed_graph(graph: &Repository, n: i64) {
    let mut tx = graph.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "N".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    for i in 0..n {
        tx.put_node(
            &node(i),
            &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
        )
        .expect("node");
    }
    tx.commit("graph: seed", &[], None).expect("commit");
}

/// Whether object `hash` exists as a *loose* file in `project/.git/objects`.
fn loose_object_exists(project: &Path, hash: &str) -> bool {
    project
        .join(".git/objects")
        .join(&hash[..2])
        .join(&hash[2..])
        .exists()
}

/// Whether object `hash` is retrievable at all (loose or packed).
fn object_retrievable(project: &Path, hash: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["cat-file", "-e", hash])
        .status()
        .expect("cat-file")
        .success()
}

#[test]
fn gc_consolidates_code_objects_without_losing_them() {
    // Phase 8 exit criterion 2 (gc half): the graph's gc must keep every object
    // reachable from the code history (never from any graph ref).
    //
    // acetone's gc repacks its *reachable set* (seeded from ALL refs) and prunes
    // the now-redundant loose copies (consolidate::prune_loose only deletes
    // objects it just packed). So "the code object still exists" is NOT the
    // discriminating property — a non-reachable object is never pruned either,
    // so that assertion would pass even under a graph-scoped-reachability
    // regression. What actually distinguishes the two is that the code blob is
    // DRAWN INTO the pack: its loose file is consolidated away yet it stays
    // retrievable. Under graph-scoped reachability the code blob would not be
    // packed, so its loose file would remain and this test would fail.
    let (project, _dir, code_commit, code_blob) = code_repo();
    // Precondition: the freshly-committed code blob is loose, not yet packed.
    assert!(
        loose_object_exists(&project, &code_blob),
        "precondition: code blob is loose before gc"
    );

    let graph =
        Repository::init_co_tenant(&project, "g", InitOptions::default()).expect("init_co_tenant");
    seed_graph(&graph, 50);
    // Churn the graph so gc has loose objects to consolidate.
    for round in 0..20i64 {
        let mut tx = graph.begin_write().expect("begin");
        tx.put_node(
            &node(round % 50),
            &NodeRecord::new(
                [],
                BTreeMap::from([("v".to_owned(), Value::Int(1000 + round))]),
            ),
        )
        .expect("node");
        tx.commit(&format!("graph: churn {round}"), &[], None)
            .expect("commit");
    }

    graph.gc().expect("gc");

    // The discriminator: the code blob was in gc's reachable set, so it is now
    // packed (loose file gone) AND still retrievable. A graph-scoped gc would
    // leave the loose file in place, failing the first assertion.
    assert!(
        !loose_object_exists(&project, &code_blob),
        "gc must consolidate the code blob into a pack (proving it was reachable)"
    );
    assert!(
        object_retrievable(&project, &code_blob),
        "the consolidated code blob must remain retrievable"
    );
    assert!(
        object_retrievable(&project, &code_commit),
        "the code commit must remain retrievable"
    );
    // And the code branch itself is untouched.
    assert_eq!(
        git(&project, &["rev-parse", "refs/heads/main"]),
        code_commit
    );
}

#[test]
fn migrate_rewrites_only_graph_refs_leaving_code_untouched() {
    // Phase 8 exit criterion 2 (migrate half): a history-rewriting migrate of
    // the graph must rewrite only the graph's refs; the code's refs and git
    // HEAD are untouched.
    let (project, _dir, code_commit, _blob) = code_repo();
    let graph =
        Repository::init_co_tenant(&project, "g", InitOptions::default()).expect("init_co_tenant");
    seed_graph(&graph, 50);

    let graph_branch_before = graph
        .store()
        .read_ref("refs/heads/acetone/g/main")
        .expect("read")
        .expect("graph branch exists");
    // The exact set of branch NAMES before migrate — nothing under refs/heads/
    // should appear or disappear (only the graph branch's target may change).
    let branch_names_before = git(
        &project,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );

    // Re-chunk migrate: version-preserving but rewrites every graph commit hash.
    let new_params = ChunkParams::new(2048, 13, 131072).expect("params");
    rewrite_history(&graph, &Rechunk::new(new_params)).expect("migrate");

    // The graph's branch was rewritten...
    let graph_branch_after = graph
        .store()
        .read_ref("refs/heads/acetone/g/main")
        .expect("read")
        .expect("graph branch still exists");
    assert_ne!(
        graph_branch_before, graph_branch_after,
        "migrate must rewrite the graph branch"
    );
    // ...while the code branch and git HEAD are completely untouched.
    assert_eq!(
        git(&project, &["rev-parse", "refs/heads/main"]),
        code_commit,
        "migrate must not touch the code branch"
    );
    assert_eq!(
        git(&project, &["symbolic-ref", "HEAD"]),
        "refs/heads/main",
        "migrate must not touch git HEAD"
    );
    assert_eq!(
        git(
            &project,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"]
        ),
        branch_names_before,
        "migrate must not create or delete any branch"
    );
    // The graph data survives the rewrite.
    let reopened = Repository::open(&project).expect("reopen");
    assert!(
        reopened
            .workspace_snapshot()
            .expect("snapshot")
            .get_node(&node(0))
            .expect("get")
            .is_some(),
        "graph data preserved across migrate"
    );
}

#[test]
fn a_graph_co_tenants_a_code_repo_without_touching_it() {
    let dir = tempfile::tempdir().expect("tmp");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).expect("mkdir");

    // 1. An ordinary code repository with one commit on `main`.
    git(&project, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(project.join("README.md"), "the source code").expect("write");
    git(&project, &["add", "README.md"]);
    git(&project, &["commit", "-m", "code: initial commit"]);
    let code_commit = git(&project, &["rev-parse", "refs/heads/main"]);
    let code_head = git(&project, &["symbolic-ref", "HEAD"]);
    assert_eq!(code_head, "refs/heads/main", "sanity: code is on main");

    // 2. Add an acetone graph as a co-tenant of that repository.
    let graph =
        Repository::init_co_tenant(&project, "g", InitOptions::default()).expect("init_co_tenant");
    {
        let mut tx = graph.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.put_node(
            &node(1),
            &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(42))])),
        )
        .expect("node");
        tx.commit("graph: first commit", &[], None).expect("commit");
    }

    // 3. The code is completely untouched.
    assert_eq!(
        git(&project, &["rev-parse", "refs/heads/main"]),
        code_commit,
        "the code branch must not move"
    );
    assert_eq!(
        git(&project, &["symbolic-ref", "HEAD"]),
        "refs/heads/main",
        "the user's git HEAD must stay on their code"
    );

    // 4. The graph lives on its own ref namespace, with its own head pointer.
    let graph_branch = git(&project, &["rev-parse", "refs/heads/acetone/g/main"]);
    assert!(!graph_branch.is_empty(), "the graph branch has a commit");
    assert_ne!(graph_branch, code_commit, "graph and code are distinct");
    assert_eq!(
        git(&project, &["symbolic-ref", "refs/acetone/g/HEAD"]),
        "refs/heads/acetone/g/main",
        "the graph's current-branch pointer is its private symref, not HEAD"
    );
    // Both branches are listed by git — they coexist.
    let branches = git(
        &project,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );
    assert!(branches.contains("refs/heads/main"), "code branch present");
    assert!(
        branches.contains("refs/heads/acetone/g/main"),
        "graph branch present alongside it"
    );

    // 5. Reopen: the layout is detected as co-tenant, and the graph reads back.
    drop(graph);
    let reopened = Repository::open(&project).expect("reopen");
    assert_eq!(
        reopened.namespace().branch_prefix(),
        "refs/heads/acetone/g/",
        "open detects co-tenant mode from the graph marker"
    );
    assert_eq!(
        reopened.current_branch().expect("branch").as_deref(),
        Some("refs/heads/acetone/g/main"),
        "the graph is on its namespaced branch"
    );
    assert!(
        reopened
            .workspace_snapshot()
            .expect("snapshot")
            .get_node(&node(1))
            .expect("get")
            .is_some(),
        "the graph's data survives the round trip"
    );
    // The code working tree is still intact.
    assert_eq!(
        std::fs::read_to_string(project.join("README.md")).expect("read"),
        "the source code"
    );
}

#[test]
fn an_interrupted_co_tenant_init_never_lets_a_write_reach_code() {
    // Marker-first ordering (ADR-0050): if init is interrupted after the graph
    // marker is written but before the workspace exists, `open` must still see
    // co-tenant mode and refuse (NoWorkspace) — never fall back to standalone,
    // which would let a later write land on the user's `refs/heads/main`.
    let dir = tempfile::tempdir().expect("tmp");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).expect("mkdir");
    git(&project, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(project.join("README.md"), "code").expect("write");
    git(&project, &["add", "README.md"]);
    git(&project, &["commit", "-m", "code"]);
    let code_commit = git(&project, &["rev-parse", "refs/heads/main"]);

    // Simulate a crash mid-init: only the graph marker exists (a direct ref at
    // the empty blob); no workspace ref, no head symref.
    let empty_blob = git(&project, &["hash-object", "-w", "/dev/null"]);
    git(
        &project,
        &["update-ref", "refs/acetone/graphs/g", &empty_blob],
    );

    // `open` must detect co-tenant mode and refuse — not present a usable
    // standalone repo whose writes would move the code branch.
    assert!(
        Repository::open(&project).is_err(),
        "an interrupted co-tenant init must not open as a usable standalone repo"
    );
    // The code is untouched regardless.
    assert_eq!(
        git(&project, &["rev-parse", "refs/heads/main"]),
        code_commit
    );
    assert_eq!(git(&project, &["symbolic-ref", "HEAD"]), "refs/heads/main");
}

#[test]
fn init_co_tenant_rejects_bad_graph_names_and_duplicates() {
    let dir = tempfile::tempdir().expect("tmp");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).expect("mkdir");
    git(&project, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(project.join("f"), "x").expect("write");
    git(&project, &["add", "f"]);
    git(&project, &["commit", "-m", "code"]);

    for bad in ["", "a/b", "..", "a..b", ".hidden", "a b", "a~b"] {
        assert!(
            Repository::init_co_tenant(&project, bad, InitOptions::default()).is_err(),
            "graph name {bad:?} must be rejected"
        );
    }

    // A valid graph initialises once; a second attempt for the same name fails.
    Repository::init_co_tenant(&project, "g", InitOptions::default()).expect("first init");
    assert!(
        Repository::init_co_tenant(&project, "g", InitOptions::default()).is_err(),
        "re-initialising the same graph must fail"
    );
}
