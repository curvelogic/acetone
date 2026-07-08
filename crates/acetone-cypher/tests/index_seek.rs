//! End-to-end tests for IndexSeek execution (spec §5.3, acetone-6g5.3.2): a
//! declared index accelerates a pinned equality and returns exactly the rows a
//! label scan would, and the executor actually takes the seek path.

use std::cell::Cell;
use std::collections::BTreeMap;

use acetone_cypher::ast::Direction;
use acetone_cypher::bind::binder::BindMode;
use acetone_cypher::exec::source::GraphSource;
use acetone_cypher::exec::value::{EntityId, NodeValue, RelValue};
use acetone_cypher::exec::{
    GraphSnapshot, NoProcedures, SingleVersion, Value, catalogue_from_schema,
    execute_versioned_with,
};
use acetone_model::Value as ModelValue;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{IndexDef, LabelDef, PropertyType, SchemaEntry};

/// A GraphSource wrapper that counts index seeks vs label scans, to prove
/// which path the executor took.
struct Counting<'a> {
    inner: &'a GraphSnapshot,
    seeks: Cell<usize>,
    scans: Cell<usize>,
}

impl<'a> Counting<'a> {
    fn new(inner: &'a GraphSnapshot) -> Self {
        Counting {
            inner,
            seeks: Cell::new(0),
            scans: Cell::new(0),
        }
    }
}

impl GraphSource for Counting<'_> {
    fn all_nodes(&self) -> Vec<NodeValue> {
        self.inner.all_nodes()
    }
    fn nodes_by_labels(&self, labels: &[String]) -> Vec<NodeValue> {
        self.scans.set(self.scans.get() + 1);
        self.inner.nodes_by_labels(labels)
    }
    fn nodes_by_index(&self, name: &str, value: &Value) -> Option<Vec<NodeValue>> {
        let result = self.inner.nodes_by_index(name, value);
        if result.is_some() {
            self.seeks.set(self.seeks.get() + 1);
        }
        result
    }
    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        self.inner.expand(node, direction, types)
    }
    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.inner.node(id)
    }
}

fn host(name: &str) -> NodeKey {
    NodeKey::new("Host", vec![ModelValue::String(name.to_owned())]).expect("key")
}

fn host_record(os: &str) -> NodeRecord {
    NodeRecord::new(
        [],
        BTreeMap::from([("os".to_owned(), ModelValue::String(os.to_owned()))]),
    )
}

/// Three hosts (two linux, one windows) and a Software node under a different
/// label.
fn records() -> Vec<(NodeKey, NodeRecord)> {
    vec![
        (host("h1"), host_record("linux")),
        (host("h2"), host_record("windows")),
        (host("h3"), host_record("linux")),
        (
            NodeKey::new("Software", vec![ModelValue::String("nginx".into())]).expect("k"),
            NodeRecord::new([], BTreeMap::new()),
        ),
    ]
}

fn host_label() -> SchemaEntry {
    SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(
            vec!["hostname".into()],
            BTreeMap::from([("os".to_owned(), PropertyType::String)]),
            [],
            [],
        )
        .expect("label"),
    }
}

fn os_index() -> SchemaEntry {
    SchemaEntry::Index {
        name: "host_os".into(),
        def: IndexDef::new("Host", "os").expect("idx"),
    }
}

const QUERY: &str = "MATCH (h:Host {os: 'linux'}) RETURN h.hostname";

/// Run QUERY against a snapshot built from `schema`, returning the hostname
/// column, sorted.
fn run(schema: &[SchemaEntry], graph: &dyn GraphSource) -> Vec<String> {
    let ast = acetone_cypher::parse(QUERY).expect("parse");
    let catalogue = catalogue_from_schema(schema.to_vec());
    let bound = acetone_cypher::bind::binder::bind(QUERY, &ast, &catalogue, BindMode::Strict)
        .expect("bind");
    let resolver = SingleVersion::new(graph);
    let result = execute_versioned_with(&bound, &resolver, &NoProcedures, &BTreeMap::new())
        .expect("execute");
    let mut names: Vec<String> = result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected string hostname, got {other:?}"),
        })
        .collect();
    names.sort();
    names
}

#[test]
fn index_seek_returns_the_same_rows_as_a_scan() {
    let recs = records();
    let edges: Vec<(EdgeKey, EdgeRecord)> = Vec::new();

    // With the index declared: the binder emits an IndexSeek hint and the
    // adapter has the value map.
    let with_schema = vec![host_label(), os_index()];
    let with_adapter = GraphSnapshot::from_records_with_schema(&recs, &edges, &with_schema);
    let counting = Counting::new(&with_adapter);
    let indexed = run(&with_schema, &counting);

    // Without the index: a plain label scan.
    let without_schema = vec![host_label()];
    let scan_adapter = GraphSnapshot::from_records_with_schema(&recs, &edges, &without_schema);
    let scanned = run(&without_schema, &scan_adapter);

    // Parity: identical, correct results (both linux hosts).
    assert_eq!(indexed, vec!["h1".to_string(), "h3".to_string()]);
    assert_eq!(indexed, scanned);

    // The indexed run actually took the seek path, not a label scan, to anchor.
    assert!(counting.seeks.get() >= 1, "IndexSeek was not used");
    assert_eq!(
        counting.scans.get(),
        0,
        "a label scan was used despite the index"
    );
}

