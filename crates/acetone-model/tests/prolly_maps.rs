//! Acceptance tests for acetone-63m.4: the spec §3.3 maps and the
//! manifest round-trip **through the prolly layer** — encoded rows are
//! loaded into real prolly trees over a content-addressed store, read
//! back by point get and range scan, and decoded to the original values;
//! manifest hashing is deterministic (identical manifests → identical
//! chunk addresses, regardless of how the map contents were built).

use acetone_model::Value;
use acetone_model::graph_keys::{
    EdgeKey, IndexEntry, NodeKey, edge_endpoint_prefix, node_label_prefix, prefix_successor,
};
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{IndexDef, LabelDef, PropertyType, RelTypeDef, SchemaEntry};
use acetone_prolly::{BatchOp, ChunkParams, Root, apply_batch, bulk_load, empty, get, scan};
use acetone_store::{Bytes, ChunkStore, Hash, StoreError};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Bound;

// ---------------------------------------------------------------------------
// Minimal in-memory ChunkStore (git-blob addressed, as GitStore would be)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemStore {
    chunks: RefCell<BTreeMap<Hash, Bytes>>,
}

impl ChunkStore for MemStore {
    fn put(&self, data: &[u8]) -> Result<Hash, StoreError> {
        let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, data)
            .expect("SHA-1 blob hashing is infallible for in-memory data");
        let hash = Hash::from_bytes(oid.as_bytes()).expect("git digest is a valid hash width");
        self.chunks
            .borrow_mut()
            .entry(hash)
            .or_insert_with(|| Bytes::from(data.to_vec()));
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Option<Bytes>, StoreError> {
        Ok(self.chunks.borrow().get(hash).cloned())
    }

    fn max_chunk_size(&self) -> u64 {
        64 * 1024 * 1024
    }
}

// ---------------------------------------------------------------------------
// A small but representative graph
// ---------------------------------------------------------------------------

fn params() -> ChunkParams {
    ChunkParams::new(1024, 12, 65536).expect("valid params")
}

fn host(name: &str) -> NodeKey {
    NodeKey::new("Host", vec![Value::String(name.to_owned())]).expect("valid")
}

fn service(name: &str) -> NodeKey {
    NodeKey::new("Service", vec![Value::String(name.to_owned())]).expect("valid")
}

fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

struct Fixture {
    schema_rows: Vec<(Vec<u8>, Vec<u8>)>,
    node_rows: Vec<(Vec<u8>, Vec<u8>)>,
    fwd_rows: Vec<(Vec<u8>, Vec<u8>)>,
    rev_rows: Vec<(Vec<u8>, Vec<u8>)>,
    idx_rows: Vec<(Vec<u8>, Vec<u8>)>,
    nodes: Vec<(NodeKey, NodeRecord)>,
    edges: Vec<(EdgeKey, EdgeRecord)>,
    schema: Vec<SchemaEntry>,
}

fn fixture() -> Fixture {
    let schema = vec![
        SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(
                vec!["name".into()],
                [("os".to_owned(), PropertyType::String)].into(),
                ["os".to_owned()],
                [],
            )
            .expect("valid"),
        },
        SchemaEntry::Label {
            name: "Service".into(),
            def: LabelDef::new(vec!["name".into()], BTreeMap::new(), [], []).expect("valid"),
        },
        SchemaEntry::RelType {
            name: "DEPENDS_ON".into(),
            def: RelTypeDef::new(Some("port".into()), BTreeMap::new(), []).expect("valid"),
        },
        SchemaEntry::Index {
            name: "host_os".into(),
            def: IndexDef::new("Host", vec!["os".into()]).expect("valid"),
        },
    ];
    let nodes = vec![
        (
            host("web1"),
            NodeRecord::new(
                ["Edge".to_owned()],
                props(&[
                    ("os", Value::String("linux".into())),
                    ("cores", Value::Int(8)),
                ]),
            ),
        ),
        (
            host("web2"),
            NodeRecord::new([], props(&[("os", Value::String("bsd".into()))])),
        ),
        (
            service("db"),
            NodeRecord::new([], props(&[("tier", Value::Int(0))])),
        ),
    ];
    let edges = vec![
        (
            EdgeKey::new(host("web1"), "DEPENDS_ON", service("db"), Value::Int(5432))
                .expect("valid"),
            EdgeRecord::new(props(&[("weight", Value::Float(0.9))])),
        ),
        (
            EdgeKey::new(host("web2"), "DEPENDS_ON", service("db"), Value::Null).expect("valid"),
            EdgeRecord::new(BTreeMap::new()),
        ),
    ];
    let schema_rows = schema
        .iter()
        .map(|e| (e.map_key(), e.encode_value()))
        .collect();
    let node_rows = nodes
        .iter()
        .map(|(k, r)| (k.encode().expect("encode"), r.encode().expect("encode")))
        .collect();
    let fwd_rows: Vec<(Vec<u8>, Vec<u8>)> = edges
        .iter()
        .map(|(k, r)| (k.encode_fwd().expect("encode"), r.encode().expect("encode")))
        .collect();
    let rev_rows: Vec<(Vec<u8>, Vec<u8>)> = edges
        .iter()
        .map(|(k, _)| (k.encode_rev().expect("encode"), Vec::new()))
        .collect();
    let idx_rows = nodes
        .iter()
        .filter(|(k, _)| k.label() == "Host")
        .filter_map(|(k, r)| {
            r.properties().get("os").map(|os| {
                let entry = IndexEntry::new("Host", vec!["os".into()], vec![os.clone()], k.clone())
                    .expect("valid");
                (entry.encode().expect("encode"), Vec::new())
            })
        })
        .collect();
    Fixture {
        schema_rows,
        node_rows,
        fwd_rows,
        rev_rows,
        idx_rows,
        nodes,
        edges,
        schema,
    }
}

