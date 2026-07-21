//! RefStore contract tests: compare-and-swap semantics, hostile ref-name
//! rejection, and behaviour under concurrent writers.

mod common;

use acetone_store::{ChunkStore, CommitStore, GitStore, NewCommit, RefStore, StoreError};
use common::{git, new_store, repo_path};

const REF: &str = "refs/acetone/workspaces/default";

#[test]
fn read_absent_ref_is_none() {
    let (_dir, store) = new_store();
    assert!(store.read_ref(REF).expect("read_ref").is_none());
}

#[test]
fn named_head_pointer_reads_sets_and_peels_like_git_head() {
    // ADR-0050: the head plumbing takes the pointer ref name, so a co-tenant
    // graph can drive its own `refs/acetone/<graph>/HEAD` instead of git HEAD.
    let (_dir, store) = new_store();
    const PTR: &str = "refs/acetone/g/HEAD";
    const BRANCH: &str = "refs/heads/acetone/g/main";

    // The git HEAD fast path is unaffected: a fresh store's HEAD is the unborn
    // default branch (Some, not detached).
    assert!(
        store.read_head("HEAD").expect("read HEAD").is_some(),
        "git HEAD still reads via the fast path"
    );

    // Point the private pointer at an unborn branch: read_head returns the
    // branch name (like an unborn git HEAD), but there is no commit yet.
    store.set_head(PTR, BRANCH).expect("set head pointer");
    assert_eq!(
        store.read_head(PTR).expect("read pointer").as_deref(),
        Some(BRANCH),
        "the pointer's symbolic target is the current branch, even unborn"
    );
    assert!(
        store.head_commit_id(PTR).expect("peel unborn").is_none(),
        "an unborn pointer has no commit"
    );

    // Give the branch a commit: the pointer now peels to it, and still reads
    // its (now born) branch as the current branch.
    let commit = store
        .create_commit(&NewCommit::new(b"m", "s", "commit on the graph branch"))
        .expect("create_commit");
    store
        .write_ref(BRANCH, None, &commit)
        .expect("create branch");
    assert_eq!(
        store.read_head(PTR).expect("read pointer").as_deref(),
        Some(BRANCH),
        "a born branch is still the current branch"
    );
    assert_eq!(
        store.head_commit_id(PTR).expect("peel"),
        Some(commit),
        "the pointer follows its symref to the branch tip commit"
    );

    // An object-valued pointer (a detached graph head) reads as no branch,
    // mirroring a detached git HEAD.
    const DETACHED: &str = "refs/acetone/g/detached";
    store
        .write_ref(DETACHED, None, &commit)
        .expect("object-valued ref");
    assert!(
        store.read_head(DETACHED).expect("read detached").is_none(),
        "a detached (object-valued) pointer has no current branch"
    );

    // An absent pointer is simply no current branch, not an error.
    assert!(
        store
            .read_head("refs/acetone/absent/HEAD")
            .expect("read absent")
            .is_none()
    );

    // A pointer that is neither bare `HEAD` nor under `refs/` is rejected at the
    // validation door, not silently treated as absent.
    assert!(matches!(
        store.read_head("bogus-pointer"),
        Err(StoreError::InvalidRefName { .. })
    ));
    assert!(matches!(
        store.set_head("bogus-pointer", BRANCH),
        Err(StoreError::InvalidRefName { .. })
    ));
}

#[test]
fn create_update_read_round_trip() {
    let (_dir, store) = new_store();
    let v1 = store.put(b"v1").expect("put");
    let v2 = store.put(b"v2").expect("put");

    // Create: expected = None.
    store.write_ref(REF, None, &v1).expect("create");
    assert_eq!(store.read_ref(REF).expect("read"), Some(v1));

    // Update: expected = current.
    store.write_ref(REF, Some(&v1), &v2).expect("update");
    assert_eq!(store.read_ref(REF).expect("read"), Some(v2));
}

#[test]
fn create_fails_if_ref_exists() {
    let (_dir, store) = new_store();
    let v1 = store.put(b"v1").expect("put");
    let v2 = store.put(b"v2").expect("put");
    store.write_ref(REF, None, &v1).expect("create");
    match store.write_ref(REF, None, &v2) {
        Err(StoreError::CasFailed { name }) => assert_eq!(name, REF),
        other => panic!("expected CasFailed, got {other:?}"),
    }
    assert_eq!(store.read_ref(REF).expect("read"), Some(v1));
}

#[test]
fn create_fails_if_ref_exists_even_with_the_same_value() {
    // acetone-0ej: gix's MustNotExist treats a value-equal edit as a no-op
    // success. A create (expected = None) of a ref that already exists must
    // fail even when it already holds the same target — the caller relies on
    // the error to detect a lost create race.
    let (_dir, store) = new_store();
    let v1 = store.put(b"v1").expect("put");
    store.write_ref(REF, None, &v1).expect("create");
    match store.write_ref(REF, None, &v1) {
        Err(StoreError::CasFailed { name }) => assert_eq!(name, REF),
        other => panic!("expected CasFailed on a value-equal create, got {other:?}"),
    }
}

