//! A lazy, store-backed [`GraphSource`] (ADR-0040, `acetone-cbl.11`).
//!
//! [`GraphSnapshot`](crate::exec::GraphSnapshot) materialises a whole version up
//! front. This source instead reads only what each query touches, straight from
//! the stored prolly maps of an [`acetone_graph::repo::Snapshot`]:
//!
//! - an `IndexSeek` reads only the matching `idx/<name>` entries and fetches
//!   only those node records (the scalability win the secondary index exists
//!   for);
//! - `expand` reads only a node's incident edges (`edges_fwd`/`edges_rev`);
//! - a full label scan still materialises (`all_nodes`) — inherently
//!   O(version) — but a seek/expand-anchored query never reaches it.
//!
//! ## Two correctness hazards, handled here
//!
//! **Lazy reads can fail mid-query.** The [`GraphSource`] methods are infallible
//! (they were designed for a pre-materialised snapshot). A store read that fails
//! here is recorded in [`StoreBackedSource::error`] and returned as empty/None;
//! the caller drains it with [`StoreBackedSource::take_error`] after execution
//! and turns it into a query error, so a corrupt read surfaces rather than
//! silently dropping rows.
//!
//! **Raw stored keys vs. rendered scan matches.** The stored index keys the
//! *raw typed* value, but a scan matches a `Bytes`/temporal property by its
//! *string rendering* (the [`Value::Stored`] carrier decays to a string under
//! `eq3`). A raw-keyed probe would miss those,
//! under-selecting. So `nodes_by_index` only serves a pin when a raw
//! probe cannot miss: numeric and boolean pins always (they never cross-type
//! match a rendering), a string pin only when the indexed property's declared
//! type is a non-deferred scalar; otherwise it falls back to a scan (`None`).

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use acetone_graph::GraphError;
use acetone_graph::repo::Snapshot;
use acetone_model::Value as ModelValue;
use acetone_model::graph_keys::prefix_successor;
use acetone_model::graph_keys::{NodeKey, index_value_prefix};
use acetone_model::schema::{PropertyType, SchemaEntry};
use std::ops::Bound;

use crate::ast::Direction;
use crate::exec::adapter::{node_value, rel_value};
use crate::exec::source::GraphSource;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// A declared index the store-backed seek can serve (single or
/// composite — ADR-0027, acetone-0c7).
struct IndexInfo {
    label: String,
    /// The indexed properties, in declared order.
    properties: Vec<String>,
    /// Each property's declared type, if the schema types it — the
    /// discriminator for whether a string pin is safe to seek (see the
    /// module docs), per component.
    property_types: Vec<Option<PropertyType>>,
}

/// A [`GraphSource`] that reads lazily from a stored [`Snapshot`].
pub struct StoreBackedSource<'s> {
    snapshot: &'s Snapshot<'s>,
    /// label → key property names (to re-expose key values as properties).
    key_names: HashMap<String, Vec<String>>,
    /// index name → its label, ordered properties and per-component types.
    indexes: HashMap<String, IndexInfo>,
    /// The first store read error hit during a query, surfaced by the caller.
    error: Cell<Option<GraphError>>,
    /// Memoised nodes-map cardinality estimate. The snapshot is immutable
    /// for the source's lifetime, so this is sampled once rather than once
    /// per incoming row — recomputing it charged the estimator's chunk reads
    /// to every re-anchored seek, +43% on the per-row point-seek path
    /// (PR #224 review finding 3).
    node_count: Cell<Option<usize>>,
}

impl<'s> StoreBackedSource<'s> {
    /// Build over `snapshot`, using `schema` for key-property names and
    /// the seekable declared indexes (single and composite).
    pub fn new(snapshot: &'s Snapshot<'s>, schema: &[SchemaEntry]) -> Self {
        let mut key_names: HashMap<String, Vec<String>> = HashMap::new();
        let mut label_types: HashMap<String, BTreeMap<String, PropertyType>> = HashMap::new();
        for entry in schema {
            if let SchemaEntry::Label { name, def } = entry {
                key_names.insert(name.clone(), def.key().to_vec());
                label_types.insert(name.clone(), def.types().clone());
            }
        }
        let mut indexes: HashMap<String, IndexInfo> = HashMap::new();
        for entry in schema {
            if let SchemaEntry::Index { name, def } = entry {
                let property_types = def
                    .properties()
                    .iter()
                    .map(|property| {
                        label_types
                            .get(def.label())
                            .and_then(|types| types.get(property))
                            .copied()
                    })
                    .collect();
                indexes.insert(
                    name.clone(),
                    IndexInfo {
                        label: def.label().to_owned(),
                        properties: def.properties().to_vec(),
                        property_types,
                    },
                );
            }
        }
        StoreBackedSource {
            snapshot,
            key_names,
            indexes,
            error: Cell::new(None),
            node_count: Cell::new(None),
        }
    }

