//! Integration tests for the import orchestration (spec §7, ADR-0021):
//! schema-driven transform, bulk upsert, provenance trailers, no-op detection
//! and `--branch` isolation. Uses an in-memory mock extractor; the built-in
//! CSV/JSON extractors are tested in `acetone-cli`.

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::import::{
    EndpointRef, ImportError, ImportOptions, ImportOutcome, ImportRecord, Provenance,
    SourceExtractor,
};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_graph::{GraphError, import};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::schema::{LabelDef, PropertyType, RelTypeDef, SchemaEntry};

/// A canned extractor returning a fixed record list.
struct Mock {
    name: String,
    records: Vec<ImportRecord>,
}

impl SourceExtractor for Mock {
    fn name(&self) -> &str {
        &self.name
    }
    fn next_record(&mut self) -> Result<Option<ImportRecord>, ImportError> {
        if self.records.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.records.remove(0)))
    }
}

/// An extractor that always fails.
struct Failing;

impl SourceExtractor for Failing {
    fn name(&self) -> &str {
        "failing"
    }
    fn next_record(&mut self) -> Result<Option<ImportRecord>, ImportError> {
        Err(ImportError::Extract("boom".into()))
    }
}

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

/// Declare a `Host { name (key), cores: int }` label and commit it, so imports
/// have a schema to bind against and a non-unborn branch to build on.
fn declare_host(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    let types = BTreeMap::from([("cores".to_owned(), PropertyType::Int)]);
    tx.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["name".into()], types, [], []).expect("label"),
    })
    .expect("schema");
    tx.commit("declare Host", &[], None).expect("commit schema");
}

/// Declare a `Service { name (key), require tier, unique ip }` label and
/// commit it — a label carrying both constraint kinds (acetone-9gw).
fn declare_service(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "Service".into(),
        def: LabelDef::new(
            vec!["name".into()],
            BTreeMap::new(),
            ["tier".to_owned()],
            ["ip".to_owned()],
        )
        .expect("label"),
    })
    .expect("schema");
    tx.commit("declare Service", &[], None)
        .expect("commit schema");
}

fn provenance() -> Provenance {
    Provenance {
        source: "hosts.csv".into(),
        extractor: "csv".into(),
        source_hash: "deadbeef".into(),
    }
}

fn opts(branch: Option<&str>) -> ImportOptions {
    ImportOptions {
        branch: branch.map(str::to_owned),
        message: None,
        provenance: provenance(),
        author: None,
        batch_size: None,
    }
}

fn node_record(label: &str, props: &[(&str, Value)]) -> ImportRecord {
    ImportRecord::Node {
        label: label.into(),
        properties: props
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    }
}

#[test]
fn imports_nodes_with_provenance_trailers() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    // Note `cores` arrives as a string, as a CSV row would; it must coerce to
    // the declared Int type.
    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            node_record(
                "Host",
                &[
                    ("name", Value::String("web1".into())),
                    ("cores", Value::String("8".into())),
                ],
            ),
            node_record(
                "Host",
                &[
                    ("name", Value::String("db1".into())),
                    ("cores", Value::String("16".into())),
                ],
            ),
        ],
    };

    let outcome = import(&repo, &mut mock, opts(None)).expect("import");
    match outcome {
        ImportOutcome::Committed { nodes, edges, .. } => {
            assert_eq!(nodes, 2);
            assert_eq!(edges, 0);
        }
        other => panic!("expected Committed, got {other:?}"),
    }

    // The commit carries all three provenance trailers.
    let head = repo.log(None).expect("log");
    let trailers = &head[0].trailers;
    assert!(trailers.contains(&("Acetone-Source".into(), "hosts.csv".into())));
    assert!(trailers.contains(&("Acetone-Extractor".into(), "csv".into())));
    assert!(trailers.contains(&("Acetone-Source-Hash".into(), "deadbeef".into())));

    // The node persisted with a typed key and typed, key-free record.
    let snapshot = repo.workspace_snapshot().expect("snap");
    let web = NodeKey::new("Host", vec![Value::String("web1".into())]).expect("key");
    let rec = snapshot.get_node(&web).expect("get").expect("present");
    assert_eq!(rec.properties().get("cores"), Some(&Value::Int(8)));
    // The key property is not duplicated into the record (Invariant #3).
    assert!(rec.properties().get("name").is_none());
}

