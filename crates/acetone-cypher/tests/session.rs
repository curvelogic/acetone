//! Integration tests for the library query entry point (ADR-0039, `acetone-vf6`):
//! the `Session` runs reads, writes, `query_at` and `CALL acetone.*` end-to-end
//! against a real on-disk repository — the glue a library consumer no longer has
//! to re-implement.

use std::collections::BTreeMap;

use acetone_cypher::exec::QueryLimits;
use acetone_cypher::exec::value::Value as RtValue;
use acetone_cypher::session::{Outcome, QueryError, Session};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value as MV;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, SchemaEntry};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

/// Seed a `Host` label keyed on `id` plus one node, committed to a branch so
/// there is a past version to query.
fn seed(repo: &Repository) {
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    txn.put_node(
        &NodeKey::new("Host", vec![MV::Int(1)]).expect("key"),
        &NodeRecord::new(
            [],
            BTreeMap::from([("name".to_owned(), MV::String("web".into()))]),
        ),
    )
    .expect("node");
    txn.save().expect("save");
}

/// Commit the current workspace state as a named version, returning its hex id.
fn commit(repo: &Repository, message: &str) -> String {
    repo.begin_write()
        .expect("begin")
        .commit(message, &[], None)
        .expect("commit")
        .to_hex()
}

#[test]
fn run_reads_the_workspace() {
    let (_d, repo) = repo();
    seed(&repo);
    let outcome = Session::new(&repo)
        .run("MATCH (h:Host {id: 1}) RETURN h.name")
        .expect("run");
    match outcome {
        Outcome::Read(result) => {
            assert_eq!(result.columns, vec!["h.name".to_string()]);
            assert_eq!(result.rows.len(), 1);
            assert!(matches!(&result.rows[0][0], RtValue::String(s) if s == "web"));
        }
        Outcome::Write(_) => panic!("a MATCH is not a write"),
    }
}

#[test]
fn run_writes_and_advances_the_workspace() {
    let (_d, repo) = repo();
    seed(&repo);
    let session = Session::new(&repo);

    let outcome = session
        .run("MATCH (h:Host {id: 1}) SET h.name = 'db' RETURN h.id")
        .expect("write");
    match outcome {
        Outcome::Write(result) => assert_eq!(result.stats.properties_set, 1),
        Outcome::Read(_) => panic!("a SET is a write"),
    }

    // The workspace advanced: a fresh read sees the new value, and the store
    // holds it.
    let after = session
        .run("MATCH (h:Host {id: 1}) RETURN h.name")
        .expect("read back");
    assert!(matches!(&after.result().rows[0][0], RtValue::String(s) if s == "db"));
    let record = repo
        .workspace_snapshot()
        .expect("snap")
        .get_node(&NodeKey::new("Host", vec![MV::Int(1)]).expect("key"))
        .expect("read")
        .expect("present");
    assert_eq!(
        record.properties().get("name"),
        Some(&MV::String("db".into()))
    );
}

#[test]
fn query_at_reads_a_past_version_read_only() {
    let (_d, repo) = repo();
    seed(&repo);
    let session = Session::new(&repo);
    // Commit the seed so there is a named past version, then mutate the workspace.
    let commit = commit(&repo, "seed");
    session
        .run("MATCH (h:Host {id: 1}) SET h.name = 'changed' RETURN h.id")
        .expect("mutate");

    // The workspace sees the change; the past commit still shows the old value.
    let now = session
        .run("MATCH (h:Host {id: 1}) RETURN h.name")
        .expect("now");
    assert!(matches!(&now.result().rows[0][0], RtValue::String(s) if s == "changed"));

    let past = session
        .query_at("MATCH (h:Host {id: 1}) RETURN h.name", &commit)
        .expect("query_at");
    assert!(matches!(&past.rows[0][0], RtValue::String(s) if s == "web"));
}

#[test]
fn query_at_rejects_a_write() {
    let (_d, repo) = repo();
    seed(&repo);
    let commit = commit(&repo, "seed");
    let err = Session::new(&repo)
        .query_at(
            "MATCH (h:Host {id: 1}) SET h.name = 'x' RETURN h.id",
            &commit,
        )
        .expect_err("write with a version pin must be rejected");
    assert!(matches!(err, QueryError::WriteAtVersion), "{err:?}");
}

