//! The binder (spec §5.3): resolves a parsed query's names against the
//! schema catalogue, validates scoping and aggregation placement, and
//! lowers the AST to the bound IR the planner consumes.

pub mod binder;
pub mod bound;
pub mod catalogue;
pub mod error;

pub use binder::{BindMode, bind};
pub use bound::{BoundClause, BoundExpr, BoundQuery, EntityKind, IndexHint, VarId};
pub use catalogue::Catalogue;
pub use error::BindError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
    use std::collections::BTreeMap;

    fn bind_lenient(query: &str) -> Result<BoundQuery, BindError> {
        let parsed = parse(query).expect("query must parse");
        bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient)
    }

    fn host_catalogue() -> Catalogue {
        let mut types = BTreeMap::new();
        types.insert(
            "hostname".to_string(),
            acetone_model::schema::PropertyType::String,
        );
        types.insert(
            "os".to_string(),
            acetone_model::schema::PropertyType::String,
        );
        let label = LabelDef::new(vec!["hostname".to_string()], types, [], []).unwrap();
        Catalogue::from_entries([
            SchemaEntry::Label {
                name: "Host".into(),
                def: label,
            },
            SchemaEntry::Index {
                name: "host_os".into(),
                def: IndexDef::new("Host", vec!["os".into()]).unwrap(),
            },
        ])
    }

    fn bind_strict(query: &str) -> Result<BoundQuery, BindError> {
        let parsed = parse(query).expect("query must parse");
        bind(query, &parsed, &host_catalogue(), BindMode::Strict)
    }

    #[test]
    fn resolves_variables_across_clauses() {
        let bound = bind_lenient("MATCH (n:Host) WHERE n.up RETURN n.hostname AS h").unwrap();
        assert_eq!(bound.variables[0].name, "n");
        assert_eq!(bound.variables[0].kind, EntityKind::Node);
    }

    #[test]
    fn undefined_variable_is_precise() {
        let err = bind_lenient("MATCH (n) RETURN m").unwrap_err();
        let BindError::UndefinedVariable { name, span } = &err else {
            panic!("wrong error: {err}");
        };
        assert_eq!(name, "m");
        assert_eq!(span.start, 17);
        assert_eq!(err.tck_detail(), Some("UndefinedVariable"));
    }

    #[test]
    fn with_rescopes_exactly_to_projected_names() {
        assert!(bind_lenient("MATCH (n), (m) WITH n RETURN n").is_ok());
        let err = bind_lenient("MATCH (n), (m) WITH n RETURN m").unwrap_err();
        assert!(matches!(err, BindError::UndefinedVariable { .. }));
        // ...but WITH's own WHERE still sees the pre-projection scope
        // (TCK WithWhere7: "WHERE sees a variable bound before but not
        // after WITH").
        assert!(bind_lenient("MATCH (a) WITH a.x AS x WHERE a.y = 1 RETURN x").is_ok());
        // A plain-variable projection keeps its entity kind for
        // re-matching.
        assert!(bind_lenient("MATCH (n)-->(m) WITH n AS q MATCH (q)-->(z) RETURN z").is_ok());
    }

    #[test]
    fn with_requires_aliases_for_expressions() {
        assert!(bind_lenient("MATCH (n) WITH n.x AS x RETURN x").is_ok());
        let err = bind_lenient("MATCH (n) WITH n.x RETURN 1").unwrap_err();
        assert!(matches!(err, BindError::NoExpressionAlias { .. }));
    }

    #[test]
    fn return_derives_column_names_from_source() {
        let bound = bind_lenient("MATCH (n) RETURN n.hostname").unwrap();
        let BoundClause::Return(p) = &bound.clauses[1] else {
            panic!()
        };
        assert_eq!(p.items[0].name, "n.hostname");
    }

    #[test]
    fn duplicate_columns_are_rejected() {
        let err = bind_lenient("MATCH (n) RETURN n.x AS a, n.y AS a").unwrap_err();
        assert!(matches!(err, BindError::ColumnNameConflict { .. }));
    }

    #[test]
    fn variable_reuse_follows_opencypher() {
        // Bound relationship variables may reappear in later patterns as
        // equality constraints (TCK Match4/Match7); node reuse is a join.
        assert!(bind_lenient("MATCH (a)-[r]->(b) MATCH (c)-[r]->(d) RETURN r").is_ok());
        assert!(bind_lenient("MATCH (a)-->(b) MATCH (b)-->(c) RETURN a, c").is_ok());
        // Projected values may stand in node/relationship positions
        // (dynamic typing: UNWIND elements, coalesce results).
        assert!(bind_lenient("MATCH (a)-[r]->(b) WITH collect(b) AS bs UNWIND bs AS x                               MATCH (x)-->(y) RETURN y")
            .is_ok());
        // Paths rebind nowhere.
        let err = bind_lenient("MATCH p = (a)-->(b) MATCH p = (c)-->(d) RETURN p").unwrap_err();
        assert!(matches!(err, BindError::VariableAlreadyBound { .. }));
    }

    #[test]
    fn kind_conflicts_are_caught() {
        // Relationship variable in node position (TCK Match1 [9]).
        let err = bind_lenient("MATCH ()-[r]-(r) RETURN r").unwrap_err();
        assert!(matches!(err, BindError::VariableTypeConflict { .. }));
        let err = bind_lenient("MATCH (a)-[r]->(b) MATCH (r) RETURN r").unwrap_err();
        assert!(matches!(err, BindError::VariableTypeConflict { .. }));
        // Node variable in relationship position.
        let err = bind_lenient("MATCH (a)-->(b) MATCH ()-[a]->() RETURN a").unwrap_err();
        assert!(matches!(err, BindError::VariableTypeConflict { .. }));
        // Path variable reused anywhere is AlreadyBound (TCK Match6 [24]).
        let err = bind_lenient("MATCH p = (a)-->(b) MATCH ()-[p]->() RETURN p").unwrap_err();
        assert!(matches!(err, BindError::VariableAlreadyBound { .. }));
    }

    #[test]
    fn var_length_binds_a_relationship_list() {
        let bound = bind_lenient("MATCH (a)-[rs:R*1..3]->(b) RETURN rs").unwrap();
        let rs = bound.variables.iter().find(|v| v.name == "rs").unwrap();
        assert_eq!(rs.kind, EntityKind::RelationshipList);
    }

    #[test]
    fn aggregation_placement_is_validated() {
        assert!(bind_lenient("MATCH (n) RETURN count(n)").is_ok());
        assert!(bind_lenient("MATCH (n) RETURN count(*)").is_ok());
        let err = bind_lenient("MATCH (n) WHERE count(n) > 0 RETURN n").unwrap_err();
        assert!(matches!(err, BindError::InvalidAggregation { .. }));
        let err = bind_lenient("MATCH (n) RETURN sum(count(n))").unwrap_err();
        assert!(matches!(err, BindError::NestedAggregation { .. }));
        let err = bind_lenient("UNWIND count(x) AS y RETURN y").unwrap_err();
        assert!(matches!(err, BindError::InvalidAggregation { .. }));
    }

    #[test]
    fn grouping_keys_are_recorded() {
        let bound = bind_lenient("MATCH (n) RETURN n.dept AS d, count(n) AS c").unwrap();
        let BoundClause::Return(p) = &bound.clauses[1] else {
            panic!()
        };
        assert!(p.aggregating);
        assert_eq!(p.grouping_items, vec![0]);
    }

    #[test]
    fn unknown_function_is_an_error_in_both_modes() {
        let err = bind_lenient("RETURN frobnicate(1)").unwrap_err();
        assert_eq!(err.tck_detail(), Some("UnknownFunction"));
        let err = bind_strict("RETURN frobnicate(1)").unwrap_err();
        assert!(matches!(err, BindError::UnknownFunction { .. }));
    }

    #[test]
    fn function_arity_is_checked() {
        let err = bind_lenient("RETURN size()").unwrap_err();
        assert!(matches!(err, BindError::InvalidNumberOfArguments { .. }));
        let err = bind_lenient("RETURN substring('a', 1, 2, 3)").unwrap_err();
        assert!(matches!(err, BindError::InvalidNumberOfArguments { .. }));
        assert!(bind_lenient("RETURN coalesce(1, 2, 3, 4, 5)").is_ok());
    }

    #[test]
    fn distinct_and_star_are_aggregate_only() {
        let err = bind_lenient("RETURN size(DISTINCT [1])").unwrap_err();
        assert!(matches!(err, BindError::InvalidAggregation { .. }));
        let err = bind_lenient("RETURN sum(*)").unwrap_err();
        assert!(matches!(err, BindError::InvalidNumberOfArguments { .. }));
    }

    #[test]
    fn strict_mode_enforces_the_catalogue() {
        // Known label and property: fine.
        assert!(bind_strict("MATCH (h:Host {hostname: 'a'}) RETURN h").is_ok());
        // Unknown label.
        let err = bind_strict("MATCH (x:Rogue) RETURN x").unwrap_err();
        assert!(matches!(err, BindError::UnknownLabel { .. }));
        // Unknown relationship type.
        let err = bind_strict("MATCH (h:Host)-[:GLUES]->(x:Host) RETURN h").unwrap_err();
        assert!(matches!(err, BindError::UnknownRelType { .. }));
        // Undeclared property on a shaped label.
        let err = bind_strict("MATCH (h:Host {shoe_size: 9}) RETURN h").unwrap_err();
        assert!(matches!(err, BindError::UnknownProperty { .. }));
        // Lenient mode accepts all of these.
        assert!(bind_lenient("MATCH (x:Rogue {shoe_size: 9})-[:GLUES]->(y) RETURN x").is_ok());
    }

    fn suggestion_of(err: &BindError) -> Option<String> {
        match err {
            BindError::UnknownLabel { suggestion, .. }
            | BindError::UnknownRelType { suggestion, .. }
            | BindError::UnknownProperty { suggestion, .. }
            | BindError::UnknownFunction { suggestion, .. } => suggestion.0.clone(),
            other => panic!("not a suggestion-bearing error: {other}"),
        }
    }

    /// A catalogue with a label, a typed property and a relationship type, so
    /// every "unknown X" suggestion has a candidate to match against.
    fn suggestion_catalogue() -> Catalogue {
        let mut types = BTreeMap::new();
        types.insert(
            "hostname".to_string(),
            acetone_model::schema::PropertyType::String,
        );
        let label = LabelDef::new(vec!["hostname".to_string()], types, [], []).unwrap();
        Catalogue::from_entries([
            SchemaEntry::Label {
                name: "Host".into(),
                def: label,
            },
            SchemaEntry::RelType {
                name: "RUNS".into(),
                def: acetone_model::schema::RelTypeDef::new(None, BTreeMap::new(), []).unwrap(),
            },
        ])
    }

    fn bind_suggest(query: &str) -> BindError {
        let parsed = parse(query).expect("query must parse");
        bind(query, &parsed, &suggestion_catalogue(), BindMode::Strict).unwrap_err()
    }

    #[test]
    fn near_typos_get_a_did_you_mean_and_nonsense_does_not() {
        // Label: a one-edit typo suggests the declared label; nonsense does not.
        let err = bind_suggest("MATCH (x:Hst) RETURN x");
        assert!(matches!(err, BindError::UnknownLabel { .. }));
        assert_eq!(suggestion_of(&err), Some("Host".to_owned()));
        assert!(err.to_string().contains("did you mean \"Host\"?"));
        assert_eq!(
            suggestion_of(&bind_suggest("MATCH (x:Zzzzzz) RETURN x")),
            None
        );

        // Relationship type.
        let err = bind_suggest("MATCH (h:Host)-[:RUNZ]->(x) RETURN h");
        assert!(matches!(err, BindError::UnknownRelType { .. }));
        assert_eq!(suggestion_of(&err), Some("RUNS".to_owned()));
        assert_eq!(
            suggestion_of(&bind_suggest("MATCH (h:Host)-[:ZZZZZ]->(x) RETURN h")),
            None
        );

        // Property on a shaped label.
        let err = bind_suggest("MATCH (h:Host {hstname: 'a'}) RETURN h");
        assert!(matches!(err, BindError::UnknownProperty { .. }));
        assert_eq!(suggestion_of(&err), Some("hostname".to_owned()));
        assert_eq!(
            suggestion_of(&bind_suggest("MATCH (h:Host {zzzzzzz: 1}) RETURN h")),
            None
        );

        // Function (mode-independent).
        let err = bind_suggest("RETURN toUppr('a')");
        assert!(matches!(err, BindError::UnknownFunction { .. }));
        assert_eq!(suggestion_of(&err), Some("toUpper".to_owned()));
        assert_eq!(suggestion_of(&bind_suggest("RETURN frobnicate(1)")), None);
    }

    #[test]
    fn suggested_rel_type_command_escapes_the_typed_name() {
        // The relationship type is echoed into a copy-pasteable
        // `acetone declare-rel-type …` command; a backtick-quoted
        // control-character name must be escaped there so the suggested
        // command carries no raw control bytes.
        let err = bind_suggest("MATCH (h:Host)-[:`ev\u{1b}il`]->(x) RETURN h");
        let rendered = err.to_string();
        let command = &rendered[rendered.find("declare-rel-type").expect("command present")..];
        assert!(
            !command.contains('\u{1b}'),
            "escape leaked into suggested command: {command:?}"
        );
        assert!(command.contains("declare-rel-type \"ev"));
    }

    #[test]
    fn index_hints_cover_key_and_secondary_indexes() {
        let bound = bind_strict("MATCH (h:Host {hostname: 'web-01'}) RETURN h").unwrap();
        let BoundClause::Match { patterns, .. } = &bound.clauses[0] else {
            panic!()
        };
        assert_eq!(
            patterns[0].start.index_hint,
            Some(IndexHint::KeySeek {
                label: "Host".into()
            })
        );

        let bound = bind_strict("MATCH (h:Host {os: 'debian'}) RETURN h").unwrap();
        let BoundClause::Match { patterns, .. } = &bound.clauses[0] else {
            panic!()
        };
        assert_eq!(
            patterns[0].start.index_hint,
            Some(IndexHint::IndexSeek {
                name: "host_os".into(),
                label: "Host".into(),
                property: "os".into()
            })
        );

        // A computed value pins nothing.
        let bound = bind_strict("MATCH (a:Host) MATCH (h:Host {os: a.os}) RETURN h").unwrap();
        let BoundClause::Match { patterns, .. } = &bound.clauses[1] else {
            panic!()
        };
        assert_eq!(patterns[0].start.index_hint, None);
    }

    #[test]
    fn pattern_predicates_cannot_introduce_variables() {
        assert!(bind_lenient("MATCH (h) WHERE (h)--(x) RETURN h").is_err());
        assert!(bind_lenient("MATCH (h) WHERE (h)--() RETURN h").is_ok());
        let err = bind_lenient("MATCH (h) WHERE (h)--(x) RETURN h").unwrap_err();
        assert_eq!(err.tck_detail(), Some("UndefinedVariable"));
        // Already-bound variables are fine.
        assert!(bind_lenient("MATCH (h), (x) WHERE (h)--(x) RETURN h").is_ok());
    }

    #[test]
    fn procedures_bind_against_the_registry() {
        assert!(bind_lenient("CALL acetone.log('main')").is_ok());
        assert!(
            bind_lenient("CALL acetone.diff('a', 'b') YIELD kind, label RETURN kind, label")
                .is_ok()
        );
        let err = bind_lenient("CALL acetone.nope()").unwrap_err();
        assert_eq!(err.tck_detail(), Some("ProcedureNotFound"));
        let err = bind_lenient("CALL acetone.log('a', 'b')").unwrap_err();
        assert!(matches!(err, BindError::InvalidNumberOfArguments { .. }));
        let err = bind_lenient("CALL acetone.diff('a', 'b') YIELD sandwich RETURN 1").unwrap_err();
        assert!(matches!(err, BindError::UnknownYieldColumn { .. }));
        // Duplicate and shadowing YIELD columns are VariableAlreadyBound
        // (TCK Call5 [5][6], Call1 [15]).
        let err =
            bind_lenient("CALL acetone.diff('a', 'b') YIELD kind, kind RETURN 1").unwrap_err();
        assert!(matches!(err, BindError::VariableAlreadyBound { .. }));
        let err = bind_lenient("MATCH (kind) CALL acetone.diff('a', 'b') YIELD kind RETURN kind")
            .unwrap_err();
        assert!(matches!(err, BindError::VariableAlreadyBound { .. }));
    }

    #[test]
    fn unwind_alias_cannot_shadow() {
        let err = bind_lenient("MATCH (n) UNWIND [1] AS n RETURN n").unwrap_err();
        assert!(matches!(err, BindError::VariableAlreadyBound { .. }));
    }

    #[test]
    fn return_star_projects_scope_and_errors_when_empty() {
        let bound = bind_lenient("MATCH (b), (a) RETURN *").unwrap();
        let BoundClause::Return(p) = &bound.clauses[1] else {
            panic!()
        };
        let names: Vec<&str> = p.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        let err = bind_lenient("RETURN *").unwrap_err();
        assert!(matches!(err, BindError::NoVariablesInScope { .. }));
    }

    #[test]
    fn order_by_sees_both_scopes() {
        assert!(bind_lenient("MATCH (n) RETURN n.x AS y ORDER BY y").is_ok());
        assert!(bind_lenient("MATCH (n) RETURN n.x AS y ORDER BY n.z").is_ok());
    }

    #[test]
    fn comprehension_variable_shadows_and_restores() {
        assert!(bind_lenient("MATCH (x) RETURN [x IN [1, 2] | x * 2] AS l, x").is_ok());
        // The comprehension variable does not leak.
        let err = bind_lenient("RETURN [y IN [1, 2] | y] AS l, y").unwrap_err();
        assert!(matches!(err, BindError::UndefinedVariable { .. }));
    }

    #[test]
    fn create_binds_and_introduces_variables() {
        let bound = bind_lenient("CREATE (a:N {v: 1})-[r:R]->(b:N) RETURN a, r, b").unwrap();
        let BoundClause::Create { patterns, .. } = &bound.clauses[0] else {
            panic!("expected CREATE clause");
        };
        assert_eq!(patterns[0].start.labels, vec!["N"]);
        // a is a node, r a relationship, b a node.
        let a = bound.variables.iter().find(|v| v.name == "a").unwrap();
        let r = bound.variables.iter().find(|v| v.name == "r").unwrap();
        assert_eq!(a.kind, EntityKind::Node);
        assert_eq!(r.kind, EntityKind::Relationship);
    }

    #[test]
    fn create_may_reference_a_bound_node() {
        // A bare reference to a bound node is fine...
        assert!(bind_lenient("MATCH (a:A) CREATE (a)-[:R]->(b:B) RETURN b").is_ok());
        // ...but attaching labels or properties to it is not (openCypher:
        // that is a SET, not a CREATE) — it must not silently vanish.
        let err = bind_lenient("MATCH (a:A) CREATE (a:Extra) RETURN a").unwrap_err();
        assert!(matches!(
            err,
            BindError::CreateBoundNodeWithProperties { .. }
        ));
        assert_eq!(err.tck_detail(), Some("VariableAlreadyBound"));
        let err = bind_lenient("MATCH (a:A) CREATE (a {x: 1}) RETURN a").unwrap_err();
        assert!(matches!(
            err,
            BindError::CreateBoundNodeWithProperties { .. }
        ));
    }

    #[test]
    fn create_relationship_rules_are_enforced() {
        // Undirected relationship in CREATE.
        let err = bind_lenient("CREATE (a)-[:R]-(b)").unwrap_err();
        assert_eq!(err.tck_detail(), Some("RequiresDirectedRelationship"));
        // No / multiple types.
        let err = bind_lenient("CREATE (a)-[:R|S]->(b)").unwrap_err();
        assert_eq!(err.tck_detail(), Some("NoSingleRelationshipType"));
        let err = bind_lenient("CREATE (a)-[]->(b)").unwrap_err();
        assert_eq!(err.tck_detail(), Some("NoSingleRelationshipType"));
        // Variable-length in CREATE.
        let err = bind_lenient("CREATE (a)-[:R*2]->(b)").unwrap_err();
        assert_eq!(err.tck_detail(), Some("CreatingVarLength"));
        // Reusing a bound relationship variable to create is an error.
        let err = bind_lenient("MATCH ()-[r]->() CREATE (a)-[r:R]->(b) RETURN a").unwrap_err();
        assert!(matches!(err, BindError::VariableAlreadyBound { .. }));
    }

    #[test]
    fn set_and_remove_bind() {
        assert!(bind_lenient("MATCH (n) SET n.x = 1 RETURN n").is_ok());
        assert!(bind_lenient("MATCH (n) SET n = {a: 1}, n += {b: 2} RETURN n").is_ok());
        assert!(bind_lenient("MATCH (n) SET n:Label RETURN n").is_ok());
        assert!(bind_lenient("MATCH (n) REMOVE n.x, n:Label RETURN n").is_ok());
        assert!(bind_lenient("MATCH ()-[r]->() SET r.w = 1 RETURN r").is_ok());
        // SET target must be in scope.
        let err = bind_lenient("SET x.p = 1").unwrap_err();
        assert!(matches!(err, BindError::UndefinedVariable { .. }));
        // A relationship carries no labels.
        let err = bind_lenient("MATCH ()-[r]->() SET r:Label RETURN r").unwrap_err();
        assert!(matches!(err, BindError::VariableTypeConflict { .. }));
    }

    #[test]
    fn set_rejects_key_property_modification_in_strict_mode() {
        // Host's key is [hostname]; SET/REMOVE of it is rejected (Invariant #3).
        let err = bind_strict("MATCH (h:Host) SET h.hostname = 'x' RETURN h").unwrap_err();
        assert!(matches!(err, BindError::SetKeyProperty { .. }));
        let err = bind_strict("MATCH (h:Host) REMOVE h.hostname RETURN h").unwrap_err();
        assert!(matches!(err, BindError::SetKeyProperty { .. }));
        // Replacing the whole map would wipe the key: also rejected.
        let err = bind_strict("MATCH (h:Host) SET h = {os: 'x'} RETURN h").unwrap_err();
        assert!(matches!(err, BindError::SetKeyProperty { .. }));
        // A `+=` naming the key is rejected; one that does not is fine.
        let err = bind_strict("MATCH (h:Host) SET h += {hostname: 'x'} RETURN h").unwrap_err();
        assert!(matches!(err, BindError::SetKeyProperty { .. }));
        assert!(bind_strict("MATCH (h:Host) SET h.os = 'debian' RETURN h").is_ok());
        assert!(bind_strict("MATCH (h:Host) SET h += {os: 'debian'} RETURN h").is_ok());
        // Lenient mode (schema-free) has no keys, so nothing is rejected.
        assert!(bind_lenient("MATCH (h:Host) SET h.hostname = 'x' RETURN h").is_ok());
    }

    #[test]
    fn merge_binds_pattern_and_on_actions() {
        assert!(
            bind_lenient("MERGE (a:N {k: 1}) ON CREATE SET a.x = 1 ON MATCH SET a.y = 2 RETURN a")
                .is_ok()
        );
        // MERGE pattern obeys CREATE relationship rules (directed).
        let err = bind_lenient("MERGE (a)-[:R]-(b)").unwrap_err();
        assert_eq!(err.tck_detail(), Some("RequiresDirectedRelationship"));
        // ON CREATE SET of a key property is rejected in Strict mode.
        let err =
            bind_strict("MERGE (h:Host {hostname: 'x'}) ON MATCH SET h.hostname = 'y' RETURN h")
                .unwrap_err();
        assert!(matches!(err, BindError::SetKeyProperty { .. }));
    }

    #[test]
    fn gate_b_corpus_binds_lenient() {
        // Every read query from the Gate B representative set must bind
        // under a lenient empty catalogue.
        for query in [
            "MATCH (n:Host) RETURN n",
            "MATCH (h:Host {hostname: 'web-01'}) RETURN h.hostname, h.os AS os",
            "MATCH (v:Supplier)<-[:SUPPLIED_BY]-(s:Software) WITH v, count(s) AS n \
             WHERE n > 3 RETURN v.name, n ORDER BY n DESC",
            "UNWIND $hostnames AS name MATCH (h:Host {hostname: name}) RETURN h",
            "RETURN size([1, 2, 3]) AS n, toUpper('abc') AS u, substring('hello', 1, 3) AS s, \
             split('a,b,c', ',') AS parts",
            "MATCH (h:Host) WHERE NOT h.decommissioned AND \
             (h.os IN ['debian', 'ubuntu'] OR h.criticality >= 3) RETURN h",
            "RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x] AS evens, \
             {name: 'web-01', tags: ['prod', 'dmz']} AS m",
            "MATCH p = (a:Software)-[:DEPENDS_ON*]->(b:Software {name: 'zlib'}) \
             RETURN length(p) AS hops",
            "MATCH (n:Host) AT 'main~5' RETURN n",
            "CALL acetone.diff('main~1', 'main') YIELD kind, label, key \
             WHERE kind = 'node_modified' RETURN label, key",
        ] {
            let parsed = parse(query).expect(query);
            if let Err(e) = bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient) {
                panic!("should bind: {query}\n  error: {e}");
            }
        }
    }
}
