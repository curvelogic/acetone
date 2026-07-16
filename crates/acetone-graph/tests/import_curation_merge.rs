//! Import vs human curation (acetone-6g5.11, ADR-0042): the recommended
//! workflow is **import-to-branch + merge** — the scheduled importer commits
//! authoritative-replace records to a one-directional source-mirror branch
//! (`ingest`), and a human merges it into their curated branch on their own
//! cadence. With cell-wise merge (ADR-0035) a human annotation on a *different*
//! property from the ones the source carries is preserved automatically (no
//! conflict); a genuine clash — both the source and the human changed the *same*
//! property — surfaces as conflict-as-data.
//!
//! These tests are the executable evidence for that decision: they drive the
//! whole flow through the shipped `import(--branch)` + `merge` machinery.

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::import::{
    ImportOptions, ImportOutcome, ImportRecord, Provenance, SourceExtractor,
};
use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository, ResolveSide};
use acetone_graph::{import, import::ImportError};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::schema::{LabelDef, PropertyType, SchemaEntry};

/// A canned extractor returning a fixed record list (one import run).
struct Mock {
    records: Vec<ImportRecord>,
}
impl SourceExtractor for Mock {
    fn name(&self) -> &str {
        "csv"
    }
    fn extract(&mut self) -> Result<Vec<ImportRecord>, ImportError> {
        Ok(self.records.clone())
    }
}

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

/// `Host { name (key), os_version, ip }` — the source owns os_version and ip.
fn declare_host(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    let types = BTreeMap::from([
        ("os_version".to_owned(), PropertyType::String),
        ("ip".to_owned(), PropertyType::String),
    ]);
    tx.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["name".into()], types, [], []).expect("label"),
    })
    .expect("schema");
    tx.commit("declare Host", &[], None).expect("commit schema");
}

fn provenance() -> Provenance {
    Provenance {
        source: "hosts.csv".into(),
        extractor: "csv".into(),
        source_hash: "deadbeef".into(),
    }
}

fn import_opts(branch: Option<&str>) -> ImportOptions {
    ImportOptions {
        branch: branch.map(str::to_owned),
        message: None,
        provenance: provenance(),
        author: None,
    }
}

fn host(name: &str, props: &[(&str, &str)]) -> ImportRecord {
    let mut properties = BTreeMap::from([("name".to_owned(), Value::String(name.into()))]);
    for (k, v) in props {
        properties.insert((*k).to_owned(), Value::String((*v).into()));
    }
    ImportRecord::Node {
        label: "Host".into(),
        properties,
    }
}

fn run_import(repo: &Repository, records: Vec<ImportRecord>, branch: Option<&str>) {
    let mut mock = Mock { records };
    match import(repo, &mut mock, import_opts(branch)).expect("import") {
        ImportOutcome::Committed { .. } | ImportOutcome::NoChange => {}
    }
}

fn prop(repo: &Repository, name: &str, key: &str) -> Option<Value> {
    let snap = repo.workspace_snapshot().expect("snap");
    let k = NodeKey::new("Host", vec![Value::String(key.into())]).expect("key");
    snap.get_node(&k)
        .expect("get")
        .and_then(|r| r.properties().get(name).cloned())
}

fn v(text: &str) -> Value {
    Value::String(text.into())
}

#[test]
fn re_import_preserves_a_human_annotation_on_a_different_property() {
    // The flagship acetone-6g5.11 scenario. Import establishes web1; a human
    // annotates web1.owner on the curated branch; the source re-imports (a new
    // os_version) onto the one-directional `ingest` mirror; merging `ingest`
    // preserves the human's owner *and* takes the source's os_version — with no
    // conflict, thanks to cell-wise merge.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    // Import #1 lands on the `ingest` mirror (a pure source branch).
    run_import(
        &repo,
        vec![host("web1", &[("os_version", "12"), ("ip", "10.0.0.1")])],
        Some("ingest"),
    );
    // Bring it into the curated branch (main) — a fast-forward the first time.
    match repo.merge("ingest", "merge ingest").expect("merge") {
        MergeOutcome::FastForward(_) | MergeOutcome::Merged(_) => {}
        other => panic!("first merge should land cleanly, got {other:?}"),
    }
    assert_eq!(prop(&repo, "os_version", "web1"), Some(v("12")));

    // A human curates: adds an owner the source knows nothing about.
    let mut tx = repo.begin_write().expect("begin");
    let key = NodeKey::new("Host", vec![v("web1")]).expect("key");
    let mut rec = repo
        .workspace_snapshot()
        .expect("snap")
        .get_node(&key)
        .expect("get")
        .expect("web1 present");
    let mut props = rec.properties().clone();
    props.insert("owner".into(), v("greg"));
    rec = acetone_model::records::NodeRecord::new(rec.secondary_labels().to_vec(), props);
    tx.put_node(&key, &rec).expect("annotate");
    tx.commit("human sets owner", &[], None).expect("commit");

    // Import #2: the source re-imports web1 with a newer os_version (and, being
    // authoritative-replace, carries no owner) onto the `ingest` mirror.
    run_import(
        &repo,
        vec![host("web1", &[("os_version", "13"), ("ip", "10.0.0.1")])],
        Some("ingest"),
    );

    // Merge the mirror into the curated branch: the human's owner survives (a
    // different property, auto-merged), the source's os_version is taken.
    match repo.merge("ingest", "merge ingest").expect("merge") {
        MergeOutcome::Merged(_) | MergeOutcome::FastForward(_) => {}
        other => panic!("re-import merge should auto-merge, not conflict: {other:?}"),
    }
    assert_eq!(
        prop(&repo, "owner", "web1"),
        Some(v("greg")),
        "the human annotation must survive the re-import"
    );
    assert_eq!(
        prop(&repo, "os_version", "web1"),
        Some(v("13")),
        "the source's update must land"
    );
}