    /// Take the first store read error hit during a query, if any. The caller
    /// runs this after execution: a lazy read cannot return its error through
    /// the infallible [`GraphSource`] trait, so it is recorded and drained here.
    pub fn take_error(&self) -> Option<GraphError> {
        self.error.take()
    }

    /// Record the first error and yield the fallback the trait method returns.
    /// A later error is a downstream symptom of the first, so the first is kept.
    fn fail<T>(&self, error: GraphError, fallback: T) -> T {
        let first = self.error.take().or(Some(error));
        self.error.set(first);
        fallback
    }

    /// How many candidates a seek over this snapshot may materialise before
    /// declining, sampling the nodes map at most once per source.
    ///
    /// `None` means the nodes map could not be sampled, which the callers
    /// treat as "cannot serve" — a scan is always a correct answer.
    fn candidate_budget(&self) -> Option<usize> {
        let rows = match self.node_count.get() {
            Some(cached) => cached,
            None => {
                let sampled = self.snapshot.estimate_nodes()?;
                self.node_count.set(Some(sampled));
                sampled
            }
        };
        Some(candidate_cap(rows))
    }

    /// Fetch and build one node by its stored key, recording any read error.
    fn node_from_key(&self, key: &NodeKey) -> Option<NodeValue> {
        match self.snapshot.get_node(key) {
            Ok(Some(record)) => Some(node_value(key, &record, &self.key_names)),
            Ok(None) => None,
            Err(e) => self.fail(e, None),
        }
    }

    /// Decode an entity id back to its stored node key. The id is exactly the
    /// `nodes`-map key encoding ([`NodeKey::encode`]), so this round-trips.
    fn key_of(&self, id: &EntityId) -> Option<NodeKey> {
        NodeKey::decode(id.0.as_ref()).ok()
    }

    /// Probe alternatives for the `component`-th indexed property: the
    /// candidate raw model values whose stored index key the pin could
    /// equal, or `None` to fall back to a scan (the pin cannot be
    /// served exactly).
    fn probe_value(
        &self,
        info: &IndexInfo,
        component: usize,
        value: &Value,
    ) -> Option<Vec<ModelValue>> {
        match value {
            // A null or NaN pin selects nothing (indexes are null/NaN-blind).
            Value::Null => Some(Vec::new()),
            Value::Float(f) if f.is_nan() => Some(Vec::new()),
            // A list pin needs element-wise equality no byte bucket serves.
            Value::List(_) => None,
            // A carrier never originates in a query pin; be safe and scan.
            Value::Stored(_) => None,
            // A numeric pin is always safe: it can never cross-type match a
            // Bytes/temporal *rendering* (a hex/debug string), so no raw entry
            // is missed. Probe BOTH numeric encodings (3 = 3.0).
            Value::Int(n) => Some(vec![ModelValue::Int(*n), ModelValue::Float(*n as f64)]),
            Value::Float(f) => {
                // An integer-valued float ≥ 2^53 has a non-unique i64 preimage,
                // so the single `f as i64` probe would under-select — scan.
                if f.fract() == 0.0 && f.abs() >= 9_007_199_254_740_992.0 {
                    return None;
                }
                let mut values = vec![ModelValue::Float(*f)];
                if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                    values.push(ModelValue::Int(*f as i64));
                }
                Some(values)
            }
            Value::Bool(b) => Some(vec![ModelValue::Bool(*b)]),
            // A string pin could equal a Bytes/temporal value's rendering, which
            // is keyed raw — so a raw probe would miss it. Safe only when the
            // property's declared type rules out a deferred value.
            Value::String(s) => match info.property_types.get(component).copied().flatten() {
                Some(PropertyType::String)
                | Some(PropertyType::Int)
                | Some(PropertyType::Float)
                | Some(PropertyType::Bool) => Some(vec![ModelValue::String(s.clone())]),
                _ => None,
            },
            // Non-storable kinds never index.
            Value::Map(_) | Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
                Some(Vec::new())
            }
        }
    }
}

