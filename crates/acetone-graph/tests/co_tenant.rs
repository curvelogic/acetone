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

/// Create a loose blob + tree + commit reachable *only* from `refname` (using
/// a throwaway index so the real index and `main` are untouched), and return
/// the blob's hash. Models a code object a cloned repo carries under
/// `refs/remotes/*` — reachable from a non-graph ref, not from any branch.
fn loose_commit_only_on_ref(project: &Path, refname: &str) -> String {
    let index = project.join(".git/acetone-test-index");
    let run = |args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .env("GIT_INDEX_FILE", &index)
            // A deterministic identity so commit-tree needs no ambient git
            // config (CI runners have none).
            .args([
                "-c",
                "user.name=Code Dev",
                "-c",
                "user.email=dev@example.invalid",
            ])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    };
    std::fs::write(project.join("remote-only.txt"), "remote-only content").expect("write");
    let blob = run(&["hash-object", "-w", "remote-only.txt"]);
    run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},remote-only.txt"),
    ]);
    let tree = run(&["write-tree"]);
    let commit = run(&["commit-tree", &tree, "-m", "remote-only commit"]);
    run(&["update-ref", refname, &commit]);
    std::fs::remove_file(project.join("remote-only.txt")).ok();
    std::fs::remove_file(&index).ok();
    blob
}

/// The stem (`pack-<hash>`) of acetone's current consolidation pack, read from
/// its sidecar list under the shared `.git`.
fn acetone_pack_stem(project: &Path) -> String {
    let text = std::fs::read_to_string(project.join(".git/acetone-consolidation-packs"))
        .expect("consolidation-packs sidecar");
    text.lines()
        .rfind(|l| !l.is_empty())
        .expect("at least one consolidation pack")
        .to_owned()
}

/// Count loose object files under `project/.git/objects` (the two-hex shards).
fn loose_object_count(project: &Path) -> usize {
    let objects = project.join(".git/objects");
    let Ok(shards) = std::fs::read_dir(&objects) else {
        return 0;
    };
    let mut count = 0;
    for shard in shards.flatten() {
        let name = shard.file_name();
        let name = name.to_string_lossy();
        if name.len() == 2
            && name.chars().all(|c| c.is_ascii_hexdigit())
            && let Ok(files) = std::fs::read_dir(shard.path())
        {
            count += files.flatten().count();
        }
    }
    count
}

/// Whether object `hash` appears in any pack index under `project`.
fn packed_object_exists(project: &Path, hash: &str) -> bool {
    let pack_dir = project.join(".git/objects/pack");
    let Ok(entries) = std::fs::read_dir(&pack_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("idx") {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(project)
                .args(["verify-pack", "-v"])
                .arg(&path)
                .output()
                .expect("verify-pack");
            if String::from_utf8_lossy(&out.stdout).contains(hash) {
                return true;
            }
        }
    }
    false
}

#[test]
fn gc_scopes_to_the_graph_and_leaves_code_storage_untouched() {
    // Phase 8 exit criterion 2 (gc half), reading (B) — ADR-0051, Greg-ruled.
    // acetone gc packs only objects reachable from the *graph's* refs; a
    // co-tenant's code objects form a prune guard, so their storage is left
    // exactly as git had it. The discriminator: after gc the code blob is STILL
    // LOOSE (acetone neither packed nor pruned it) and still retrievable, while
    // the graph's own loose objects have been consolidated away. Reading (A) —
    // the repo-global repack that shipped in acetone-iva — would instead have
    // drawn the code blob into acetone's pack and deleted its loose file; that
    // is precisely what (B) must not do, so this test fails under a regression
    // to (A).
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

    let loose_before = loose_object_count(&project);
    graph.gc().expect("gc");
    let loose_after = loose_object_count(&project);

    // (B): the code blob's storage is untouched — still loose, still retrievable,
    // and NOT drawn into any acetone pack.
    assert!(
        loose_object_exists(&project, &code_blob),
        "reading B: gc must leave the code blob's loose file in place (not pack/prune it)"
    );
    assert!(
        !packed_object_exists(&project, &code_blob),
        "reading B: the code blob must not appear in an acetone pack"
    );
    assert!(
        object_retrievable(&project, &code_blob),
        "the code blob must remain retrievable"
    );
    assert!(
        object_retrievable(&project, &code_commit),
        "the code commit must remain retrievable"
    );

    // gc really did consolidate the graph — not a no-op that would trivially
    // leave the code blob loose: the graph's loose objects were packed away.
    assert!(
        loose_after < loose_before,
        "gc must consolidate the graph's loose objects ({loose_before} -> {loose_after})"
    );

    // And the code branch itself is untouched.
    assert_eq!(
        git(&project, &["rev-parse", "refs/heads/main"]),
        code_commit
    );
}

