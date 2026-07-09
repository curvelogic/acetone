//! A deterministic asset-registry lab graph and the realistic registry
//! queries the roadmap names as the Phase 2 correctness / interactive-
//! latency check (bead acetone-yzc.8).
//!
//! The graph models a security-asset registry: hosts run software,
//! software depends on other software and is supplied by suppliers, and
//! hosts hold certificates. Generation is fully deterministic (a seeded
//! LCG, no wall-clock or RNG — the workspace bans nondeterminism) so a
//! given `scale` always yields the same graph and the same query counts.

use std::collections::BTreeMap;

use acetone_graph::Repository;
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{IndexDef, LabelDef, PropertyType, RelTypeDef, SchemaEntry};

/// A deterministic linear-congruential generator (no `Math.random` /
/// wall-clock, which the workspace forbids). Reproducible across runs.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed.wrapping_mul(6364136223846793005).wrapping_add(1))
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    /// Uniform in `[0, n)`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 33) as usize % n.max(1)
    }
}

/// Counts of each entity kind, derived from a single `scale` knob so the
/// shape stays realistic (hosts dominate; a smaller supplier set).
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    pub hosts: usize,
    pub software: usize,
    pub suppliers: usize,
    pub certificates: usize,
    /// The last `orphans` software indices are never `RUNS`-targeted, so
    /// the "orphaned software" query has a real, non-empty answer.
    pub orphans: usize,
}

impl Shape {
    /// `scale` is the host count; the rest scale proportionally. `scale =
    /// 50_000` gives ~110k nodes / ~220k edges (the node total is >50k
    /// because `scale` counts hosts, and hosts+software+suppliers+certs
    /// all contribute; the edge total is ~the roadmap's 200k).
    pub fn from_scale(scale: usize) -> Self {
        let software = (scale / 5).max(2);
        Shape {
            hosts: scale,
            software,
            suppliers: (scale / 250).max(1),
            certificates: scale, // one cert per host
            orphans: (software / 20).max(1),
        }
    }

    pub fn nodes(&self) -> usize {
        self.hosts + self.software + self.suppliers + self.certificates
    }

    /// Software indices hosts may run (the orphan tail is excluded).
    fn runnable_software(&self) -> usize {
        self.software.saturating_sub(self.orphans).max(1)
    }
}

/// Declare the registry schema, generate a graph of `shape`, and commit
/// it. Returns the number of nodes and edges written.
pub fn build(
    repo: &Repository,
    shape: Shape,
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let mut tx = repo.begin_write()?;

    for entry in schema() {
        tx.put_schema(&entry)?;
    }

    let mut edges = 0usize;
    let mut rng = Lcg::new(0x5EED);

    // Suppliers.
    for i in 0..shape.suppliers {
        let key = NodeKey::new("Supplier", vec![Value::String(format!("supplier-{i}"))])?;
        let mut props = BTreeMap::new();
        props.insert("country".to_string(), Value::String(country(i).to_string()));
        tx.put_node(&key, &NodeRecord::new([], props))?;
    }

    // Software: each supplied by a supplier, some depending on others.
    for i in 0..shape.software {
        let key = software_key(i)?;
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::String(format!("pkg-{i}")));
        props.insert(
            "version".to_string(),
            Value::String(format!("{}.{}", i % 9, i % 7)),
        );
        props.insert("size_kb".to_string(), Value::Int((100 + i % 9000) as i64));
        tx.put_node(&key, &NodeRecord::new([], props))?;

        let supplier = NodeKey::new(
            "Supplier",
            vec![Value::String(format!("supplier-{}", i % shape.suppliers))],
        )?;
        tx.put_edge(
            &EdgeKey::new(key.clone(), "SUPPLIED_BY", supplier, Value::Null)?,
            &EdgeRecord::new(BTreeMap::new()),
        )?;
        edges += 1;

        // A dependency on an earlier package (a shallow DAG).
        if i > 0 {
            let dep = software_key(rng.below(i))?;
            tx.put_edge(
                &EdgeKey::new(key.clone(), "DEPENDS_ON", dep, Value::Null)?,
                &EdgeRecord::new(BTreeMap::new()),
            )?;
            edges += 1;
        }
    }

    // Hosts: each runs a few software packages and holds one certificate.
    for i in 0..shape.hosts {
        let key = NodeKey::new("Host", vec![Value::String(format!("host-{i}"))])?;
        let mut props = BTreeMap::new();
        props.insert("os".to_string(), Value::String(os(i).to_string()));
        props.insert("criticality".to_string(), Value::Int((i % 5) as i64));
        props.insert("decommissioned".to_string(), Value::Bool(i % 17 == 0));
        tx.put_node(&key, &NodeRecord::new([], props))?;

        // RUNS: up to 3 distinct software packages (distinct so the edge
        // count is exact — a repeated key would collapse to one edge).
        // Picks exclude the orphan tail, so those stay unreferenced.
        let mut runs = Vec::new();
        for _ in 0..3 {
            let target = rng.below(shape.runnable_software());
            if !runs.contains(&target) {
                runs.push(target);
            }
        }
        for target in runs {
            let sw = software_key(target)?;
            tx.put_edge(
                &EdgeKey::new(key.clone(), "RUNS", sw, Value::Null)?,
                &EdgeRecord::new(BTreeMap::new()),
            )?;
            edges += 1;
        }

        // HAS_CERT: one certificate, some expiring.
        let cert = NodeKey::new("Certificate", vec![Value::String(format!("cert-{i}"))])?;
        let mut cert_props = BTreeMap::new();
        cert_props.insert("cn".to_string(), Value::String(format!("host-{i}.example")));
        // not_after as a day-number; deadline queries compare integers.
        cert_props.insert("not_after".to_string(), Value::Int((i % 365) as i64));
        tx.put_node(&cert, &NodeRecord::new([], cert_props))?;
        tx.put_edge(
            &EdgeKey::new(key.clone(), "HAS_CERT", cert, Value::Null)?,
            &EdgeRecord::new(BTreeMap::new()),
        )?;
        edges += 1;
    }

    tx.commit("lab: generated asset registry", &[], None)?;
    Ok((shape.nodes(), edges))
}