/// How many candidates a seek may materialise before it should decline and
/// let the scan answer (acetone-2ck.2).
///
/// A seek does one **random** point read per matching row; the scan it
/// replaces reads the nodes map **sequentially**. Firing regardless is what
/// made a declared index able to make a query *slower*: 3.7x at 2.9%
/// selectivity, 18x at 20%, and up to 53x on a small label. So the budget is
/// a **fraction of the scan's own cost**, not a constant.
///
/// `BREAK_EVEN_PERMILLE` is that fraction, measured on this project's
/// loose-object store by timing the two primitives directly — a full
/// sequential read of the nodes map against a batch of random point reads —
/// across node sizes from 29 to 1036 bytes per row and 50k to 200k rows:
///
/// ```text
///   29 bytes/row,   50k rows:  scan  28.4ms, point read 201us -> 0.28%
///   235 bytes/row,  50k rows:  scan  23.5ms, point read 150us -> 0.31%
///   1036 bytes/row, 50k rows:  scan  56.1ms, point read 185us -> 0.61%
///   29 bytes/row,  200k rows:  scan 179.9ms, point read 249us -> 0.36%
/// ```
///
/// An earlier version of this model made the fraction proportional to bytes
/// per row, reasoning that a scan over fat records costs more and so buys
/// the seek more room. **The measurement above refutes that**: 36x the bytes
/// moves break-even by about 2x, because a point read is dominated by
/// per-object overhead rather than by size, and a scan by per-entry decoding.
/// The fraction is near enough constant, so the model is a constant, and the
/// `SizeEstimate`-carrying variant was abandoned rather than shipped.
///
/// End to end, a query costs more than these primitives on both sides, which
/// dilutes the ratio: measured that way break-even is nearer 1%. That is the
/// number to calibrate to, since it is what a user experiences — but 2% ran
/// 1.9x *slower* than the scan on a small-record shape (PR #224 review
/// finding 4), so it is the ceiling of the range and not its middle. Packed
/// stores make random reads cheaper still, so calibrating on loose objects is
/// the conservative direction.
///
/// The floor keeps point-lookup-shaped queries working on tiny graphs, where
/// a scan is cheap anyway so a wrong choice costs little.
///
/// An earlier version tiered on the index's prolly height, which fails
/// structurally: height changes once per fanout (~10x in entries), so one
/// tier spans a 10x range of cardinalities.
/// Dividing rather than multiplying-then-dividing is deliberate: the estimate
/// is an `f64` product cast with `as usize`, which **saturates**, so a
/// pathological tree can present `usize::MAX` here. `rows * 5 / 1000`
/// overflows above `usize::MAX / 5` — a debug-build panic on a crafted
/// repository (PR #224 review finding 1). `rows / 200` is the same 0.5% and
/// cannot overflow.
pub fn candidate_cap(estimated_rows: usize) -> usize {
    const BREAK_EVEN_ONE_IN: usize = 200; // 0.5%
    (estimated_rows / BREAK_EVEN_ONE_IN).max(CANDIDATE_FLOOR)
}

/// Candidates a seek may always materialise, whatever the graph's size.
///
/// A result this small beats a scan on any graph large enough for the
/// question to matter, so it is served without sampling the nodes map at
/// all — which is also what keeps the estimator off the point-lookup path.
pub const CANDIDATE_FLOOR: usize = 32;

