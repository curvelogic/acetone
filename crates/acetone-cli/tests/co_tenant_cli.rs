//! End-to-end CLI tests for `acetone init --co-tenant <graph>` (acetone-xg6):
//! the graph is created inside an existing git repository, on its own ref
//! namespace, alongside the code — and is then usable through the shipped CLI.
//! Also covers the init preconditions (acetone-eo7 edge cases).

use std::path::Path;
use std::process::{Command, Output};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Code Dev",
            "-c",
            "user.email=dev@example.invalid",
        ])
        .args(args)
        .output()
        .expect("run git")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf8")
}
fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf8")
}

/// A git repository with one code commit on `main`.
fn code_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("project");
    std::fs::create_dir(&repo).expect("mkdir");
    assert!(
        git(&repo, &["-c", "init.defaultBranch=main", "init"])
            .status
            .success()
    );
    std::fs::write(repo.join("code.txt"), "source").expect("write");
    assert!(git(&repo, &["add", "code.txt"]).status.success());
    assert!(
        git(&repo, &["commit", "-m", "code: initial"])
            .status
            .success()
    );
    (dir, repo)
}

#[test]
fn init_co_tenant_creates_the_graph_beside_code_and_is_cli_usable() {
    let (_dir, repo) = code_repo();
    let head_before = stdout(&git(&repo, &["symbolic-ref", "HEAD"]));
    let main_before = stdout(&git(&repo, &["rev-parse", "refs/heads/main"]));

    let out = acetone(&repo, &["init", "--co-tenant", "assets"]);
    assert!(
        out.status.success(),
        "init --co-tenant failed: {}",
        stderr(&out)
    );
    let msg = stdout(&out);
    assert!(
        msg.contains("co-tenant") && msg.contains("assets"),
        "unexpected init message: {msg}"
    );

    // The graph's marker exists; the code refs and git HEAD are untouched.
    let refs = stdout(&git(&repo, &["for-each-ref", "--format=%(refname)"]));
    assert!(
        refs.contains("refs/acetone/graphs/assets"),
        "co-tenant marker missing; refs:\n{refs}"
    );
    assert_eq!(
        stdout(&git(&repo, &["symbolic-ref", "HEAD"])),
        head_before,
        "git HEAD must be untouched"
    );
    assert_eq!(
        stdout(&git(&repo, &["rev-parse", "refs/heads/main"])),
        main_before,
        "the code branch must be untouched"
    );

    // The shipped CLI opens the co-tenant graph.
    let status = acetone(&repo, &["status"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        stderr(&status)
    );
    assert!(
        stdout(&status).contains("On branch main"),
        "status did not open the co-tenant graph: {}",
        stdout(&status)
    );
}

#[test]
fn init_co_tenant_rejects_a_bad_graph_name() {
    let (_dir, repo) = code_repo();
    let out = acetone(&repo, &["init", "--co-tenant", "a/b"]);
    assert!(
        !out.status.success(),
        "a graph name with a slash must be rejected"
    );
}

#[test]
fn init_co_tenant_on_a_non_git_directory_errors_clearly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = dir.path().join("not-a-repo");
    std::fs::create_dir(&plain).expect("mkdir");
    let out = acetone(&plain, &["init", "--co-tenant", "assets"]);
    assert!(
        !out.status.success(),
        "co-tenant init needs an existing git repository"
    );
}

#[test]
fn init_co_tenant_refuses_a_same_name_reinit_but_allows_a_distinct_graph() {
    // acetone-j6ui: a repository may host several graphs, so a second
    // DISTINCT graph is allowed; only re-initialising an EXISTING name is
    // refused (naming that graph).
    let (_dir, repo) = code_repo();
    assert!(
        acetone(&repo, &["init", "--co-tenant", "assets"])
            .status
            .success()
    );
    // A distinct second graph now succeeds.
    assert!(
        acetone(&repo, &["init", "--co-tenant", "other"])
            .status
            .success(),
        "a second distinct graph must be allowed (acetone-j6ui)"
    );
    // Re-initialising an existing name is refused, naming it.
    let dup = acetone(&repo, &["init", "--co-tenant", "assets"]);
    assert!(!dup.status.success(), "a same-name re-init must be refused");
    assert!(
        stderr(&dup).contains("assets") || stdout(&dup).contains("assets"),
        "the error should name the existing graph: {}",
        stderr(&dup)
    );
}

#[test]
fn init_co_tenant_rejects_a_legacy_standalone_workspace() {
    // A pre-ADR-0014 standalone acetone repository has the legacy shared
    // workspace ref (`refs/acetone/workspaces/default`) but not the modern
    // per-worktree one. Co-tenant init must refuse it too (acetone-eo7).
    let (_dir, repo) = code_repo();
    // Fabricate the legacy workspace ref pointing at a real object. An empty
    // tree is a well-known object git can write without stdin plumbing.
    let empty_tree = stdout(&git(
        &repo,
        &["hash-object", "-w", "-t", "tree", "/dev/null"],
    ));
    let empty_tree = empty_tree.trim();
    assert!(
        git(
            &repo,
            &["update-ref", "refs/acetone/workspaces/default", empty_tree]
        )
        .status
        .success(),
        "failed to fabricate the legacy workspace ref"
    );
    let out = acetone(&repo, &["init", "--co-tenant", "assets"]);
    assert!(
        !out.status.success(),
        "co-tenant init must reject a repo carrying a legacy standalone workspace"
    );
}