#[test]
fn unchanged_reimport_is_a_detected_noop() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    let records = vec![node_record(
        "Host",
        &[
            ("name", Value::String("web1".into())),
            ("cores", Value::Int(8)),
        ],
    )];
    let mut first = Mock {
        name: "csv".into(),
        records: records.clone(),
    };
    let committed = import(&repo, &mut first, opts(None)).expect("first import");
    assert!(matches!(committed, ImportOutcome::Committed { .. }));
    let head_after_first = repo.head_commit().expect("head");

    // Re-importing the identical source makes no change: no commit.
    let mut again = Mock {
        name: "csv".into(),
        records,
    };
    let outcome = import(&repo, &mut again, opts(None)).expect("reimport");
    assert_eq!(outcome, ImportOutcome::NoChange);
    assert_eq!(repo.head_commit().expect("head"), head_after_first);
}

#[test]
fn imports_edges_maintaining_both_edge_maps() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    // Also need the relationship type declared.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::RelType {
            name: "PEERS_WITH".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("rtype"),
        })
        .expect("schema");
        tx.commit("declare rel", &[], None).expect("commit");
    }

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            node_record("Host", &[("name", Value::String("web1".into()))]),
            node_record("Host", &[("name", Value::String("db1".into()))]),
            ImportRecord::Edge {
                rtype: "PEERS_WITH".into(),
                src: EndpointRef {
                    label: "Host".into(),
                    key: vec![Value::String("web1".into())],
                },
                dst: EndpointRef {
                    label: "Host".into(),
                    key: vec![Value::String("db1".into())],
                },
                discriminator: Value::Null,
                properties: BTreeMap::new(),
            },
        ],
    };

    let outcome = import(&repo, &mut mock, opts(None)).expect("import");
    assert!(matches!(
        outcome,
        ImportOutcome::Committed {
            nodes: 2,
            edges: 1,
            ..
        }
    ));

    let snapshot = repo.workspace_snapshot().expect("snap");
    let web = NodeKey::new("Host", vec![Value::String("web1".into())]).expect("k");
    let db = NodeKey::new("Host", vec![Value::String("db1".into())]).expect("k");
    let expected = EdgeKey::new(web, "PEERS_WITH", db, Value::Null).expect("edge");
    let fwd: Vec<EdgeKey> = snapshot
        .edges()
        .expect("edges")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(fwd, vec![expected.clone()]);
    assert_eq!(snapshot.reverse_edge_keys().expect("rev"), vec![expected]);
}

#[test]
fn branch_import_isolates_and_returns_to_original() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    let main_head = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Host",
            &[
                ("name", Value::String("web1".into())),
                ("cores", Value::Int(8)),
            ],
        )],
    };
    let outcome = import(&repo, &mut mock, opts(Some("ingest"))).expect("import");
    assert!(matches!(outcome, ImportOutcome::Committed { .. }));

    // We are back on main, and main is untouched.
    assert_eq!(
        repo.current_branch().expect("branch"),
        Some("refs/heads/main".into())
    );
    assert_eq!(repo.head_commit().expect("head"), main_head);
    assert!(!repo.is_dirty().expect("clean"));

    // The import landed on `ingest`.
    let branches = repo.branches().expect("branches");
    let ingest = branches
        .iter()
        .find(|(n, _)| n == "ingest")
        .expect("ingest branch");
    assert_ne!(Some(ingest.1), main_head);
    let web = NodeKey::new("Host", vec![Value::String("web1".into())]).expect("k");
    let on_ingest = repo
        .snapshot("ingest")
        .expect("snap")
        .get_node(&web)
        .expect("get");
    assert!(on_ingest.is_some());
    // …and not on main.
    assert!(
        repo.workspace_snapshot()
            .expect("snap")
            .get_node(&web)
            .expect("get")
            .is_none()
    );
}

#[test]
fn dirty_workspace_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    // Stage an uncommitted node → dirty workspace.
    {
        let mut tx = repo.begin_write().expect("begin");
        let web = NodeKey::new("Host", vec![Value::String("staged".into())]).expect("k");
        tx.put_node(
            &web,
            &acetone_model::records::NodeRecord::new([], BTreeMap::new()),
        )
        .expect("node");
        tx.save().expect("save");
    }
    assert!(repo.is_dirty().expect("dirty"));

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Host",
            &[("name", Value::String("web1".into()))],
        )],
    };
    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::DirtyWorkspace) => {}
        other => panic!("expected DirtyWorkspace, got {other:?}"),
    }
}