#[test]
fn call_acetone_log_runs_through_the_session_procedures() {
    let (_d, repo) = repo();
    seed(&repo);
    commit(&repo, "first commit");
    let result = Session::new(&repo)
        .run("CALL acetone.log() YIELD commit, subject RETURN subject")
        .expect("call log");
    let subjects: Vec<&str> = result
        .result()
        .rows
        .iter()
        .filter_map(|row| match &row[0] {
            RtValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        subjects.contains(&"first commit"),
        "acetone.log should surface the commit subject, got {subjects:?}"
    );
}

#[test]
fn call_acetone_diff_runs_through_the_session_procedures() {
    let (_d, repo) = repo();
    seed(&repo);
    let session = Session::new(&repo);
    let base = commit(&repo, "base");
    session
        .run("MATCH (h:Host {id: 1}) SET h.name = 'db' RETURN h.id")
        .expect("mutate");
    let head = commit(&repo, "head");

    let result = session
        .run(&format!(
            "CALL acetone.diff('{}', '{}') YIELD kind, label, key RETURN kind, label",
            base, head
        ))
        .expect("call diff");
    // The one changed node appears as a 'modified' Host.
    assert!(
        result.result().rows.iter().any(|row| matches!(
            (&row[0], &row[1]),
            (RtValue::String(k), RtValue::String(l)) if k == "modified" && l == "Host"
        )),
        "acetone.diff should report the modified Host, got {:?}",
        result.result().rows
    );
}

#[test]
fn query_error_renders_with_line_and_column() {
    let (_d, repo) = repo();
    seed(&repo);
    let cypher = "MATCH (h:Host {id: 1}) RETURN"; // dangling RETURN → parse error
    let err = Session::new(&repo).run(cypher).expect_err("parse error");
    let rendered = err.render(cypher);
    assert!(
        rendered.contains("line") || rendered.contains('^') || rendered.contains("1 |"),
        "a parse error should render a caret diagnostic, got: {rendered}"
    );
}

#[test]
fn a_schema_free_repository_binds_leniently() {
    // A repository with raw data but no declared schema stays queryable under
    // openCypher's permissive read semantics (the Lenient bind mode).
    let (_d, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        // Node with a keyless label — no schema entry at all.
        txn.put_node(
            &NodeKey::new("Thing", vec![MV::Int(7)]).expect("key"),
            &NodeRecord::new([], BTreeMap::from([("v".to_owned(), MV::Int(42))])),
        )
        .expect("node");
        txn.save().expect("save");
    }
    // An unknown label under Strict binding would error; Lenient returns rows.
    let outcome = Session::new(&repo)
        .run("MATCH (t:Thing) RETURN t.v")
        .expect("lenient read");
    assert_eq!(outcome.result().rows.len(), 1);
    assert!(matches!(&outcome.result().rows[0][0], RtValue::Int(42)));
}

#[test]
fn run_with_honours_an_explicit_governor_budget() {
    let (_d, repo) = repo();
    seed(&repo);
    // A zero-ish budget makes even a trivial query exceed the cap — proving the
    // limits are threaded through, not ignored.
    let tight = QueryLimits {
        max_work_units: 1,
        ..QueryLimits::default()
    };
    let err = Session::new(&repo)
        .run_with("MATCH (h:Host) RETURN h", &BTreeMap::new(), &tight)
        .expect_err("a 1-unit budget must be exceeded");
    assert!(matches!(err, QueryError::Exec(_)), "{err:?}");
}

#[test]
fn run_with_binds_query_parameters() {
    let (_d, repo) = repo();
    seed(&repo);
    // A `$name` parameter is threaded through to the executor and drives the pin.
    let params = BTreeMap::from([("wanted".to_string(), RtValue::Int(1))]);
    let outcome = Session::new(&repo)
        .run_with(
            "MATCH (h:Host {id: $wanted}) RETURN h.name",
            &params,
            &QueryLimits::default(),
        )
        .expect("parameterised read");
    assert_eq!(outcome.result().rows.len(), 1);
    assert!(matches!(&outcome.result().rows[0][0], RtValue::String(s) if s == "web"));
}

#[test]
fn call_acetone_blame_runs_through_the_session_procedures() {
    let (_d, repo) = repo();
    seed(&repo);
    let first = commit(&repo, "create host");
    let result = Session::new(&repo)
        .run("CALL acetone.blame('Host', 1) YIELD label, key, commit RETURN commit")
        .expect("call blame");
    let commits: Vec<&str> = result
        .result()
        .rows
        .iter()
        .filter_map(|row| match &row[0] {
            RtValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        commits.contains(&first.as_str()),
        "acetone.blame should name the commit that last touched Host/1, got {commits:?}"
    );
}