#[test]
fn tag_in_co_tenant_mode_lands_in_the_graph_namespace_and_resolves_by_short_name() {
    // The scenario acetone-ujsk exists for: in co-tenant mode a plain
    // `git tag v1` would land in the CODE repo's namespace, invisible to
    // `--at v1`; `acetone tag v1` writes the namespaced path instead.
    let (_dir, repo) = code_repo();
    assert!(
        acetone(&repo, &["init", "--co-tenant", "assets"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["declare-label", "Host", "--key", "name"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["query", "CREATE (:Host {name:'web1'})"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "first"]).status.success());

    let out = acetone(&repo, &["tag", "v1"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Physically namespaced; the code repo's own refs/tags is untouched.
    let refs = stdout(&git(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/tags"],
    ));
    assert!(
        refs.contains("refs/tags/acetone/assets/v1"),
        "tag not namespaced; refs:\n{refs}"
    );
    assert!(
        !refs.contains("refs/tags/v1"),
        "plain refs/tags/v1 must not exist; refs:\n{refs}"
    );

    // The short name time-travels; listing shows the short name only.
    let out = acetone(
        &repo,
        &[
            "query",
            "--at",
            "v1",
            "MATCH (h:Host) RETURN count(h) AS n",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "n\n1", "{}", stderr(&out));
    assert_eq!(stdout(&acetone(&repo, &["tag"])), "v1\n");
}

/// acetone-j6ui: the multi-graph CLI surface — two graphs in one repo, the
/// `--graph` selector, and `graph list`, all through the shipped binary.
#[test]
fn multi_graph_cli_selection_and_listing() {
    let (_dir, repo) = code_repo();

    // Two co-tenant graphs now coexist.
    assert!(
        acetone(&repo, &["init", "--co-tenant", "alpha"])
            .status
            .success(),
        "init alpha"
    );
    assert!(
        acetone(&repo, &["init", "--co-tenant", "beta"])
            .status
            .success(),
        "init beta (a second graph must be allowed)"
    );

    // `graph list` enumerates them, sorted; --json too.
    let list = acetone(&repo, &["graph", "list"]);
    assert!(list.status.success());
    assert_eq!(stdout(&list), "alpha\nbeta\n");
    let list_json = acetone(&repo, &["graph", "list", "--json"]);
    assert_eq!(
        stdout(&list_json).split_whitespace().collect::<String>(),
        "[\"alpha\",\"beta\"]"
    );

    // Plain `status` cannot choose; the error names the flag.
    let ambiguous = acetone(&repo, &["status"]);
    assert!(!ambiguous.status.success());
    assert!(
        stderr(&ambiguous).contains("multiple acetone graphs")
            && stderr(&ambiguous).contains("--graph"),
        "ambiguous status must point at --graph: {}",
        stderr(&ambiguous)
    );

    // Declare a key and write into ALPHA only.
    assert!(
        acetone(
            &repo,
            &["--graph", "alpha", "declare-label", "Doc", "--key", "id"]
        )
        .status
        .success(),
        "declare-label in alpha"
    );
    let w = acetone(
        &repo,
        &["--graph", "alpha", "query", "CREATE (:Doc {id:'a1'})"],
    );
    assert!(w.status.success(), "write to alpha: {}", stderr(&w));

    // ALPHA sees the node; BETA (its own namespace + workspace) does not —
    // isolation through the CLI.
    let a = acetone(
        &repo,
        &[
            "--graph",
            "alpha",
            "query",
            "--format",
            "json",
            "MATCH (d:Doc) RETURN count(d) AS n",
        ],
    );
    assert!(
        stdout(&a).contains("\"n\":1") || stdout(&a).contains("\"n\": 1"),
        "alpha n=1: {}",
        stdout(&a)
    );
    // Beta has no Doc label declared, so a MATCH returns zero rows/nothing;
    // the key check: beta must not see alpha's node. Declare + count on beta.
    assert!(
        acetone(
            &repo,
            &["--graph", "beta", "declare-label", "Doc", "--key", "id"]
        )
        .status
        .success()
    );
    let b = acetone(
        &repo,
        &[
            "--graph",
            "beta",
            "query",
            "--format",
            "json",
            "MATCH (d:Doc) RETURN count(d) AS n",
        ],
    );
    assert!(
        stdout(&b).contains("\"n\":0") || stdout(&b).contains("\"n\": 0"),
        "beta n=0 (isolated): {}",
        stdout(&b)
    );

    // A missing graph names the available ones.
    let bad = acetone(&repo, &["--graph", "gamma", "status"]);
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).contains("no acetone graph named \"gamma\"") && stderr(&bad).contains("alpha"),
        "NoSuchGraph must list available graphs: {}",
        stderr(&bad)
    );

    // fsck scopes to the named graph (does not error on the multi-graph repo).
    let f = acetone(&repo, &["--graph", "alpha", "fsck"]);
    assert!(f.status.success(), "fsck --graph alpha: {}", stderr(&f));

    // The code branch is untouched throughout.
    assert_eq!(
        stdout(&git(&repo, &["symbolic-ref", "HEAD"])).trim(),
        "refs/heads/main"
    );
}