#[test]
fn extractor_failure_leaves_repo_untouched() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    let head = repo.head_commit().expect("head");

    match import(&repo, &mut Failing, opts(None)) {
        Err(GraphError::Import(ImportError::Extract(_))) => {}
        other => panic!("expected extract error, got {other:?}"),
    }
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(!repo.is_dirty().expect("clean"));
}

#[test]
fn invalid_provenance_fails_before_staging_leaving_workspace_clean() {
    // A source string unsuitable as a trailer value (trailing whitespace) is
    // rejected up front, so the workspace is never advanced — even under a
    // branch import, which must not strand the caller (reviewer MAJOR finding).
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    let head = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Host",
            &[("name", Value::String("web1".into()))],
        )],
    };
    let bad = ImportOptions {
        branch: Some("ingest".into()),
        message: None,
        provenance: Provenance {
            source: "hosts.csv ".into(), // trailing space → invalid trailer value
            extractor: "csv".into(),
            source_hash: "deadbeef".into(),
        },
        author: None,
        batch_size: None,
    };
    assert!(import(&repo, &mut mock, bad).is_err());
    // Workspace untouched, still on the original branch.
    assert!(!repo.is_dirty().expect("clean"));
    assert_eq!(repo.head_commit().expect("head"), head);
    assert_eq!(
        repo.current_branch().expect("branch"),
        Some("refs/heads/main".into())
    );
    // No `ingest` branch was created.
    assert!(
        !repo
            .branches()
            .expect("branches")
            .iter()
            .any(|(n, _)| n == "ingest")
    );
}

#[test]
fn branch_equal_to_current_is_rejected() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Host",
            &[("name", Value::String("web1".into()))],
        )],
    };
    match import(&repo, &mut mock, opts(Some("main"))) {
        Err(GraphError::Import(ImportError::Config(_))) => {}
        other => panic!("expected Config error, got {other:?}"),
    }
    // Nothing committed.
    assert!(!repo.is_dirty().expect("clean"));
}

#[test]
fn unknown_label_is_a_mapping_error() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record("Ghost", &[("name", Value::String("x".into()))])],
    };
    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::Import(ImportError::Mapping(_))) => {}
        other => panic!("expected mapping error, got {other:?}"),
    }
}

#[test]
fn importing_an_edge_to_a_missing_node_is_rejected_and_leaves_no_commit() {
    // U6 (pre-0.1 review / ADR-0028): import must not commit a dangling edge.
    // web1 exists; db1 is never imported, so the PEERS_WITH edge has no target.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::RelType {
            name: "PEERS_WITH".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("rtype"),
        })
        .expect("schema");
        tx.commit("declare rel", &[], None).expect("commit");
    }
    let head_before = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            node_record("Host", &[("name", Value::String("web1".into()))]),
            ImportRecord::Edge {
                rtype: "PEERS_WITH".into(),
                src: EndpointRef {
                    label: "Host".into(),
                    key: vec![Value::String("web1".into())],
                },
                dst: EndpointRef {
                    label: "Host".into(),
                    key: vec![Value::String("db1".into())], // never imported
                },
                discriminator: Value::Null,
                properties: BTreeMap::new(),
            },
        ],
    };

    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::DanglingEdge { role, .. }) => assert_eq!(role, "target"),
        other => panic!("expected DanglingEdge (target), got {other:?}"),
    }
    // No commit was written, and the workspace is left clean.
    assert_eq!(repo.head_commit().expect("head"), head_before);
    assert!(!repo.is_dirty().expect("dirty"));
    assert!(
        repo.workspace_snapshot()
            .expect("snap")
            .edges()
            .expect("edges")
            .is_empty(),
        "no edge should have been persisted"
    );
}

// --- declared-constraint enforcement (acetone-9gw) --------------------------