#[test]
fn the_annotation_survives_repeated_re_import_cycles() {
    // The scheduled-importer reality: many import→merge cycles. Because the
    // `ingest` mirror never carries `owner` (it is one-directional — the human
    // only ever merges ingest→main, never main→ingest), the merge base of every
    // cycle is an owner-free import commit, so `owner` is always a one-sided add
    // on the curated side and is preserved indefinitely while os_version tracks
    // the source.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    run_import(
        &repo,
        vec![host("web1", &[("os_version", "1"), ("ip", "10.0.0.1")])],
        Some("ingest"),
    );
    repo.merge("ingest", "merge ingest").expect("merge");

    // Annotate once.
    let key = NodeKey::new("Host", vec![v("web1")]).expect("key");
    let mut tx = repo.begin_write().expect("begin");
    let rec = repo
        .workspace_snapshot()
        .expect("snap")
        .get_node(&key)
        .expect("get")
        .expect("present");
    let mut props = rec.properties().clone();
    props.insert("owner".into(), v("greg"));
    tx.put_node(
        &key,
        &acetone_model::records::NodeRecord::new(rec.secondary_labels().to_vec(), props),
    )
    .expect("annotate");
    tx.commit("human sets owner", &[], None).expect("commit");

    // Five more scheduled re-imports, each bumping os_version.
    for cycle in 2..=6 {
        run_import(
            &repo,
            vec![host(
                "web1",
                &[("os_version", &cycle.to_string()), ("ip", "10.0.0.1")],
            )],
            Some("ingest"),
        );
        match repo.merge("ingest", "merge ingest").expect("merge") {
            MergeOutcome::Merged(_) | MergeOutcome::FastForward(_) => {}
            other => panic!("cycle {cycle} should auto-merge, got {other:?}"),
        }
        assert_eq!(
            prop(&repo, "owner", "web1"),
            Some(v("greg")),
            "owner must survive cycle {cycle}"
        );
        assert_eq!(
            prop(&repo, "os_version", "web1"),
            Some(v(&cycle.to_string())),
            "os_version must track the source at cycle {cycle}"
        );
    }
}

#[test]
fn a_clash_on_the_same_property_surfaces_as_conflict_as_data() {
    // When the human and the source both change the *same* property, the merge
    // does not silently pick one — it surfaces a conflict for the human to
    // resolve (conflict-as-data). Here both set os_version.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host(&repo);

    run_import(
        &repo,
        vec![host("web1", &[("os_version", "12"), ("ip", "10.0.0.1")])],
        Some("ingest"),
    );
    repo.merge("ingest", "merge ingest").expect("merge");

    // Human overrides os_version on the curated branch.
    run_import(
        &repo,
        vec![host("web1", &[("os_version", "human"), ("ip", "10.0.0.1")])],
        None, // straight onto main
    );

    // Source re-imports a different os_version onto the mirror.
    run_import(
        &repo,
        vec![host("web1", &[("os_version", "13"), ("ip", "10.0.0.1")])],
        Some("ingest"),
    );

    match repo.merge("ingest", "merge ingest").expect("merge") {
        MergeOutcome::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1, "exactly os_version conflicts");
        }
        other => panic!("a same-property clash must conflict, got {other:?}"),
    }
    // The human resolves in their favour and completes.
    repo.resolve_all(ResolveSide::Ours).expect("resolve");
    let tx = repo.begin_write().expect("begin");
    tx.commit("merge ingest", &[], None).expect("commit");
    assert_eq!(prop(&repo, "os_version", "web1"), Some(v("human")));
    assert!(repo.merge_head().expect("merge head").is_none());
}
