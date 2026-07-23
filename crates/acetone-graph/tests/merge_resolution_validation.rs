//! Graph-level violations surfaced through the resolution path (acetone-jm8).
//!
//! ADR-0016: cell conflicts short-circuit graph validation, so a mixed merge
//! (a property cell conflict *and* a latent dangling edge) reports only cell
//! conflicts at merge time — that single-class outcome is designed and pinned
//! here. The gap this file closes (PR #178): once the cell conflicts are
//! resolved, the violation the merge composed must surface as structured
//! conflict data — `Repository::conflicts` re-derives graph violations live
//! over the resolved workspace (ADR-0056), and merge completion refuses with
//! an error that names each violation (`GraphError::MergeViolations`), not an
//! anonymous string.

use acetone_graph::GraphError;
use acetone_graph::conflicts::WorkspaceConflict;
use acetone_graph::merge::{Endpoint, GraphViolation, MergeConflict, MergeOutcome};
use acetone_graph::repo::{InitOptions, Repository, ResolveSide};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_store::CommitStore;
use std::collections::BTreeMap;
use std::path::Path;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn edge(s: u8, d: u8) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

fn record(props: &[(&str, Value)]) -> NodeRecord {
    NodeRecord::new(
        [],
        props
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// The PR #178 scenario: base has nodes 1 and 2 (node 1 carries `v`); theirs
/// deletes node 2 and sets `v=2`; ours adds edge 1→2 and sets `v=1`. The merge
/// has a cell conflict on `v` *and* composes a dangling edge, but per
/// ADR-0016 only the cell conflict is visible at merge time. Leaves the
/// repository merge-in-progress; returns the merge outcome's conflicts.
fn mixed_merge_in_progress(repo: &Repository) -> Vec<MergeConflict> {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(&[("v", Value::Int(0))]))
        .expect("put");
    tx.put_node(&node(2), &record(&[])).expect("put");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(2)).expect("delete");
    tx.put_node(&node(1), &record(&[("v", Value::Int(2))]))
        .expect("put");
    tx.commit("theirs deletes 2, v=2", &[], None)
        .expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("edge");
    tx.put_node(&node(1), &record(&[("v", Value::Int(1))]))
        .expect("put");
    tx.commit("ours adds 1->2, v=1", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(conflicts) => conflicts,
        other => panic!("expected conflicts, got {other:?}"),
    }
}

/// The dangling edge the mixed merge composes, as a violation record.
fn dangling_1_2() -> GraphViolation {
    GraphViolation::DanglingEdge {
        edge: edge(1, 2).encode_fwd().expect("encode"),
        endpoint: node(2).encode().expect("encode"),
        role: Endpoint::Dst,
    }
}

#[test]
fn mixed_merge_reports_only_cell_conflicts_at_merge_time() {
    // ADR-0016 pin: cell conflicts short-circuit before the merged graph
    // exists, so the latent dangling edge is *not* reported at merge time and
    // cell and graph conflicts never coexist in one outcome.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let conflicts = mixed_merge_in_progress(&repo);
    assert!(
        conflicts
            .iter()
            .all(|c| matches!(c, MergeConflict::Cell(_))),
        "merge-time conflicts must be cell-only (ADR-0016), got {conflicts:?}"
    );
    // And while cells remain unresolved, `conflicts()` reports them alone —
    // the partial graph is not validated (ADR-0016's precondition).
    let reported = repo.conflicts().expect("conflicts");
    assert!(
        reported
            .iter()
            .all(|c| matches!(c, WorkspaceConflict::Cell { .. })),
        "pre-resolution conflicts must be cell-only, got {reported:?}"
    );
    assert!(!reported.is_empty());
}

#[test]
fn resolution_surfaces_the_dangling_edge_as_a_structured_conflict() {
    // The PR #178 gap: after `resolve --all-theirs` clears the cell conflict,
    // the resolved graph carries the dangling edge — `conflicts()` must report
    // it as a `GraphViolation`, not return an empty list.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    mixed_merge_in_progress(&repo);

    repo.resolve_all(ResolveSide::Theirs).expect("resolve");

    let conflicts = repo.conflicts().expect("conflicts");
    assert_eq!(
        conflicts,
        vec![WorkspaceConflict::Graph(dangling_1_2())],
        "the resolved workspace's dangling edge must surface via conflicts()"
    );
}

