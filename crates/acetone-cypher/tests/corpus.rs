//! Corpus test: the Gate B representative query set (ADR-0013), Level R
//! plus extensions and procedures, must parse; the invalid inputs must be
//! rejected with in-bounds spans. Level W entries from the spike set are
//! deliberately absent — write clauses arrive with Phase 3.

use acetone_cypher::parse;

const VALID: &[&str] = &[
    // Level R: basic MATCH / RETURN
    "MATCH (n:Host) RETURN n",
    "MATCH (h:Host {hostname: 'web-01'}) RETURN h.hostname, h.os AS os",
    "MATCH (h:Host)-[:RUNS]->(s:Software) WHERE s.version STARTS WITH '2.' \
     RETURN h.hostname, s.name ORDER BY h.hostname LIMIT 10",
    "MATCH (h:Host)-[r:RUNS|HOSTS]-(m) RETURN type(r), m",
    "MATCH (h:Host) OPTIONAL MATCH (h)-[:HAS_CERT]->(c:Certificate) \
     RETURN h.hostname, c.not_after",
    "MATCH (s:Software) RETURN DISTINCT s.name SKIP 5 LIMIT 10",
    // WITH, aggregation, ordering
    "MATCH (v:Supplier)<-[:SUPPLIED_BY]-(s:Software) WITH v, count(s) AS n WHERE n > 3 \
     RETURN v.name, n ORDER BY n DESC",
    "MATCH (h:Host)-[:RUNS]->(s:Software) \
     RETURN count(*) AS total, count(DISTINCT s.name) AS names, sum(s.size_kb) AS kb, \
            avg(s.size_kb) AS avg_kb, min(s.version) AS lo, max(s.version) AS hi, \
            collect(s.name) AS all_names",
    "UNWIND [1, 2, 3] AS x RETURN x * 2 AS y",
    "UNWIND $hostnames AS name MATCH (h:Host {hostname: name}) RETURN h",
    // Expression language
    "MATCH (c:Certificate) RETURN c.cn, \
     CASE WHEN c.not_after < $today THEN 'expired' WHEN c.not_after < $soon THEN 'expiring' \
     ELSE 'ok' END AS status",
    "RETURN size([1, 2, 3]) AS n, toUpper('abc') AS u, substring('hello', 1, 3) AS s, \
     split('a,b,c', ',') AS parts",
    "RETURN null = null AS eq, null IS NULL AS isn, null + 1 AS arith, \
     coalesce(null, 'x') AS co",
    "MATCH (h:Host) WHERE NOT h.decommissioned AND \
     (h.os IN ['debian', 'ubuntu'] OR h.criticality >= 3) RETURN h",
    "RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x] AS evens, \
     {name: 'web-01', tags: ['prod', 'dmz']} AS m",
    "WITH [1, 2, 3, 4] AS xs RETURN xs[0] AS first_elem, xs[1..3] AS mid",
    // Patterns
    "MATCH (h:Host) WHERE (h)-[:RUNS]->(:Software {name: 'openssl'}) RETURN h.hostname",
    "MATCH (a:Software)-[:DEPENDS_ON*1..3]->(b:Software) RETURN a.name, b.name",
    "MATCH p = (a:Software)-[:DEPENDS_ON*]->(b:Software {name: 'zlib'}) \
     RETURN length(p) AS hops",
    "MATCH (h:Host)-[:RUNS]->(s:Software), (s)-[:SUPPLIED_BY]->(v:Supplier) \
     WHERE v.country = 'DE' RETURN h.hostname, s.name, v.name",
    // Registry queries from the roadmap exit criteria
    "MATCH (h:Host)-[:HAS_CERT]->(c:Certificate) \
     WHERE c.not_after < $deadline AND NOT h.decommissioned \
     RETURN h.hostname, c.cn, c.not_after ORDER BY c.not_after ASC LIMIT 100",
    "MATCH (v:Supplier {name: $vendor})<-[:SUPPLIED_BY]-(s:Software)\
     <-[:DEPENDS_ON*0..4]-(top:Software)<-[:RUNS]-(h:Host) \
     RETURN DISTINCT h.hostname, top.name ORDER BY h.hostname",
    "MATCH (s:Software) WHERE NOT (s)<-[:RUNS]-(:Host) RETURN s.name, s.version",
    // Acetone extensions (spec §5.2)
    "MATCH (n:Host) AT 'main~5' RETURN n",
    "MATCH (h:Host)-[:RUNS]->(s:Software) AT 'release/1.2' \
     WHERE s.name = 'openssl' RETURN h.hostname, s.version",
    // Procedures
    "CALL acetone.log('main')",
    "CALL acetone.diff('main~1', 'main') YIELD kind, label, key \
     WHERE kind = 'node_modified' RETURN label, key",
];

const INVALID: &[&str] = &[
    "MATCH (n:Host RETURN n",
    "MATCH (n:Host) WHERE n.x > RETURN n",
    "MATCHX (n) RETURN n",
    "MATCH (n)",
    "RETURN",
    "MATCH (n) RETURN n RETURN n",
    "MATCH (n) WHERE RETURN n",
    "RETURN 'unterminated",
    "RETURN /* open",
];

#[test]
fn corpus_valid_queries_parse() {
    for query in VALID {
        if let Err(e) = parse(query) {
            panic!("should parse: {query}\n  error: {e}");
        }
    }
}

#[test]
fn corpus_invalid_queries_are_rejected_with_in_bounds_spans() {
    for query in INVALID {
        match parse(query) {
            Ok(_) => panic!("should not parse: {query}"),
            Err(e) => assert!(
                e.span().end <= query.len(),
                "span out of bounds for: {query}"
            ),
        }
    }
}