/// Run `cypher` against a snapshot built from `schema`, returning the row
/// count. Used to compare an indexed run against an unindexed (scan) run.
fn count(cypher: &str, schema: &[SchemaEntry], graph: &dyn GraphSource) -> usize {
    let ast = acetone_cypher::parse(cypher).expect("parse");
    let catalogue = catalogue_from_schema(schema.to_vec());
    let bound = acetone_cypher::bind::binder::bind(cypher, &ast, &catalogue, BindMode::Strict)
        .expect("bind");
    let resolver = SingleVersion::new(graph);
    execute_versioned_with(&bound, &resolver, &NoProcedures, &BTreeMap::new())
        .expect("execute")
        .rows
        .len()
}

#[test]
fn numeric_cross_type_query_matches_a_scan_end_to_end() {
    // A node whose indexed numeric property is stored as a Float, queried with
    // an Int literal (and vice versa), must return the same rows via IndexSeek
    // as via a scan — openCypher `1 = 1.0`.
    let schema_with = vec![
        SchemaEntry::Label {
            name: "M".into(),
            def: LabelDef::new(
                vec!["id".into()],
                BTreeMap::from([("v".to_owned(), PropertyType::Int)]),
                [],
                [],
            )
            .expect("label"),
        },
        SchemaEntry::Index {
            name: "m_v".into(),
            def: IndexDef::new("M", "v").expect("idx"),
        },
    ];
    let schema_without = vec![schema_with[0].clone()];
    let edges: Vec<(EdgeKey, EdgeRecord)> = Vec::new();

    // Stored as Float(1.0), queried with Int literal 1.
    let float_stored = vec![(
        NodeKey::new("M", vec![ModelValue::String("a".into())]).expect("k"),
        NodeRecord::new(
            [],
            BTreeMap::from([("v".to_owned(), ModelValue::Float(1.0))]),
        ),
    )];
    let with = GraphSnapshot::from_records_with_schema(&float_stored, &edges, &schema_with);
    let without = GraphSnapshot::from_records_with_schema(&float_stored, &edges, &schema_without);
    let q = "MATCH (m:M {v: 1}) RETURN m.id";
    assert_eq!(
        count(q, &schema_with, &with),
        1,
        "IndexSeek dropped a cross-type match"
    );
    assert_eq!(
        count(q, &schema_without, &without),
        1,
        "scan baseline wrong"
    );

    // Stored as Int(2), queried with Float literal 2.0.
    let int_stored = vec![(
        NodeKey::new("M", vec![ModelValue::String("b".into())]).expect("k"),
        NodeRecord::new([], BTreeMap::from([("v".to_owned(), ModelValue::Int(2))])),
    )];
    let with2 = GraphSnapshot::from_records_with_schema(&int_stored, &edges, &schema_with);
    let without2 = GraphSnapshot::from_records_with_schema(&int_stored, &edges, &schema_without);
    let q2 = "MATCH (m:M {v: 2.0}) RETURN m.id";
    assert_eq!(
        count(q2, &schema_with, &with2),
        1,
        "IndexSeek dropped a cross-type match"
    );
    assert_eq!(
        count(q2, &schema_without, &without2),
        1,
        "scan baseline wrong"
    );

    // f64 precision boundary: a large integer whose float-rounding equals the
    // pin (2^53+1 rounds to 2^53) must still match via the scan fallback for a
    // large integral float pin — the seek must not silently under-select.
    let big_stored = vec![(
        NodeKey::new("M", vec![ModelValue::String("c".into())]).expect("k"),
        NodeRecord::new(
            [],
            BTreeMap::from([("v".to_owned(), ModelValue::Int(9_007_199_254_740_993))]),
        ),
    )];
    let with3 = GraphSnapshot::from_records_with_schema(&big_stored, &edges, &schema_with);
    let without3 = GraphSnapshot::from_records_with_schema(&big_stored, &edges, &schema_without);
    let q3 = "MATCH (m:M {v: 9007199254740992.0}) RETURN m.id";
    assert_eq!(
        count(q3, &schema_with, &with3),
        count(q3, &schema_without, &without3),
        "seek disagrees with scan at the f64 integer-precision boundary"
    );
    assert_eq!(
        count(q3, &schema_without, &without3),
        1,
        "scan baseline wrong"
    );
}