#[test]
fn completion_refusal_names_the_violations() {
    // Completing the merge while the dangling edge remains must refuse with
    // an error that names it — not an anonymous "graph-level violations"
    // string (PR #178: only fsck identified the edge).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    mixed_merge_in_progress(&repo);
    repo.resolve_all(ResolveSide::Theirs).expect("resolve");

    let txn = repo.begin_write().expect("begin");
    let err = txn.commit("finish", &[], None).expect_err("must refuse");
    match &err {
        GraphError::MergeViolations(violations) => {
            assert_eq!(violations, &vec![dangling_1_2()]);
        }
        other => panic!("expected MergeViolations, got {other:?}"),
    }
    let message = err.to_string();
    assert!(
        message.contains("dangling"),
        "refusal must describe the violation class: {message}"
    );
    assert!(
        message.contains("\"N\" [2]"),
        "refusal must name the absent endpoint: {message}"
    );
    assert!(
        message.contains("\"R\""),
        "refusal must name the dangling relationship: {message}"
    );
}

#[test]
fn repairing_after_resolution_clears_conflicts_and_completes() {
    // The surfaced violation is actionable: dropping the dangling edge empties
    // `conflicts()` and lets the completion commit land with two parents.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    mixed_merge_in_progress(&repo);
    repo.resolve_all(ResolveSide::Theirs).expect("resolve");

    let mut tx = repo.begin_write().expect("begin");
    tx.delete_edge(&edge(1, 2)).expect("delete edge");
    tx.save().expect("save");
    assert_eq!(
        repo.conflicts().expect("conflicts"),
        Vec::new(),
        "a repaired workspace must report no conflicts"
    );

    let txn = repo.begin_write().expect("begin");
    let merge_commit = txn.commit("merge other", &[], None).expect("commit");
    let commit = repo
        .store()
        .read_commit(&merge_commit)
        .expect("read")
        .expect("commit");
    assert_eq!(commit.parents.len(), 2, "completion is a two-parent commit");
    assert!(repo.merge_head().expect("merge head").is_none());
}

#[test]
fn pure_graph_violation_merge_reports_violations_via_conflicts() {
    // Regression pin for ADR-0016/ADR-0041's existing behaviour, plus the new
    // surface: a map-clean merge that composes a dangling edge (no cell
    // conflict) demotes to `Conflicts` with `GraphViolation` records at merge
    // time, and `conflicts()` reports the same violations while the merge is
    // in progress.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(&[])).expect("put");
    tx.put_node(&node(2), &record(&[])).expect("put");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(2)).expect("delete");
    tx.commit("theirs deletes 2", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("edge");
    tx.commit("ours adds 1->2", &[], None).expect("commit");

    let outcome = repo.merge("other", "merge other").expect("merge");
    let merge_time: Vec<GraphViolation> = match outcome {
        MergeOutcome::Conflicts(conflicts) => conflicts
            .into_iter()
            .map(|c| match c {
                MergeConflict::Graph(v) => v,
                other => panic!("expected graph violations only, got {other:?}"),
            })
            .collect(),
        other => panic!("expected conflicts, got {other:?}"),
    };
    assert_eq!(merge_time, vec![dangling_1_2()]);

    // The same violations, as structured data, while the merge is in progress.
    let conflicts = repo.conflicts().expect("conflicts");
    assert_eq!(conflicts, vec![WorkspaceConflict::Graph(dangling_1_2())]);
}

#[test]
fn surfaced_violations_are_deterministic() {
    // Invariant #4 extends to reporting: the same workspace yields the same
    // violations in the same (category-then-key) order, call after call.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    mixed_merge_in_progress(&repo);
    repo.resolve_all(ResolveSide::Theirs).expect("resolve");

    let first = repo.conflicts().expect("conflicts");
    let second = repo.conflicts().expect("conflicts");
    assert_eq!(first, second, "conflicts() must be deterministic");
    assert!(!first.is_empty());
}