impl GraphSource for StoreBackedSource<'_> {
    fn all_nodes(&self) -> Vec<NodeValue> {
        match self.snapshot.nodes() {
            Ok(nodes) => nodes
                .iter()
                .map(|(key, record)| node_value(key, record, &self.key_names))
                .collect(),
            Err(e) => self.fail(e, Vec::new()),
        }
    }

    fn nodes_by_key(&self, label: &str, key_values: &[Value]) -> Option<Vec<NodeValue>> {
        // A primary-key pin is an exact lookup in the nodes map — the
        // cheapest seek there is, and the one the shipped read path was
        // missing entirely (Phase 9 security review finding 7).
        //
        // Candidate-superset semantics as everywhere else: probe both
        // numeric encodings (3 == 3.0 in openCypher but they encode
        // differently), and a probe set we cannot form means "cannot
        // serve" — scan — never "definitively absent".
        let mut per_component: Vec<Vec<ModelValue>> = Vec::with_capacity(key_values.len());
        for value in key_values {
            let alternatives = match value {
                // Null/NaN can never be a key value (keys are non-null),
                // so nothing matches — but say "cannot serve" rather than
                // asserting absence.
                Value::Null => return None,
                Value::Float(f) if f.is_nan() => return None,
                Value::Int(n) => vec![ModelValue::Int(*n), ModelValue::Float(*n as f64)],
                Value::Float(f) => {
                    // An integral float >= 2^53 has a non-unique i64
                    // preimage; a single probe would under-select.
                    if f.fract() == 0.0 && f.abs() >= 9_007_199_254_740_992.0 {
                        return None;
                    }
                    let mut alts = vec![ModelValue::Float(*f)];
                    if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                        alts.push(ModelValue::Int(*f as i64));
                    }
                    alts
                }
                // A STRING pin cannot be served: a Bytes/temporal key
                // value compares equal to its string *rendering* at
                // runtime (ADR-0038 carriers decay under eq3), and the
                // stored encodings differ, so a probe on the string
                // encoding alone would MISS such a node — under-selection,
                // which candidate-superset semantics forbid. `probe_value`
                // guards this for indexes via the declared type; keys have
                // no such guard here, so decline and let the scan answer
                // (PR #219 review blocker 3 — a wrong-answer regression).
                Value::String(_) => return None,
                // A carrier pin has the same hazard from the other side.
                Value::Stored(_) => return None,
                other => vec![crate::exec::adapter::model_value_of(other)?],
            };
            per_component.push(alternatives);
        }
        // `product()` wraps on overflow in release, so a key of >=64
        // numeric components made the cap vacuous and the tuple loop
        // allocate unboundedly (PR #219 review finding 4).
        let mut combinations: usize = 1;
        for alternatives in &per_component {
            combinations = combinations.checked_mul(alternatives.len())?;
            if combinations > 16 {
                return None;
            }
        }
        let mut tuples: Vec<Vec<ModelValue>> = vec![Vec::new()];
        for alternatives in &per_component {
            let mut next = Vec::with_capacity(tuples.len() * alternatives.len());
            for prefix in &tuples {
                for alt in alternatives {
                    let mut tuple = prefix.clone();
                    tuple.push(alt.clone());
                    next.push(tuple);
                }
            }
            tuples = next;
        }
        let mut out = Vec::new();
        let mut served = false;
        for tuple in tuples {
            let Ok(key) = acetone_model::graph_keys::NodeKey::new(label, tuple) else {
                continue;
            };
            match self.snapshot.get_node(&key) {
                Ok(Some(record)) => {
                    served = true;
                    out.push(node_value(&key, &record, &self.key_names));
                }
                // A well-formed probe that finds nothing is a real answer
                // for an exact key, but stay conservative and let the
                // scan confirm rather than assert absence here.
                Ok(None) => {}
                Err(e) => return self.fail(e, None),
            }
        }
        if served { Some(out) } else { None }
    }

    fn nodes_by_index(
        &self,
        index_name: &str,
        properties: &[String],
        values: &[&Value],
    ) -> Option<Vec<NodeValue>> {
        let info = self.indexes.get(index_name)?;
        // The hint may have been bound against another version's catalogue
        // (AT clauses); a same-named index over different properties must
        // not serve it (PR #206 review finding 4), and the value tuple
        // must match the declared arity.
        if info.properties.as_slice() != properties || values.len() != properties.len() {
            return None;
        }
        // Per-component probe alternatives, then their bounded cartesian
        // (mirrors adapter.rs::cartesian_probes over byte encodings —
        // keep caps and bail rules aligned): a composite entry's key is
        // the ordered value tuple (ADR-0027).
        let mut per_component: Vec<Vec<ModelValue>> = Vec::with_capacity(values.len());
        for (component, value) in values.iter().enumerate() {
            let probes = self.probe_value(info, component, value)?;
            if probes.is_empty() {
                // A null/NaN component matches nothing (null-blind index).
                return Some(Vec::new());
            }
            per_component.push(probes);
        }
        let combinations: usize = per_component.iter().map(Vec::len).product();
        if combinations > 16 {
            return None;
        }
        let mut tuples: Vec<Vec<ModelValue>> = vec![Vec::new()];
        for alternatives in &per_component {
            let mut next = Vec::with_capacity(tuples.len() * alternatives.len());
            for prefix in &tuples {
                for alt in alternatives {
                    let mut tuple = prefix.clone();
                    tuple.push(alt.clone());
                    next.push(tuple);
                }
            }
            tuples = next;
        }
        // The equality/composite path had no budget at all: it fired
        // however unselective the probe was, which is what made declaring
        // an index able to make a query slower (acetone-2ck.2).
        let candidates = self.within_budget(|cap| {
            self.equality_candidates(index_name, &info.label, properties, &tuples, cap)
        })?;
        // Only now, with the whole candidate set known to be under budget,
        // pay for the point reads.
        let mut out = Vec::with_capacity(candidates.len());
        for key in &candidates {
            if let Some(node) = self.node_from_key(key) {
                out.push(node);
            }
        }
        Some(out)
    }

    fn nodes_by_index_range(
        &self,
        index_name: &str,
        property: &str,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
    ) -> Option<Vec<NodeValue>> {
        // Ranges serve single-property indexes only, and the hint may have
        // been bound against another version's catalogue (AT clauses), so
        // the registry entry must be exactly [property].
        let info = self.indexes.get(index_name)?;
        if info.properties.len() != 1 || info.properties[0] != property {
            return None;
        }
        // Serve NUMERIC bounds only. The hazard is a deferred-typed
        // (Bytes/temporal) stored value, which the runtime compares as its
        // string *rendering* while the index holds its own encoding — so a
        // byte range could miss it. A numeric bound is immune, exactly as
        // `probe_value` argues for equality pins: a string rendering never
        // compares less/greater than a number in openCypher (the
        // comparison is null), so no row that the predicate would accept
        // is left out. A non-numeric bound has no such guarantee, so it
        // declines and the scan answers.
        //
        // Gating on the *bounds* rather than on a declared property type
        // matters: `declare-label` does not take property types, so a
        // declared-type test declines on ordinary graphs and the seek
        // never fires — measured, not assumed.
        let numeric_bound = |b: &Option<(&Value, bool)>| {
            b.is_none_or(|(v, _)| matches!(v, Value::Int(_) | Value::Float(_)))
        };
        if !numeric_bound(&lower) || !numeric_bound(&upper) {
            return None;
        }
        // The value ranges themselves come from the same reviewed helper
        // the in-memory source uses (dual int/float families, precision
        // hazards, zero-widening), then are lifted into index-key space.
        let families = crate::exec::adapter::range_families(lower, upper)?;
        if families.is_empty() {
            return Some(Vec::new());
        }
        // An index key is encode_key([label, [property], [value]]) ++ node
        // key. Everything before the value list is constant, and with ONE
        // element in that list the bytes after `TAG_LIST` sort exactly as
        // the value does — which is what makes a byte range sound here.
        // Derived from the encoder rather than by hand: encode the key
        // with an EMPTY value list and drop the list terminator, leaving
        // exactly the bytes every entry for this index shares, up to and
        // including the value list's opening tag. This cannot drift from
        // the encoding the way a hand-written tag byte could.
        let mut head = acetone_model::keys::encode_key(&[
            ModelValue::String(info.label.clone()),
            ModelValue::List(vec![ModelValue::String(property.to_owned())]),
            ModelValue::List(Vec::new()),
        ])
        .ok()?;
        head.pop()?;

        // A range seek does one RANDOM point read per matching entry;
        // the label scan it replaces reads the nodes map sequentially. So
        // the seek wins only while the range is selective, and loses
        // badly when it is not — measured at up to 37x SLOWER than the
        // scan for a range covering the whole label (PR #221 review
        // blocker). Declining past a cap keeps the selective win and
        // removes the cliff: `None` is the trait's "cannot serve, scan
        // instead", and the scan is always correct.
        //
        // Sized from the estimated scan cost (acetone-2ck.2).
        let candidates =
            self.within_budget(|cap| self.range_candidates(index_name, &head, &families, cap))?;
        // Point reads only once the whole set is known to be under budget.
        let mut out = Vec::with_capacity(candidates.len());
        for key in &candidates {
            if let Some(node) = self.node_from_key(key) {
                out.push(node);
            }
        }
        Some(out)
    }

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        let Some(key) = self.key_of(node) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let type_ok = |rtype: &str| types.is_empty() || types.iter().any(|t| t == rtype);

        // Out-edges first (matching GraphSnapshot's order), then in-edges.
        if matches!(direction, Direction::Out | Direction::Undirected) {
            match self.snapshot.out_edges(&key) {
                Ok(edges) => {
                    for (edge, record) in edges {
                        if !type_ok(edge.rtype()) {
                            continue;
                        }
                        if let Some(neighbour) = self.node_from_key(edge.dst()) {
                            out.push((rel_value(&edge, &record), neighbour));
                        }
                    }
                }
                Err(e) => return self.fail(e, Vec::new()),
            }
        }
        if matches!(direction, Direction::In | Direction::Undirected) {
            match self.snapshot.in_edges(&key) {
                Ok(edges) => {
                    for (edge, record) in edges {
                        // A self-loop is already emitted by the out pass; skip
                        // its second sighting under Undirected.
                        if direction == Direction::Undirected && edge.src() == &key {
                            continue;
                        }
                        if !type_ok(edge.rtype()) {
                            continue;
                        }
                        if let Some(neighbour) = self.node_from_key(edge.src()) {
                            out.push((rel_value(&edge, &record), neighbour));
                        }
                    }
                }
                Err(e) => return self.fail(e, Vec::new()),
            }
        }
        out
    }

    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        let key = self.key_of(id)?;
        self.node_from_key(&key)
    }
}