#[test]
fn stale_expected_value_is_rejected() {
    let (_dir, store) = new_store();
    let v1 = store.put(b"v1").expect("put");
    let v2 = store.put(b"v2").expect("put");
    let v3 = store.put(b"v3").expect("put");

    store.write_ref(REF, None, &v1).expect("create");
    store.write_ref(REF, Some(&v1), &v2).expect("update");

    // A writer still holding v1 must lose.
    match store.write_ref(REF, Some(&v1), &v3) {
        Err(StoreError::CasFailed { name }) => assert_eq!(name, REF),
        other => panic!("expected CasFailed, got {other:?}"),
    }
    assert_eq!(store.read_ref(REF).expect("read"), Some(v2));
}

#[test]
fn update_of_absent_ref_is_rejected() {
    let (_dir, store) = new_store();
    let v1 = store.put(b"v1").expect("put");
    match store.write_ref(REF, Some(&v1), &v1) {
        Err(StoreError::CasFailed { name }) => assert_eq!(name, REF),
        other => panic!("expected CasFailed, got {other:?}"),
    }
}

#[test]
fn concurrent_writer_simulation_two_handles() {
    // Two independent store handles on the same repository: the slower
    // writer's CAS must fail against the faster writer's update.
    let (dir, store_a) = new_store();
    let store_b = GitStore::open(&repo_path(&dir)).expect("open second handle");

    let v1 = store_a.put(b"v1").expect("put");
    let v2 = store_a.put(b"v2").expect("put");
    let v3 = store_b.put(b"v3").expect("put");

    store_a.write_ref(REF, None, &v1).expect("create");

    // Both handles read v1; A wins the race.
    let seen_by_b = store_b.read_ref(REF).expect("read").expect("present");
    store_a.write_ref(REF, Some(&v1), &v2).expect("A updates");
    match store_b.write_ref(REF, Some(&seen_by_b), &v3) {
        Err(StoreError::CasFailed { .. }) => {}
        other => panic!("expected CasFailed for stale writer, got {other:?}"),
    }
    assert_eq!(store_b.read_ref(REF).expect("read"), Some(v2));
}

#[test]
fn racing_creators_exactly_one_wins() {
    // N threads race to create the same ref, each with its own store
    // handle and its own target value. CAS create must admit exactly one.
    let (dir, setup) = new_store();
    let candidates: Vec<_> = (0..8u32)
        .map(|i| setup.put(format!("candidate-{i}").as_bytes()).expect("put"))
        .collect();
    drop(setup);
    let path = repo_path(&dir);

    let outcomes: Vec<Result<(), StoreError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|hash| {
                let path = path.clone();
                scope.spawn(move || {
                    let store = GitStore::open(&path).expect("open per-thread");
                    store.write_ref(REF, None, hash)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });

    let winners = outcomes.iter().filter(|o| o.is_ok()).count();
    assert_eq!(
        winners, 1,
        "exactly one racing creator must win: {outcomes:?}"
    );

    // The ref's final value is the winner's candidate.
    let store = GitStore::open(&path).expect("reopen");
    let final_value = store.read_ref(REF).expect("read").expect("present");
    let winner_index = outcomes.iter().position(|o| o.is_ok()).expect("a winner");
    assert_eq!(final_value, candidates[winner_index]);
}

#[test]
fn invalid_ref_names_are_rejected() {
    let (_dir, store) = new_store();
    let hash = store.put(b"x").expect("put");

    let hostile = [
        "",
        "foo",                           // not under refs/
        "HEAD",                          // valid for git, outside refs/
        "refs",                          // no component
        "refs/",                         // empty component
        "refs//x",                       // empty component
        "refs/acetone/../../etc/passwd", // path traversal
        "refs/acetone/.hidden",          // component starts with '.'
        "refs/acetone/x..y",             // double dot
        "refs/acetone/x.lock",           // reserved suffix
        "refs/acetone/x/",               // trailing slash
        "refs/acetone/x y",              // space
        "refs/acetone/x~1",              // revision syntax
        "refs/acetone/x^y",              // revision syntax
        "refs/acetone/x:y",              // refspec syntax
        "refs/acetone/x[y",              // pattern syntax
        "refs/acetone/x\\y",             // backslash
        "refs/acetone/x\u{7}y",          // control character
        "refs/acetone/@{upstream}",      // reflog syntax
        "refs/acetone/x.",               // trailing dot
    ];
    for name in hostile {
        match store.write_ref(name, None, &hash) {
            Err(StoreError::InvalidRefName { .. }) => {}
            other => panic!("expected InvalidRefName for {name:?}, got {other:?}"),
        }
        match store.read_ref(name) {
            Err(StoreError::InvalidRefName { .. }) => {}
            other => panic!("expected InvalidRefName for {name:?} on read, got {other:?}"),
        }
    }
}

#[test]
fn symbolic_ref_is_error_not_value() {
    // A hostile or foreign tool can plant a symbolic ref where acetone
    // expects a direct one; reading it must be a typed error, not a panic
    // or a bogus hash.
    let (dir, store) = new_store();
    let v1 = store.put(b"v1").expect("put");
    store
        .write_ref("refs/acetone/workspaces/real", None, &v1)
        .expect("create");
    git(
        &repo_path(&dir),
        &[
            "symbolic-ref",
            "refs/acetone/workspaces/default",
            "refs/acetone/workspaces/real",
        ],
    );
    match store.read_ref(REF) {
        Err(StoreError::SymbolicRef { name }) => assert_eq!(name, REF),
        other => panic!("expected SymbolicRef, got {other:?}"),
    }
}