/// A record violating a `--require` constraint must fail the whole import
/// atomically: no commit, clean workspace, nothing persisted — matching what
/// the Cypher write path would have rejected.
#[test]
fn require_violating_import_fails_atomically() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    let head = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            // Valid row first: atomicity means it must not land either.
            node_record(
                "Service",
                &[
                    ("name", Value::String("ok".into())),
                    ("tier", Value::String("gold".into())),
                ],
            ),
            // No `tier` — violates the existence constraint.
            node_record("Service", &[("name", Value::String("bad".into()))]),
        ],
    };
    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::Import(ImportError::Constraints(v))) => {
            let msg = v.to_string();
            assert!(msg.contains("\"Service\""), "names the label: {msg}");
            assert!(msg.contains("\"bad\""), "names the violating key: {msg}");
            assert!(msg.contains("\"tier\""), "names the property: {msg}");
        }
        other => panic!("expected Constraints error, got {other:?}"),
    }
    // Nothing committed, workspace clean, neither node persisted.
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(!repo.is_dirty().expect("dirty"));
    let snap = repo.workspace_snapshot().expect("snap");
    let ok = NodeKey::new("Service", vec![Value::String("ok".into())]).expect("k");
    assert!(
        snap.get_node(&ok).expect("get").is_none(),
        "atomic: valid row must not land"
    );
}

/// Import must enforce UNIQUE against committed data — this proves the bead's
/// "likely applies to UNIQUE (untested)" suspicion either way.
#[test]
fn unique_violating_import_against_base_fails_atomically() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    // Base data: a committed service holding ip 10.0.0.1.
    let mut first = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Service",
            &[
                ("name", Value::String("a".into())),
                ("tier", Value::String("gold".into())),
                ("ip", Value::String("10.0.0.1".into())),
            ],
        )],
    };
    import(&repo, &mut first, opts(None)).expect("seed import");
    let head = repo.head_commit().expect("head");

    // A different node claiming the same unique ip must be rejected.
    let mut second = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Service",
            &[
                ("name", Value::String("b".into())),
                ("tier", Value::String("gold".into())),
                ("ip", Value::String("10.0.0.1".into())),
            ],
        )],
    };
    match import(&repo, &mut second, opts(None)) {
        Err(GraphError::Import(ImportError::Constraints(v))) => {
            let msg = v.to_string();
            assert!(msg.contains("UNIQUE"), "{msg}");
            assert!(msg.contains("\"ip\""), "{msg}");
            assert!(
                msg.contains("\"a\"") && msg.contains("\"b\""),
                "names both nodes: {msg}"
            );
        }
        other => panic!("expected Constraints error, got {other:?}"),
    }
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(!repo.is_dirty().expect("dirty"));
}

/// Two rows inside one import colliding on a UNIQUE property must also fail —
/// the write path's known same-statement gap must not be replicated here.
#[test]
fn unique_collision_between_two_imported_rows_fails() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    let head = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            node_record(
                "Service",
                &[
                    ("name", Value::String("a".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.9".into())),
                ],
            ),
            node_record(
                "Service",
                &[
                    ("name", Value::String("b".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.9".into())),
                ],
            ),
        ],
    };
    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::Import(ImportError::Constraints(_))) => {}
        other => panic!("expected Constraints error, got {other:?}"),
    }
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(!repo.is_dirty().expect("dirty"));
}

/// Valid data against a constrained label still imports; re-importing the
/// same key with the same unique value is not a self-collision.
#[test]
fn valid_import_against_constrained_label_commits() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);

    let records = vec![
        node_record(
            "Service",
            &[
                ("name", Value::String("a".into())),
                ("tier", Value::String("gold".into())),
                ("ip", Value::String("10.0.0.1".into())),
            ],
        ),
        node_record(
            "Service",
            &[
                ("name", Value::String("b".into())),
                ("tier", Value::String("bronze".into())),
                ("ip", Value::String("10.0.0.2".into())),
            ],
        ),
    ];
    let mut mock = Mock {
        name: "csv".into(),
        records: records.clone(),
    };
    let outcome = import(&repo, &mut mock, opts(None)).expect("valid import");
    assert!(matches!(outcome, ImportOutcome::Committed { nodes: 2, .. }));

    // An authoritative re-import of the same rows must not see its own base
    // records as UNIQUE collisions (same key, same value = same node).
    let mut again = Mock {
        name: "csv".into(),
        records,
    };
    let outcome = import(&repo, &mut again, opts(None)).expect("reimport");
    assert_eq!(outcome, ImportOutcome::NoChange);
}