#[test]
fn a_kept_pack_survives_a_foreign_git_gc_and_falls_when_the_keep_is_removed() {
    // ADR-0053: acetone marks its consolidation pack `.keep` so a foreign
    // `git gc`/`git repack` — which a co-tenant repo's owner runs routinely,
    // including git's automatic `gc.auto` — leaves acetone's content-aware
    // REF_DELTAs (ADR-0011) intact instead of re-deltifying them to a poor
    // baseline. This proves the `.keep` is present and *load-bearing*: with it
    // the pack survives a full `git repack -a -d`; without it the same repack
    // folds it away.
    let (project, _dir, _c, _b) = code_repo();
    let graph =
        Repository::init_co_tenant(&project, "g", InitOptions::default()).expect("init_co_tenant");
    seed_graph(&graph, 60);
    for round in 0..30i64 {
        let mut tx = graph.begin_write().expect("begin");
        tx.put_node(
            &node(round % 60),
            &NodeRecord::new(
                [],
                BTreeMap::from([("v".to_owned(), Value::Int(3000 + round))]),
            ),
        )
        .expect("node");
        tx.commit(&format!("graph: churn {round}"), &[], None)
            .expect("commit");
    }
    let stats = graph.gc().expect("gc");
    assert!(
        stats.deltas > 0,
        "churn should produce REF_DELTAs worth protecting (got {})",
        stats.deltas
    );

    let stem = acetone_pack_stem(&project);
    let pack = project.join(format!(".git/objects/pack/{stem}.pack"));
    let keep = project.join(format!(".git/objects/pack/{stem}.keep"));
    assert!(pack.exists(), "acetone pack {stem}.pack present after gc");
    assert!(keep.exists(), "acetone pack must be marked .keep after gc");
    let pack_size = std::fs::metadata(&pack).expect("meta").len();

    // A foreign, aggressive repack of the whole repository.
    git(&project, &["repack", "-a", "-d"]);

    // With the `.keep`, git left acetone's pack (and its deltas) exactly in place.
    assert!(pack.exists(), "kept pack must survive `git repack -a -d`");
    assert_eq!(
        std::fs::metadata(&pack).expect("meta").len(),
        pack_size,
        "kept pack must be byte-unchanged by the foreign repack"
    );
    assert!(
        keep.exists(),
        "the .keep marker survives the foreign repack too"
    );
    git(&project, &["fsck", "--strict"]);
    // Reopen (a fresh handle sees the new pack layout) and confirm the graph reads.
    let reopened = Repository::open(&project).expect("reopen");
    assert!(
        reopened
            .workspace_snapshot()
            .expect("snapshot")
            .get_node(&node(0))
            .expect("get")
            .is_some(),
        "graph still readable after a foreign repack"
    );

    // The `.keep` is load-bearing: remove it and the same repack folds the pack
    // away — proving it was what protected acetone's deltas.
    std::fs::remove_file(&keep).expect("rm keep");
    git(&project, &["repack", "-a", "-d"]);
    assert!(
        !pack.exists(),
        "without .keep, `git repack -a -d` must fold acetone's pack into git's"
    );
    // No object was lost — git repacked them into its own pack.
    git(&project, &["fsck", "--strict"]);
    let reopened = Repository::open(&project).expect("reopen");
    assert!(
        reopened
            .workspace_snapshot()
            .expect("snapshot")
            .get_node(&node(0))
            .expect("get")
            .is_some(),
        "graph still readable after its pack was folded into git's"
    );
}

#[test]
fn gc_guards_code_reachable_only_from_remote_tracking_refs() {
    // Reading B, the realistic co-tenant shape: a graph added to a *cloned* code
    // repo, which carries the code under `refs/remotes/origin/*` (and may carry
    // notes/stash). Those are the user's code, not the graph's — gc must guard
    // them exactly like `refs/heads/*` code branches. A code object reachable
    // ONLY from a remote-tracking ref must stay loose and out of acetone's pack;
    // owning it (the fallthrough bug) would draw a clone's code into acetone's
    // pack — reading A in disguise.
    let (project, _dir, _code_commit, _code_blob) = code_repo();
    let remote_blob = loose_commit_only_on_ref(&project, "refs/remotes/origin/main");
    assert!(
        loose_object_exists(&project, &remote_blob),
        "precondition: remote-only code blob is loose"
    );

    let graph =
        Repository::init_co_tenant(&project, "g", InitOptions::default()).expect("init_co_tenant");
    seed_graph(&graph, 30);
    for round in 0..10i64 {
        let mut tx = graph.begin_write().expect("begin");
        tx.put_node(
            &node(round % 30),
            &NodeRecord::new(
                [],
                BTreeMap::from([("v".to_owned(), Value::Int(2000 + round))]),
            ),
        )
        .expect("node");
        tx.commit(&format!("graph: churn {round}"), &[], None)
            .expect("commit");
    }

    graph.gc().expect("gc");

    // The remote-tracking code object is guarded: still loose, never packed by
    // acetone, still retrievable.
    assert!(
        loose_object_exists(&project, &remote_blob),
        "reading B: gc must not pack/prune a code object reachable only from refs/remotes/*"
    );
    assert!(
        object_retrievable(&project, &remote_blob),
        "the remote-only code blob must remain retrievable"
    );
    // The remote-tracking ref itself is untouched.
    assert!(
        object_retrievable(
            &project,
            &git(&project, &["rev-parse", "refs/remotes/origin/main"])
        ),
        "the remote-tracking ref's commit must remain retrievable"
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