fn software_key(i: usize) -> Result<NodeKey, acetone_model::graph_keys::GraphKeyError> {
    NodeKey::new("Software", vec![Value::String(format!("pkg-{i}"))])
}

fn os(i: usize) -> &'static str {
    ["debian", "ubuntu", "rhel", "alpine", "windows"][i % 5]
}

fn country(i: usize) -> &'static str {
    ["DE", "US", "GB", "FR", "JP", "CN"][i % 6]
}

/// The registry schema: keyed labels, relationship types, and a secondary
/// index on `Host.os` (exercises the binder's IndexSeek planning hint).
pub fn schema() -> Vec<SchemaEntry> {
    let mut host_types = BTreeMap::new();
    host_types.insert("os".to_string(), PropertyType::String);
    host_types.insert("criticality".to_string(), PropertyType::Int);
    host_types.insert("decommissioned".to_string(), PropertyType::Bool);

    let mut sw_types = BTreeMap::new();
    sw_types.insert("name".to_string(), PropertyType::String);
    sw_types.insert("version".to_string(), PropertyType::String);
    sw_types.insert("size_kb".to_string(), PropertyType::Int);

    let mut supplier_types = BTreeMap::new();
    supplier_types.insert("country".to_string(), PropertyType::String);

    let mut cert_types = BTreeMap::new();
    cert_types.insert("cn".to_string(), PropertyType::String);
    cert_types.insert("not_after".to_string(), PropertyType::Int);

    vec![
        SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(vec!["hostname".into()], host_types, [], []).expect("valid"),
        },
        SchemaEntry::Label {
            name: "Software".into(),
            def: LabelDef::new(vec!["name".into()], sw_types, [], []).expect("valid"),
        },
        SchemaEntry::Label {
            name: "Supplier".into(),
            def: LabelDef::new(vec!["name".into()], supplier_types, [], []).expect("valid"),
        },
        SchemaEntry::Label {
            name: "Certificate".into(),
            def: LabelDef::new(vec!["serial".into()], cert_types, [], []).expect("valid"),
        },
        SchemaEntry::RelType {
            name: "RUNS".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("valid"),
        },
        SchemaEntry::RelType {
            name: "DEPENDS_ON".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("valid"),
        },
        SchemaEntry::RelType {
            name: "SUPPLIED_BY".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("valid"),
        },
        SchemaEntry::RelType {
            name: "HAS_CERT".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("valid"),
        },
        SchemaEntry::Index {
            name: "host_os".into(),
            def: IndexDef::new("Host", vec!["os".into()]).expect("valid"),
        },
    ]
}

/// The realistic asset-registry queries (name, cypher) the roadmap names.
pub fn registry_queries() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "certificate expiry sweep",
            "MATCH (h:Host)-[:HAS_CERT]->(c:Certificate) \
             WHERE c.not_after < 30 AND NOT h.decommissioned \
             RETURN h, c.cn, c.not_after ORDER BY c.not_after LIMIT 100",
        ),
        (
            "orphaned software",
            "MATCH (s:Software) WHERE NOT (s)<-[:RUNS]-(:Host) RETURN s.name ORDER BY s.name",
        ),
        (
            "supply-chain blast radius (var-length deps)",
            "MATCH (v:Supplier {name: 'supplier-0'})<-[:SUPPLIED_BY]-(s:Software) \
             OPTIONAL MATCH (s)<-[:DEPENDS_ON*0..3]-(top:Software)<-[:RUNS]-(h:Host) \
             RETURN count(DISTINCT h) AS exposed_hosts",
        ),
        (
            "hosts by OS (indexed property)",
            "MATCH (h:Host {os: 'debian'}) RETURN count(*) AS debian_hosts",
        ),
        (
            "critical hosts running a package from a DE supplier",
            "MATCH (h:Host)-[:RUNS]->(s:Software)-[:SUPPLIED_BY]->(v:Supplier) \
             WHERE v.country = 'DE' AND h.criticality >= 3 \
             RETURN count(DISTINCT h) AS n",
        ),
    ]
}