/// Within one import the last record for a key wins (put_node replace
/// semantics), so constraints are judged against the surviving record.
#[test]
fn last_record_for_a_key_wins_before_constraint_checks() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            // First row for `a` is missing tier…
            node_record("Service", &[("name", Value::String("a".into()))]),
            // …but the last row for the same key satisfies the constraint.
            node_record(
                "Service",
                &[
                    ("name", Value::String("a".into())),
                    ("tier", Value::String("gold".into())),
                ],
            ),
        ],
    };
    let outcome = import(&repo, &mut mock, opts(None)).expect("import");
    assert!(matches!(outcome, ImportOutcome::Committed { .. }));
}

/// A violating `--branch` import must restore the caller's branch and leave
/// nothing committed anywhere.
#[test]
fn violating_branch_import_restores_original_branch() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    let head = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Service",
            &[("name", Value::String("bad".into()))],
        )],
    };
    match import(&repo, &mut mock, opts(Some("ingest"))) {
        Err(GraphError::Import(ImportError::Constraints(_))) => {}
        other => panic!("expected Constraints error, got {other:?}"),
    }
    assert_eq!(
        repo.current_branch().expect("branch"),
        Some("refs/heads/main".into())
    );
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(!repo.is_dirty().expect("dirty"));
}

/// Violation reporting is deterministic (sorted by node key) and bounded:
/// many violations render as the first 20 plus a remainder count.
#[test]
fn violation_report_is_sorted_and_bounded() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);

    // 25 violating rows, deliberately supplied in reverse key order.
    let records: Vec<ImportRecord> = (0..25)
        .rev()
        .map(|i| node_record("Service", &[("name", Value::String(format!("svc-{i:02}")))]))
        .collect();
    let mut mock = Mock {
        name: "csv".into(),
        records,
    };
    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::Import(ImportError::Constraints(v))) => {
            let msg = v.to_string();
            // Bounded: first 20 shown, remainder counted.
            assert!(msg.contains("25"), "total count: {msg}");
            assert!(msg.contains("5 more"), "remainder: {msg}");
            assert!(msg.contains("svc-00"), "{msg}");
            assert!(msg.contains("svc-19"), "{msg}");
            assert!(!msg.contains("svc-20"), "bounded at 20: {msg}");
            // Sorted ascending by key despite reverse input order.
            let p0 = msg.find("svc-00").expect("svc-00");
            let p19 = msg.find("svc-19").expect("svc-19");
            assert!(p0 < p19, "sorted: {msg}");
        }
        other => panic!("expected Constraints error, got {other:?}"),
    }
}

/// A pre-existing violation in base data that the import does not touch must
/// not fail the import (fsck's advisory owns pre-fix damage, not import).
#[test]
fn untouched_pre_existing_violation_does_not_fail_import() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    // Plant a pre-fix-style violation via the raw transaction API (what the
    // pre-fix import used to do): a Service with no tier, committed.
    {
        let mut tx = repo.begin_write().expect("begin");
        let bad = NodeKey::new("Service", vec![Value::String("legacy".into())]).expect("k");
        tx.put_node(
            &bad,
            &acetone_model::records::NodeRecord::new([], BTreeMap::new()),
        )
        .expect("node");
        tx.commit("legacy damage", &[], None).expect("commit");
    }

    // An import of unrelated, valid rows still succeeds.
    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Service",
            &[
                ("name", Value::String("new".into())),
                ("tier", Value::String("gold".into())),
            ],
        )],
    };
    let outcome = import(&repo, &mut mock, opts(None)).expect("import");
    assert!(matches!(outcome, ImportOutcome::Committed { .. }));
}

// --- Streaming import (ADR-0062, acetone-6g5.7) -----------------------------

/// An extractor that yields some records and then fails mid-stream.
struct FailAfter {
    records: Vec<ImportRecord>,
}
impl SourceExtractor for FailAfter {
    fn name(&self) -> &str {
        "failing"
    }
    fn next_record(&mut self) -> Result<Option<ImportRecord>, ImportError> {
        if self.records.is_empty() {
            return Err(ImportError::Extract("stream broke mid-source".into()));
        }
        Ok(Some(self.records.remove(0)))
    }
}

fn peers_edge(src: &str, dst: &str) -> ImportRecord {
    ImportRecord::Edge {
        rtype: "PEERS_WITH".into(),
        src: EndpointRef {
            label: "Host".into(),
            key: vec![Value::String(src.into())],
        },
        dst: EndpointRef {
            label: "Host".into(),
            key: vec![Value::String(dst.into())],
        },
        discriminator: Value::Null,
        properties: BTreeMap::new(),
    }
}