fn load(store: &MemStore, rows: &[(Vec<u8>, Vec<u8>)]) -> Root {
    bulk_load(store, params(), rows.to_vec()).expect("bulk load")
}

fn build_manifest(store: &MemStore, f: &Fixture) -> Manifest {
    Manifest {
        chunk_params: params(),
        schema: MapRoot::from_root(&load(store, &f.schema_rows)),
        nodes: MapRoot::from_root(&load(store, &f.node_rows)),
        edges_fwd: MapRoot::from_root(&load(store, &f.fwd_rows)),
        edges_rev: MapRoot::from_root(&load(store, &f.rev_rows)),
        indexes: [(
            "host_os".to_owned(),
            MapRoot::from_root(&load(store, &f.idx_rows)),
        )]
        .into(),
        conflicts: None,
    }
}

fn scan_all(store: &MemStore, root: &Root) -> Vec<(Vec<u8>, Vec<u8>)> {
    scan(store, root, ..)
        .expect("scan")
        .map(|item| item.expect("scan item"))
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

fn prefix_scan(store: &MemStore, root: &Root, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let lower = Bound::Included(prefix);
    let upper = prefix_successor(prefix);
    let items: Vec<_> = match &upper {
        Some(succ) => scan(store, root, (lower, Bound::Excluded(succ.as_slice()))),
        None => scan(store, root, (lower, Bound::Unbounded)),
    }
    .expect("scan")
    .map(|item| item.expect("scan item"))
    .map(|(k, v)| (k.to_vec(), v.to_vec()))
    .collect();
    items
}

// ---------------------------------------------------------------------------
// Acceptance: maps round-trip through the prolly layer
// ---------------------------------------------------------------------------

#[test]
fn all_maps_round_trip_through_prolly_trees() {
    let store = MemStore::default();
    let f = fixture();
    let manifest = build_manifest(&store, &f);

    // Manifest itself round-trips through the store as a chunk.
    let manifest_hash = store.put(&manifest.encode()).expect("put");
    let manifest_bytes = store.get(&manifest_hash).expect("get").expect("present");
    let manifest_back = Manifest::decode(&manifest_bytes).expect("decode");
    assert_eq!(manifest_back, manifest);

    // Schema map: every row decodes back to its entry.
    let schema_root = manifest_back
        .schema
        .to_root(manifest_back.chunk_params)
        .expect("root");
    let rows = scan_all(&store, &schema_root);
    assert_eq!(rows.len(), f.schema.len());
    let decoded: Vec<SchemaEntry> = rows
        .iter()
        .map(|(k, v)| SchemaEntry::decode(k, v).expect("decode"))
        .collect();
    for entry in &f.schema {
        assert!(decoded.contains(entry), "missing schema entry {entry:?}");
    }

    // Nodes map: point gets by encoded key, then decoded equality.
    let nodes_root = manifest_back
        .nodes
        .to_root(manifest_back.chunk_params)
        .expect("root");
    for (key, record) in &f.nodes {
        let bytes = get(&store, &nodes_root, &key.encode().expect("encode"))
            .expect("get")
            .expect("present");
        assert_eq!(&NodeRecord::decode(&bytes).expect("decode"), record);
    }

    // Edge maps: forward rows decode to (key, record); reverse rows are
    // empty-valued and decode to the same edge keys.
    let fwd_root = manifest_back
        .edges_fwd
        .to_root(manifest_back.chunk_params)
        .expect("root");
    let fwd = scan_all(&store, &fwd_root);
    assert_eq!(fwd.len(), f.edges.len());
    for (k, v) in &fwd {
        let key = EdgeKey::decode_fwd(k).expect("decode");
        let record = EdgeRecord::decode(v).expect("decode");
        assert!(f.edges.contains(&(key, record)), "unexpected fwd row");
    }
    let rev_root = manifest_back
        .edges_rev
        .to_root(manifest_back.chunk_params)
        .expect("root");
    let rev = scan_all(&store, &rev_root);
    assert_eq!(rev.len(), f.edges.len());
    for (k, v) in &rev {
        assert!(v.is_empty(), "edges_rev values must be empty");
        let key = EdgeKey::decode_rev(k).expect("decode");
        assert!(
            f.edges.iter().any(|(ek, _)| *ek == key),
            "rev row without a matching edge"
        );
    }

    // Index map: entries decode and point at real nodes.
    let idx_root = manifest_back.indexes["host_os"]
        .to_root(manifest_back.chunk_params)
        .expect("root");
    let idx = scan_all(&store, &idx_root);
    assert_eq!(idx.len(), 2);
    for (k, v) in &idx {
        assert!(v.is_empty(), "index values must be empty");
        let entry = IndexEntry::decode(k).expect("decode");
        assert!(f.nodes.iter().any(|(nk, _)| nk == entry.node()));
    }
}

#[test]
fn label_and_endpoint_prefix_scans_select_exactly() {
    let store = MemStore::default();
    let f = fixture();
    let manifest = build_manifest(&store, &f);
    let cp = manifest.chunk_params;

    // "All nodes with label Host" — exactly the two hosts, in key order.
    let nodes_root = manifest.nodes.to_root(cp).expect("root");
    let hosts = prefix_scan(&store, &nodes_root, &node_label_prefix("Host"));
    assert_eq!(hosts.len(), 2);
    for (k, _) in &hosts {
        assert_eq!(NodeKey::decode(k).expect("decode").label(), "Host");
    }
    // A label that shares a string prefix must not leak in.
    assert!(prefix_scan(&store, &nodes_root, &node_label_prefix("Ho")).is_empty());

    // "All edges out of web1" on edges_fwd.
    let fwd_root = manifest.edges_fwd.to_root(cp).expect("root");
    let out = prefix_scan(
        &store,
        &fwd_root,
        &edge_endpoint_prefix(&host("web1")).expect("prefix"),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        EdgeKey::decode_fwd(&out[0].0).expect("decode").dst(),
        &service("db")
    );

    // "All edges into db" on edges_rev — both dependents.
    let rev_root = manifest.edges_rev.to_root(cp).expect("root");
    let into = prefix_scan(
        &store,
        &rev_root,
        &edge_endpoint_prefix(&service("db")).expect("prefix"),
    );
    assert_eq!(into.len(), 2);
}

// ---------------------------------------------------------------------------
// Acceptance: manifest hashing deterministic
// ---------------------------------------------------------------------------

#[test]
fn manifest_hash_is_deterministic_and_history_independent() {
    // Build the same graph twice in different ways: bulk load versus
    // empty-plus-batches in a different insertion order, on separate
    // stores. Identical contents must yield identical map roots
    // (Invariant 1), identical manifest bytes, and hence the identical
    // manifest chunk address (Invariant 2 / "hashing deterministic").
    let f = fixture();

    let store_a = MemStore::default();
    let manifest_a = build_manifest(&store_a, &f);

    let store_b = MemStore::default();
    let via_batches = |rows: &[(Vec<u8>, Vec<u8>)]| -> Root {
        let mut root = empty(&store_b, params()).expect("empty");
        // Insert in reverse, one batch per row, to vary operation order.
        for (k, v) in rows.iter().rev() {
            root =
                apply_batch(&store_b, &root, [BatchOp::Put(k.clone(), v.clone())]).expect("apply");
        }
        root
    };
    let manifest_b = Manifest {
        chunk_params: params(),
        schema: MapRoot::from_root(&via_batches(&f.schema_rows)),
        nodes: MapRoot::from_root(&via_batches(&f.node_rows)),
        edges_fwd: MapRoot::from_root(&via_batches(&f.fwd_rows)),
        edges_rev: MapRoot::from_root(&via_batches(&f.rev_rows)),
        indexes: [(
            "host_os".to_owned(),
            MapRoot::from_root(&via_batches(&f.idx_rows)),
        )]
        .into(),
        conflicts: None,
    };

    assert_eq!(
        manifest_a, manifest_b,
        "map roots must be history-independent"
    );
    assert_eq!(manifest_a.encode(), manifest_b.encode());
    let hash_a = store_a.put(&manifest_a.encode()).expect("put");
    let hash_b = store_b.put(&manifest_b.encode()).expect("put");
    assert_eq!(
        hash_a, hash_b,
        "manifest chunk address must be deterministic"
    );
}
