//! Costed anchor choice (acetone-7qw.10): with several planner hints on one
//! pattern, the executor sizes every alternative via
//! `GraphSource::seek_count` and materialises only the smallest, instead of
//! taking the first hint that serves. The measured failure: in the band
//! where an unselective equality just fits its budget, first-serve chose it
//! over a far more selective range on the same pattern — 65× off the best
//! available plan (PR #224 review finding 2).

use std::cell::Cell;
use std::collections::BTreeMap;

use acetone_cypher::ast::Direction;
use acetone_cypher::bind::binder::{BindMode, bind};
use acetone_cypher::exec::source::{GraphSource, SeekProbe};
use acetone_cypher::exec::value::{EntityId, NodeValue, RelValue, Value};
use acetone_cypher::exec::{
    NoProcedures, SingleVersion, catalogue_from_schema, execute_versioned_with,
};
use acetone_model::schema::{IndexDef, LabelDef, PropertyType, SchemaEntry};

/// A source with two indexed properties whose selectivities differ wildly,
/// which counts which seeks are MATERIALISED (the expensive act sizing is
/// meant to avoid).
struct TwoIndexSource {
    /// b = 0 bucket: many nodes (the unselective equality).
    equality_bucket: Vec<NodeValue>,
    /// u > threshold: few nodes (the selective range).
    range_bucket: Vec<NodeValue>,
    /// Whether `seek_count` answers; `false` = sizing unavailable, the
    /// executor must fall back to serve order.
    sizable: bool,
    /// The range DECLINES at materialisation while still sizing small —
    /// the KeySeek-shaped asymmetry (sizing is a probe-shape count, not an
    /// existence check), exercising the winner-declined fall-through.
    range_declines: Cell<bool>,
    equality_materialised: Cell<usize>,
    range_materialised: Cell<usize>,
    equality_sized: Cell<usize>,
    range_sized: Cell<usize>,
}

fn node(id: i64, b: i64, u: i64) -> NodeValue {
    NodeValue {
        id: EntityId::from_bytes(format!("n{id}").into_bytes()),
        labels: vec!["L".into()],
        properties: BTreeMap::from([
            ("id".to_owned(), Value::Int(id)),
            ("b".to_owned(), Value::Int(b)),
            ("u".to_owned(), Value::Int(u)),
        ]),
    }
}

impl TwoIndexSource {
    fn new(sizable: bool) -> Self {
        // 100 nodes with b=0 (u low), plus 3 with b=0 AND u>49990 — the
        // range answer is a strict subset so both plans give one result set.
        let mut equality_bucket: Vec<NodeValue> = (0..100).map(|i| node(i, 0, i)).collect();
        let range_bucket: Vec<NodeValue> = (0..3).map(|i| node(1000 + i, 0, 49991 + i)).collect();
        equality_bucket.extend(range_bucket.iter().cloned());
        TwoIndexSource {
            equality_bucket,
            range_bucket,
            sizable,
            equality_materialised: Cell::new(0),
            range_materialised: Cell::new(0),
            equality_sized: Cell::new(0),
            range_sized: Cell::new(0),
            range_declines: Cell::new(false),
        }
    }
}

impl GraphSource for TwoIndexSource {
    fn all_nodes(&self) -> Vec<NodeValue> {
        self.equality_bucket.clone()
    }
    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.equality_bucket.iter().find(|n| &n.id == id).cloned()
    }
    fn expand(&self, _: &EntityId, _: Direction, _: &[String]) -> Vec<(RelValue, NodeValue)> {
        Vec::new()
    }
    fn nodes_by_index(
        &self,
        name: &str,
        _properties: &[String],
        _values: &[&Value],
    ) -> Option<Vec<NodeValue>> {
        assert_eq!(name, "idx_b");
        self.equality_materialised
            .set(self.equality_materialised.get() + 1);
        Some(self.equality_bucket.clone())
    }
    fn nodes_by_index_range(
        &self,
        name: &str,
        _property: &str,
        _lower: Option<(&Value, bool)>,
        _upper: Option<(&Value, bool)>,
    ) -> Option<Vec<NodeValue>> {
        assert_eq!(name, "idx_u");
        self.range_materialised
            .set(self.range_materialised.get() + 1);
        if self.range_declines.get() {
            return None;
        }
        Some(self.range_bucket.clone())
    }
    fn seek_count(&self, probe: &SeekProbe) -> Option<usize> {
        if !self.sizable {
            return None;
        }
        match probe {
            SeekProbe::Index { name, .. } => {
                assert_eq!(name, "idx_b");
                self.equality_sized.set(self.equality_sized.get() + 1);
                Some(self.equality_bucket.len())
            }
            SeekProbe::Range { name, .. } => {
                assert_eq!(name, "idx_u");
                self.range_sized.set(self.range_sized.get() + 1);
                Some(self.range_bucket.len())
            }
            SeekProbe::Key { .. } => None,
        }
    }
}