#[test]
fn numeric_index_seek_matches_across_int_and_float_like_a_scan() {
    // An index on a numeric property must honour openCypher's cross-type
    // equality (3 = 3.0): a seek pinned with one numeric type must still find a
    // node stored under the other, exactly as a scan would.
    let recs = vec![
        (
            NodeKey::new("M", vec![ModelValue::String("as_int".into())]).expect("k"),
            NodeRecord::new([], BTreeMap::from([("v".to_owned(), ModelValue::Int(3))])),
        ),
        (
            NodeKey::new("M", vec![ModelValue::String("as_float".into())]).expect("k"),
            NodeRecord::new(
                [],
                BTreeMap::from([("v".to_owned(), ModelValue::Float(3.0))]),
            ),
        ),
        (
            NodeKey::new("M", vec![ModelValue::String("other".into())]).expect("k"),
            NodeRecord::new([], BTreeMap::from([("v".to_owned(), ModelValue::Int(4))])),
        ),
    ];
    let edges: Vec<(EdgeKey, EdgeRecord)> = Vec::new();
    let schema = vec![
        SchemaEntry::Label {
            name: "M".into(),
            def: LabelDef::new(
                vec!["id".into()],
                BTreeMap::from([("v".to_owned(), PropertyType::Int)]),
                [],
                [],
            )
            .expect("label"),
        },
        SchemaEntry::Index {
            name: "m_v".into(),
            def: IndexDef::new("M", "v").expect("idx"),
        },
    ];
    let adapter = GraphSnapshot::from_records_with_schema(&recs, &edges, &schema);

    // Seek with an Int: finds both the Int-stored and Float-stored node.
    let by_int = adapter
        .nodes_by_index("m_v", &Value::Int(3))
        .expect("present");
    assert_eq!(by_int.len(), 2, "Int seek missed the Float-stored node");
    // Seek with a Float: same two nodes.
    let by_float = adapter
        .nodes_by_index("m_v", &Value::Float(3.0))
        .expect("present");
    assert_eq!(by_float.len(), 2, "Float seek missed the Int-stored node");
    // A non-integer float matches neither.
    assert_eq!(
        adapter
            .nodes_by_index("m_v", &Value::Float(3.5))
            .expect("present")
            .len(),
        0
    );
}

#[test]
fn list_valued_seek_falls_back_to_a_scan() {
    // A list pin cannot be served by exact-byte buckets under element-wise
    // Int/Float equality, so nodes_by_index returns None → the executor scans.
    let recs = vec![(
        NodeKey::new("M", vec![ModelValue::String("a".into())]).expect("k"),
        NodeRecord::new(
            [],
            BTreeMap::from([(
                "tags".to_owned(),
                ModelValue::List(vec![ModelValue::Float(1.0)]),
            )]),
        ),
    )];
    let edges: Vec<(EdgeKey, EdgeRecord)> = Vec::new();
    let schema = vec![
        SchemaEntry::Label {
            name: "M".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        },
        SchemaEntry::Index {
            name: "m_tags".into(),
            def: IndexDef::new("M", "tags").expect("idx"),
        },
    ];
    let adapter = GraphSnapshot::from_records_with_schema(&recs, &edges, &schema);
    assert!(
        adapter
            .nodes_by_index("m_tags", &Value::List(vec![Value::Int(1)]))
            .is_none(),
        "a list pin must fall back to a scan, not risk a subset"
    );
}

#[test]
fn nodes_by_index_selects_correctly_and_is_null_blind() {
    let recs = records();
    let edges: Vec<(EdgeKey, EdgeRecord)> = Vec::new();
    let schema = vec![host_label(), os_index()];
    let adapter = GraphSnapshot::from_records_with_schema(&recs, &edges, &schema);

    // Two linux hosts.
    let linux = adapter
        .nodes_by_index("host_os", &Value::String("linux".into()))
        .expect("index present");
    let mut names: Vec<String> = linux
        .iter()
        .map(|n| match n.properties.get("hostname") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("no hostname: {other:?}"),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["h1".to_string(), "h3".to_string()]);

    // One windows host.
    assert_eq!(
        adapter
            .nodes_by_index("host_os", &Value::String("windows".into()))
            .expect("present")
            .len(),
        1
    );
    // A value with no matching node: empty, not None.
    assert_eq!(
        adapter
            .nodes_by_index("host_os", &Value::String("bsd".into()))
            .expect("present")
            .len(),
        0
    );
    // Null is null-blind: selects nothing.
    assert!(
        adapter
            .nodes_by_index("host_os", &Value::Null)
            .expect("present")
            .is_empty()
    );
    // An undeclared index → None, so the executor falls back to a scan.
    assert!(
        adapter
            .nodes_by_index("nonexistent", &Value::String("x".into()))
            .is_none()
    );
}