fn declare_peers(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::RelType {
        name: "PEERS_WITH".into(),
        def: RelTypeDef::new(None, BTreeMap::new(), []).expect("rtype"),
    })
    .expect("schema");
    tx.commit("declare rel", &[], None).expect("commit");
}

/// The final graph is batch-size independent: the same source at batch
/// sizes 1, 3 and whole-source yields byte-identical committed manifests
/// (history independence, Load-Bearing Invariant #1; derived maps, #5).
#[test]
fn streamed_batches_yield_identical_manifests() {
    let mut source: Vec<ImportRecord> = Vec::new();
    for i in 0..10 {
        source.push(node_record(
            "Host",
            &[
                ("name", Value::String(format!("h{i:02}"))),
                ("cores", Value::Int(i)),
            ],
        ));
    }
    for i in 0..9 {
        source.push(peers_edge(&format!("h{i:02}"), &format!("h{:02}", i + 1)));
    }

    let mut manifests = Vec::new();
    for batch_size in [Some(1), Some(3), Some(10_000)] {
        let dir = tempfile::tempdir().expect("tmp");
        let repo = init_repo(dir.path());
        declare_host(&repo);
        declare_peers(&repo);
        let mut mock = Mock {
            name: "csv".into(),
            records: source.clone(),
        };
        let options = ImportOptions {
            batch_size,
            ..opts(None)
        };
        let outcome = import(&repo, &mut mock, options).expect("import");
        assert!(matches!(
            outcome,
            ImportOutcome::Committed {
                nodes: 10,
                edges: 9,
                ..
            }
        ));
        manifests.push(repo.workspace_manifest().expect("manifest").encode());
    }
    assert_eq!(manifests[0], manifests[1]);
    assert_eq!(manifests[1], manifests[2]);
}

/// An edge whose endpoint only arrives in a later batch violates the
/// extractor contract (nodes before the edges that reference them) and
/// fails at that batch's transaction boundary — leaving the workspace
/// clean, not partially imported.
#[test]
fn forward_reference_across_batches_errors_and_cleans_up() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    declare_peers(&repo);
    let head = repo.head_commit().expect("head");

    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            peers_edge("h00", "h01"),
            node_record("Host", &[("name", Value::String("h00".into()))]),
            node_record("Host", &[("name", Value::String("h01".into()))]),
        ],
    };
    let options = ImportOptions {
        batch_size: Some(1),
        ..opts(None)
    };
    assert!(import(&repo, &mut mock, options).is_err());
    assert!(!repo.is_dirty().expect("dirty"));
    assert_eq!(repo.head_commit().expect("head"), head);
}