fn schema() -> Vec<SchemaEntry> {
    vec![
        SchemaEntry::Label {
            name: "L".into(),
            def: LabelDef::new(
                vec!["id".into()],
                BTreeMap::from([
                    ("b".to_owned(), PropertyType::Int),
                    ("u".to_owned(), PropertyType::Int),
                ]),
                [],
                [],
            )
            .expect("label"),
        },
        SchemaEntry::Index {
            name: "idx_b".into(),
            def: IndexDef::new("L", vec!["b".into()]).expect("index"),
        },
        SchemaEntry::Index {
            name: "idx_u".into(),
            def: IndexDef::new("L", vec!["u".into()]).expect("index"),
        },
    ]
}

fn run(graph: &TwoIndexSource) -> usize {
    // The PR #224 review's measured shape: an unselective equality AND a
    // selective range on one pattern.
    let query = "MATCH (n:L) WHERE n.b = 0 AND n.u > 49990 RETURN n.id";
    let ast = acetone_cypher::parse(query).expect("parse");
    let catalogue = catalogue_from_schema(schema());
    let bound = bind(query, &ast, &catalogue, BindMode::Strict).expect("bind");
    let resolver = SingleVersion::new(graph as &dyn GraphSource);
    let result = execute_versioned_with(&bound, &resolver, &NoProcedures, &BTreeMap::new())
        .expect("execute");
    result.rows.len()
}

#[test]
fn the_smallest_sized_seek_is_the_one_materialised() {
    let graph = TwoIndexSource::new(true);
    let rows = run(&graph);
    assert_eq!(rows, 3, "the WHERE filters to the 3 range nodes");
    assert_eq!(
        (graph.equality_sized.get(), graph.range_sized.get()),
        (1, 1),
        "BOTH hints must be resolved and sized before any materialisation"
    );
    assert_eq!(
        graph.range_materialised.get(),
        1,
        "the selective range (3 candidates) is chosen"
    );
    assert_eq!(
        graph.equality_materialised.get(),
        0,
        "the at-cap equality (103 candidates) is sized but never \
         materialised — first-serve used to pick it (acetone-7qw.10)"
    );
}

#[test]
fn an_unsizable_source_keeps_serve_order() {
    // seek_count = None everywhere: the executor must fall back to the
    // original first-serve behaviour — exactly one hint materialises and
    // the answer is identical.
    let graph = TwoIndexSource::new(false);
    let rows = run(&graph);
    assert_eq!(rows, 3, "same answer whichever plan served");
    assert_eq!(
        graph.equality_materialised.get() + graph.range_materialised.get(),
        1,
        "exactly one seek serves, in hint order"
    );
}

#[test]
fn a_sized_winner_that_declines_falls_through_in_order() {
    // Sizing is not an existence check (a KeySeek sizes on probe shape,
    // and any source may decline at materialisation), so the chosen
    // winner CAN decline — the fall-through must then try the remaining
    // probes in order rather than losing the plan (PR #242 review
    // major 3: this branch is routine, not defensive, and was untested).
    let graph = TwoIndexSource::new(true);
    graph.range_declines.set(true);
    let rows = run(&graph);
    assert_eq!(rows, 3, "the equality plan still answers correctly");
    assert_eq!(
        graph.range_materialised.get(),
        1,
        "the small-sized range wins the choice and is attempted first"
    );
    assert_eq!(
        graph.equality_materialised.get(),
        1,
        "on the winner's decline, the fall-through serves the equality"
    );
}