/// One half-open byte range over an index map, as
/// [`range_families`](crate::exec::adapter::range_families) emits them: a
/// numeric predicate yields one per numeric family (int, float).
type ByteRange = (Bound<Vec<u8>>, Bound<Vec<u8>>);

/// The outcome of collecting a probe's candidates under a budget.
///
/// Over-budget is its own variant rather than a length the caller compares,
/// because the two are not equivalent: probes are de-duplicated across
/// families and composite tuples, so a walk that stopped early can still
/// yield *fewer* than `cap` distinct keys. Reporting that by length would
/// let the caller mistake a truncated walk for a complete one and return a
/// short answer — under-selection, which is a wrong answer, not a slow one.
enum Candidates {
    /// Every candidate the probe matches.
    Complete(Vec<NodeKey>),
    /// The probe matches more than the budget allows.
    OverBudget,
}

impl StoreBackedSource<'_> {
    /// The candidate keys of a numeric range, stopping once more than `cap`
    /// have been walked. `None` means the index is missing or a read failed.
    fn range_candidates(
        &self,
        index_name: &str,
        head: &[u8],
        families: &[ByteRange],
        cap: usize,
    ) -> Option<Candidates> {
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut candidates: Vec<NodeKey> = Vec::new();
        for (start, end) in families {
            let lifted = |bytes: &[u8]| {
                let mut k = head.to_vec();
                k.extend_from_slice(bytes);
                k
            };
            // Inclusivity is expressed against the *value region*: a key
            // for value v continues past v's encoding (list terminator,
            // then the node key), so an inclusive upper bound must run to
            // the successor of that region, and an exclusive lower bound
            // must start there.
            //
            // INVARIANT (PR #221 review finding 5): the two arms that
            // apply `prefix_successor` — exclusive lower, inclusive upper
            // — require a COMPLETE value encoding. `range_families` also
            // emits bare family-tag sentinels (`[0x04]`, `[0x05]`, …),
            // and those only ever appear on the inclusive-lower and
            // exclusive-upper arms, which pass the bytes through
            // untouched. If that ever changed, `prefix_successor` on a
            // lone `[0x04]` would yield `[0x05]` and silently skip the
            // whole int family — under-selection, while the in-memory
            // source stayed correct. The debug assertion below pins it.
            debug_assert!(
                !matches!(&start, Bound::Excluded(v) if v.len() == 1),
                "a bare family sentinel must not reach the exclusive-lower arm"
            );
            debug_assert!(
                !matches!(&end, Bound::Included(v) if v.len() == 1),
                "a bare family sentinel must not reach the inclusive-upper arm"
            );
            let start = match start {
                Bound::Included(v) => Bound::Included(lifted(v)),
                Bound::Excluded(v) => match prefix_successor(&lifted(v)) {
                    Some(next) => Bound::Included(next),
                    None => continue,
                },
                Bound::Unbounded => Bound::Included(head.to_vec()),
            };
            let end = match end {
                Bound::Included(v) => match prefix_successor(&lifted(v)) {
                    Some(next) => Bound::Excluded(next),
                    None => Bound::Unbounded,
                },
                Bound::Excluded(v) => Bound::Excluded(lifted(v)),
                Bound::Unbounded => match prefix_successor(head) {
                    Some(next) => Bound::Excluded(next),
                    None => Bound::Unbounded,
                },
            };
            // Budget across families: the int and float halves of one
            // numeric range share the cap, so their sum decides.
            let remaining = cap.saturating_sub(seen.len());
            match self.snapshot.index_range(index_name, start, end, remaining) {
                Ok(None) => return None,
                Ok(Some(keys)) => {
                    if keys.len() > remaining {
                        return Some(Candidates::OverBudget);
                    }
                    for key in keys {
                        let Ok(encoded) = key.encode() else { continue };
                        if seen.insert(encoded) {
                            candidates.push(key);
                        }
                    }
                }
                Err(e) => return self.fail(e, None),
            }
        }
        Some(Candidates::Complete(candidates))
    }

    /// The candidate keys of an equality/composite probe set, stopping once
    /// more than `cap` have been walked. Every probe's keys are gathered
    /// before the total is judged: an earlier version tested per probe, so a
    /// composite whose rows split across probes paid the point reads for the
    /// first probe before declining on the second (PR #224 review finding 5).
    fn equality_candidates(
        &self,
        index_name: &str,
        label: &str,
        properties: &[String],
        tuples: &[Vec<ModelValue>],
        cap: usize,
    ) -> Option<Candidates> {
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut candidates: Vec<NodeKey> = Vec::new();
        for tuple in tuples {
            let prefix = match index_value_prefix(label, properties, tuple) {
                Ok(prefix) => prefix,
                // A value that cannot encode (e.g. a NaN nested somewhere)
                // contributes no entries — it indexes nothing.
                Err(_) => continue,
            };
            let remaining = cap.saturating_sub(seen.len());
            match self
                .snapshot
                .index_scan_capped(index_name, &prefix, remaining)
            {
                // Index map absent though the schema declares it: fall back.
                Ok(None) => return None,
                Ok(Some(keys)) => {
                    if keys.len() > remaining {
                        return Some(Candidates::OverBudget);
                    }
                    for key in keys {
                        let Ok(encoded) = key.encode() else { continue };
                        if seen.insert(encoded) {
                            candidates.push(key);
                        }
                    }
                }
                Err(e) => return self.fail(e, None),
            }
        }
        Some(Candidates::Complete(candidates))
    }

    /// Run `collect` under the seek budget, returning its candidates only if
    /// they fit.
    ///
    /// Two phases, so that a selective seek never pays for the estimator.
    /// A result no larger than [`CANDIDATE_FLOOR`] beats a scan on any graph
    /// big enough for the question to matter, so it is served without
    /// sampling the nodes map at all — which keeps ~50 chunk reads off the
    /// point-lookup path, the very case an index exists for. Only a probe
    /// that clears the floor is worth costing properly.
    fn within_budget<F>(&self, collect: F) -> Option<Vec<NodeKey>>
    where
        F: Fn(usize) -> Option<Candidates>,
    {
        if let Candidates::Complete(keys) = collect(CANDIDATE_FLOOR)? {
            return Some(keys);
        }
        let budget = self.candidate_budget()?;
        match collect(budget)? {
            Candidates::Complete(keys) => Some(keys),
            // Unselective: hand the work back to the scan before paying
            // for a single point read.
            Candidates::OverBudget => None,
        }
    }
}