/// UNIQUE is enforced across batches: a value already claimed in an
/// earlier batch collides, while a re-imported key un-claims its old
/// value first (replace semantics).
#[test]
fn unique_constraints_hold_across_batches() {
    // Collision across batches.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            node_record(
                "Service",
                &[
                    ("name", Value::String("a".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.1".into())),
                ],
            ),
            node_record(
                "Service",
                &[
                    ("name", Value::String("b".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.1".into())),
                ],
            ),
        ],
    };
    let options = ImportOptions {
        batch_size: Some(1),
        ..opts(None)
    };
    match import(&repo, &mut mock, options) {
        Err(GraphError::Import(ImportError::Constraints(v))) => {
            assert!(v.to_string().contains("ip"), "{v}");
        }
        other => panic!("expected Constraints error, got {other:?}"),
    }
    assert!(!repo.is_dirty().expect("dirty"));

    // Replace un-claims: the same key re-imported with a new value frees
    // the old one for another key in a later batch.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    let mut mock = Mock {
        name: "csv".into(),
        records: vec![
            node_record(
                "Service",
                &[
                    ("name", Value::String("a".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.1".into())),
                ],
            ),
            node_record(
                "Service",
                &[
                    ("name", Value::String("a".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.2".into())),
                ],
            ),
            node_record(
                "Service",
                &[
                    ("name", Value::String("b".into())),
                    ("tier", Value::String("gold".into())),
                    ("ip", Value::String("10.0.0.1".into())),
                ],
            ),
        ],
    };
    let options = ImportOptions {
        batch_size: Some(1),
        ..opts(None)
    };
    let outcome = import(&repo, &mut mock, options).expect("replace un-claims");
    assert!(matches!(outcome, ImportOutcome::Committed { .. }));
}

/// A mid-stream extractor failure after earlier batches have saved resets
/// the workspace: nothing committed, nothing left staged.
#[test]
fn mid_stream_failure_resets_the_workspace() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    let head = repo.head_commit().expect("head");

    let mut failing = FailAfter {
        records: vec![
            node_record("Host", &[("name", Value::String("h00".into()))]),
            node_record("Host", &[("name", Value::String("h01".into()))]),
        ],
    };
    let options = ImportOptions {
        batch_size: Some(1),
        ..opts(None)
    };
    match import(&repo, &mut failing, options) {
        Err(GraphError::Import(ImportError::Extract(_))) => {}
        other => panic!("expected Extract error, got {other:?}"),
    }
    assert!(!repo.is_dirty().expect("dirty"));
    assert_eq!(repo.head_commit().expect("head"), head);
}

/// An import that *shrinks* a pre-existing UNIQUE breach (replacing one
/// colliding node's value) must succeed: the residual breach involves no
/// imported record and stays fsck's business (PR #205 review finding 3).
#[test]
fn import_shrinking_a_pre_existing_breach_succeeds() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_service(&repo);
    // Plant a 3-owner breach on ip="X" via the raw transaction API.
    {
        let mut tx = repo.begin_write().expect("begin");
        for name in ["a", "b", "c"] {
            let key = NodeKey::new("Service", vec![Value::String(name.into())]).expect("k");
            let props = BTreeMap::from([
                ("tier".to_owned(), Value::String("gold".into())),
                ("ip".to_owned(), Value::String("X".into())),
            ]);
            tx.put_node(&key, &acetone_model::records::NodeRecord::new([], props))
                .expect("node");
        }
        tx.commit("legacy damage", &[], None).expect("commit");
    }

    // Re-import `a` with a fresh ip: the breach shrinks to {b, c}, no
    // imported record collides — the import must commit.
    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Service",
            &[
                ("name", Value::String("a".into())),
                ("tier", Value::String("gold".into())),
                ("ip", Value::String("Y".into())),
            ],
        )],
    };
    let outcome = import(&repo, &mut mock, opts(None)).expect("shrinking import");
    assert!(matches!(outcome, ImportOutcome::Committed { .. }));

    // But joining the breach still fails, naming the imported key.
    let mut mock = Mock {
        name: "csv".into(),
        records: vec![node_record(
            "Service",
            &[
                ("name", Value::String("d".into())),
                ("tier", Value::String("gold".into())),
                ("ip", Value::String("X".into())),
            ],
        )],
    };
    match import(&repo, &mut mock, opts(None)) {
        Err(GraphError::Import(ImportError::Constraints(v))) => {
            let msg = v.to_string();
            assert!(msg.contains("\"d\""), "{msg}");
        }
        other => panic!("expected Constraints error, got {other:?}"),
    }
}

/// The library-convenience `VecExtractor` drives a whole import.
#[test]
fn vec_extractor_imports() {
    use acetone_graph::import::VecExtractor;
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    let mut extractor = VecExtractor::new(
        "vec",
        vec![node_record(
            "Host",
            &[("name", Value::String("web1".into()))],
        )],
    );
    let outcome = import(&repo, &mut extractor, opts(None)).expect("import");
    assert!(matches!(outcome, ImportOutcome::Committed { nodes: 1, .. }));
}

/// A failed `--branch` import removes the side branch it created.
#[test]
fn failed_branch_import_removes_created_branch() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);
    let head = repo.head_commit().expect("head");

    let mut failing = FailAfter {
        records: vec![node_record(
            "Host",
            &[("name", Value::String("h00".into()))],
        )],
    };
    let options = ImportOptions {
        branch: Some("ingest".into()),
        batch_size: Some(1),
        ..opts(None)
    };
    assert!(import(&repo, &mut failing, options).is_err());
    assert_eq!(
        repo.current_branch().expect("branch"),
        Some("refs/heads/main".into())
    );
    assert!(!repo.is_dirty().expect("dirty"));
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(
        !repo
            .branches()
            .expect("branches")
            .iter()
            .any(|(name, _)| name == "ingest"),
        "side branch must be cleaned up"
    );
}
