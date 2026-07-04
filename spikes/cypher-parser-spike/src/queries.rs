//! The representative query set for the Gate B parser spike.
//!
//! Drawn from three sources, per the bead's acceptance criteria:
//! - spec §5.1 Level R (read) and Level W (write) target subsets;
//! - the roadmap's lab asset-registry queries (hosts, software, suppliers,
//!   certificates);
//! - spec §5.2 versioning surface (`AT <ref>`, `CALL acetone.*` procedures).
//!
//! Each entry records which conformance bucket it exercises and whether it
//! is standard openCypher or an acetone extension — the extension entries
//! are the extensibility probe: a candidate parser must either accept them
//! or be cheaply extensible to.

pub struct SpikeQuery {
    pub name: &'static str,
    pub category: Category,
    pub text: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// spec §5.1 Level R — must parse for Phase 2.
    Read,
    /// spec §5.1 Level W — must parse by Phase 3; parse support now is a plus.
    Write,
    /// spec §5.2 — acetone syntax extensions (`AT <ref>`).
    Extension,
    /// spec §5.2 — standard CALL syntax for acetone procedures.
    Procedure,
    /// Deliberately malformed — probes error quality and recovery.
    Invalid,
}

pub const QUERIES: &[SpikeQuery] = &[
    // --- Level R: basic MATCH / RETURN ---------------------------------
    SpikeQuery {
        name: "match-all-label",
        category: Category::Read,
        text: "MATCH (n:Host) RETURN n",
    },
    SpikeQuery {
        name: "match-property-map",
        category: Category::Read,
        text: "MATCH (h:Host {hostname: 'web-01'}) RETURN h.hostname, h.os AS os",
    },
    SpikeQuery {
        name: "match-relationship",
        category: Category::Read,
        text: "MATCH (h:Host)-[:RUNS]->(s:Software) \
               WHERE s.version STARTS WITH '2.' \
               RETURN h.hostname, s.name ORDER BY h.hostname LIMIT 10",
    },
    SpikeQuery {
        name: "match-undirected-reltype-alt",
        category: Category::Read,
        text: "MATCH (h:Host)-[r:RUNS|HOSTS]-(m) RETURN type(r), m",
    },
    SpikeQuery {
        name: "optional-match",
        category: Category::Read,
        text: "MATCH (h:Host) OPTIONAL MATCH (h)-[:HAS_CERT]->(c:Certificate) \
               RETURN h.hostname, c.not_after",
    },
    SpikeQuery {
        name: "distinct-skip-limit",
        category: Category::Read,
        text: "MATCH (s:Software) RETURN DISTINCT s.name SKIP 5 LIMIT 10",
    },
    // --- Level R: WITH, aggregation, ordering --------------------------
    SpikeQuery {
        name: "with-aggregate-having",
        category: Category::Read,
        text: "MATCH (v:Supplier)<-[:SUPPLIED_BY]-(s:Software) \
               WITH v, count(s) AS n WHERE n > 3 \
               RETURN v.name, n ORDER BY n DESC",
    },
    SpikeQuery {
        name: "aggregations",
        category: Category::Read,
        text: "MATCH (h:Host)-[:RUNS]->(s:Software) \
               RETURN count(*) AS total, count(DISTINCT s.name) AS names, \
                      sum(s.size_kb) AS kb, avg(s.size_kb) AS avg_kb, \
                      min(s.version) AS lo, max(s.version) AS hi, \
                      collect(s.name) AS all_names",
    },
    SpikeQuery {
        name: "unwind",
        category: Category::Read,
        text: "UNWIND [1, 2, 3] AS x RETURN x * 2 AS y",
    },
    SpikeQuery {
        name: "unwind-parameter",
        category: Category::Read,
        text: "UNWIND $hostnames AS name MATCH (h:Host {hostname: name}) RETURN h",
    },
    // --- Level R: expression language ----------------------------------
    SpikeQuery {
        name: "case-expression",
        category: Category::Read,
        text: "MATCH (c:Certificate) \
               RETURN c.cn, \
                      CASE WHEN c.not_after < $today THEN 'expired' \
                           WHEN c.not_after < $soon THEN 'expiring' \
                           ELSE 'ok' END AS status",
    },
    SpikeQuery {
        name: "string-list-functions",
        category: Category::Read,
        text: "RETURN size([1, 2, 3]) AS n, toUpper('abc') AS u, \
                      substring('hello', 1, 3) AS s, split('a,b,c', ',') AS parts",
    },
    SpikeQuery {
        name: "null-semantics-expressions",
        category: Category::Read,
        text: "RETURN null = null AS eq, null IS NULL AS isn, \
                      null + 1 AS arith, coalesce(null, 'x') AS co",
    },
    SpikeQuery {
        name: "boolean-precedence-in",
        category: Category::Read,
        text: "MATCH (h:Host) \
               WHERE NOT h.decommissioned AND \
                     (h.os IN ['debian', 'ubuntu'] OR h.criticality >= 3) \
               RETURN h",
    },
    SpikeQuery {
        name: "list-comprehension-map-literal",
        category: Category::Read,
        text: "RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x] AS evens, \
                      {name: 'web-01', tags: ['prod', 'dmz']} AS m",
    },
    SpikeQuery {
        name: "list-index-slice",
        category: Category::Read,
        text: "WITH [1, 2, 3, 4] AS xs RETURN xs[0] AS first_elem, xs[1..3] AS mid",
    },
    // --- Level R: patterns ---------------------------------------------
    SpikeQuery {
        name: "pattern-predicate",
        category: Category::Read,
        text: "MATCH (h:Host) \
               WHERE (h)-[:RUNS]->(:Software {name: 'openssl'}) \
               RETURN h.hostname",
    },
    SpikeQuery {
        name: "var-length-bounded",
        category: Category::Read,
        text: "MATCH (a:Software)-[:DEPENDS_ON*1..3]->(b:Software) \
               RETURN a.name, b.name",
    },
    SpikeQuery {
        name: "named-path",
        category: Category::Read,
        text: "MATCH p = (a:Software)-[:DEPENDS_ON*]->(b:Software {name: 'zlib'}) \
               RETURN length(p) AS hops",
    },
    SpikeQuery {
        name: "multi-part-pattern",
        category: Category::Read,
        text: "MATCH (h:Host)-[:RUNS]->(s:Software), (s)-[:SUPPLIED_BY]->(v:Supplier) \
               WHERE v.country = 'DE' RETURN h.hostname, s.name, v.name",
    },
    // --- Registry queries from the roadmap exit criteria ----------------
    SpikeQuery {
        name: "registry-cert-expiry-sweep",
        category: Category::Read,
        text: "MATCH (h:Host)-[:HAS_CERT]->(c:Certificate) \
               WHERE c.not_after < $deadline AND NOT h.decommissioned \
               RETURN h.hostname, c.cn, c.not_after \
               ORDER BY c.not_after ASC LIMIT 100",
    },
    SpikeQuery {
        name: "registry-supply-chain-blast-radius",
        category: Category::Read,
        text: "MATCH (v:Supplier {name: $vendor})<-[:SUPPLIED_BY]-(s:Software)\
               <-[:DEPENDS_ON*0..4]-(top:Software)<-[:RUNS]-(h:Host) \
               RETURN DISTINCT h.hostname, top.name ORDER BY h.hostname",
    },
    SpikeQuery {
        name: "registry-orphaned-software",
        category: Category::Read,
        text: "MATCH (s:Software) WHERE NOT (s)<-[:RUNS]-(:Host) \
               RETURN s.name, s.version",
    },
    // --- Level W (parse now, execute in Phase 3) ------------------------
    SpikeQuery {
        name: "create-node-edge",
        category: Category::Write,
        text: "CREATE (h:Host {hostname: 'web-02', os: 'debian'})\
               -[:RUNS {since: 2024}]->(s:Software {name: 'nginx', version: '1.24'})",
    },
    SpikeQuery {
        name: "merge-on-create-on-match",
        category: Category::Write,
        text: "MERGE (h:Host {hostname: $name}) \
               ON CREATE SET h.first_seen = $now \
               ON MATCH SET h.last_seen = $now",
    },
    SpikeQuery {
        name: "set-remove",
        category: Category::Write,
        text: "MATCH (h:Host {hostname: 'web-01'}) \
               SET h.criticality = 4, h:Critical REMOVE h.legacy_id",
    },
    SpikeQuery {
        name: "delete-detach-delete",
        category: Category::Write,
        text: "MATCH (s:Software {name: 'leftpad'}) DETACH DELETE s",
    },
    // --- Acetone extensions (spec §5.2) ---------------------------------
    SpikeQuery {
        name: "at-ref-time-travel",
        category: Category::Extension,
        text: "MATCH (n:Host) AT 'main~5' RETURN n",
    },
    SpikeQuery {
        name: "at-ref-with-where",
        category: Category::Extension,
        text: "MATCH (h:Host)-[:RUNS]->(s:Software) AT 'release/1.2' \
               WHERE s.name = 'openssl' RETURN h.hostname, s.version",
    },
    // --- Procedures (standard CALL syntax, spec §5.2) --------------------
    SpikeQuery {
        name: "call-log",
        category: Category::Procedure,
        text: "CALL acetone.log('main')",
    },
    SpikeQuery {
        name: "call-diff-yield",
        category: Category::Procedure,
        text: "CALL acetone.diff('main~1', 'main') \
               YIELD kind, label, key \
               WHERE kind = 'node_modified' RETURN label, key",
    },
    // --- Invalid inputs: error quality and recovery ---------------------
    SpikeQuery {
        name: "invalid-unclosed-paren",
        category: Category::Invalid,
        text: "MATCH (n:Host RETURN n",
    },
    SpikeQuery {
        name: "invalid-trailing-operator",
        category: Category::Invalid,
        text: "MATCH (n:Host) WHERE n.x > RETURN n",
    },
    SpikeQuery {
        name: "invalid-bad-keyword",
        category: Category::Invalid,
        text: "MATCHX (n) RETURN n",
    },
];
