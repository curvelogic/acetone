//! `lab <repo> [--scale N]`: generate the asset-registry lab graph and run
//! the registry query suite, reporting row counts and wall-clock latency
//! per query — the Phase 2 interactive-latency evidence (bead
//! acetone-yzc.8).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use acetone_cypher::bind::{BindMode, bind};
use acetone_cypher::exec::{GraphSnapshot, catalogue_from_schema, execute};
use acetone_cypher::session::Session;
use acetone_graph::{InitOptions, Repository};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut repo_path: Option<PathBuf> = None;
    let mut scale = 50_000usize;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scale" => match args.next().and_then(|s| s.parse().ok()) {
                Some(n) => scale = n,
                None => return usage("--scale needs a positive integer"),
            },
            other if !other.starts_with('-') && repo_path.is_none() => {
                repo_path = Some(PathBuf::from(other));
            }
            other => return usage(&format!("unexpected argument {other:?}")),
        }
    }
    let Some(repo_path) = repo_path else {
        return usage("a repository path is required");
    };

    match run(&repo_path, scale) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lab: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(repo_path: &std::path::Path, scale: usize) -> Result<(), Box<dyn std::error::Error>> {
    let shape = acetone_lab::Shape::from_scale(scale);
    println!(
        "Generating lab graph: {} hosts, {} software, {} suppliers, {} certificates ({} nodes)…",
        shape.hosts,
        shape.software,
        shape.suppliers,
        shape.certificates,
        shape.nodes()
    );

    let repo = Repository::init(repo_path, InitOptions::default())?;
    let build_start = Instant::now();
    let (nodes, edges) = acetone_lab::build(&repo, shape)?;
    println!(
        "Built and committed {nodes} nodes, {edges} edges in {:.2}s.\n",
        build_start.elapsed().as_secs_f64()
    );

    // Read the committed graph once into a query snapshot.
    let snapshot = repo.workspace_snapshot()?;
    let read_start = Instant::now();
    let node_records = snapshot.nodes()?;
    let edge_records = snapshot.edges()?;
    let schema = snapshot.schema_entries()?;
    let graph = GraphSnapshot::from_records_with_schema(&node_records, &edge_records, &schema);
    let catalogue = catalogue_from_schema(schema.clone());
    println!(
        "Loaded {} nodes / {} edges into the query engine in {:.2}s.\n",
        graph.node_count(),
        graph.rel_count(),
        read_start.elapsed().as_secs_f64()
    );

    let params = BTreeMap::new();
    println!("Registry queries (Strict binding against the declared schema):");
    for (name, cypher) in acetone_lab::registry_queries() {
        let parsed = acetone_cypher::parse(cypher)?;
        // Strict: the lab graph declares a full schema, so unknown labels
        // or properties would be caught at bind time.
        let bound = bind(cypher, &parsed, &catalogue, BindMode::Strict)?;
        let start = Instant::now();
        // At larger envelopes the heaviest joins can trip the *default*
        // governor caps — report that honestly, then re-run unbounded so
        // the latency evidence survives (the lab is exactly the trusted
        // operator QueryLimits::unbounded documents). Any error OTHER
        // than a resource cap is a genuine defect and fails the run.
        match execute(&bound, &graph, &params) {
            Ok(result) => {
                let elapsed = start.elapsed();
                println!(
                    "  {name:<48} {:>7} rows   {:>8.2} ms",
                    result.rows.len(),
                    elapsed.as_secs_f64() * 1000.0
                );
            }
            Err(acetone_cypher::exec::ExecError::ResourceExceeded { limit, .. }) => {
                let start = Instant::now();
                let result = acetone_cypher::exec::execute_with_limits(
                    &bound,
                    &graph,
                    &params,
                    &acetone_cypher::exec::QueryLimits::unbounded(),
                )?;
                let elapsed = start.elapsed();
                println!(
                    "  {name:<48} {:>7} rows   {:>8.2} ms  (trips the default {limit} cap; unbounded run)",
                    result.rows.len(),
                    elapsed.as_secs_f64() * 1000.0
                );
            }
            Err(e) => return Err(e.into()),
        }
    }

    index_vs_scan_demo(&graph, &node_records, &edge_records, &schema, &params)?;

    // An identical graph WITHOUT the secondary indexes, so seek-versus-scan
    // can be measured on the shipped read path rather than between two
    // schemas over one in-memory snapshot (acetone-2ck.16).
    let plain_path = repo_path.with_file_name(format!(
        "{}-unindexed",
        repo_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "lab".to_string())
    ));
    println!(
        "\nBuilding an identical unindexed twin at {} for the shipped-path comparison…",
        plain_path.display()
    );
    let plain = Repository::init(&plain_path, InitOptions::default())?;
    let twin_start = Instant::now();
    let (twin_nodes, twin_edges) = acetone_lab::build_with(&plain, shape, false)?;
    println!(
        "Built {twin_nodes} nodes, {twin_edges} edges in {:.2}s.",
        twin_start.elapsed().as_secs_f64()
    );
    // The comparison is only meaningful if the twins hold the same graph.
    assert_eq!(
        (twin_nodes, twin_edges),
        (nodes, edges),
        "the unindexed twin must hold the identical graph"
    );

    criterion_3_measurements(&repo, &plain, shape)?;
    Ok(())
}

/// Demonstrate IndexSeek acceleration (acetone-6g5.3.2) against the
/// **in-memory** `GraphSnapshot`: the same pinned equality on the indexed
/// `Host.os`, served by an IndexSeek versus a LabelScan+filter. Best of
/// several runs each.
///
/// This measures the in-memory source, where a seek costs a vector lookup
/// and pays nothing for a point read, so its speed-up is an upper bound
/// rather than what a user sees. `criterion_3_measurements` is the
/// shipped-path number (`acetone-2ck.16`).
fn index_vs_scan_demo(
    indexed: &GraphSnapshot,
    node_records: &[(
        acetone_model::graph_keys::NodeKey,
        acetone_model::records::NodeRecord,
    )],
    edge_records: &[(
        acetone_model::graph_keys::EdgeKey,
        acetone_model::records::EdgeRecord,
    )],
    schema: &[acetone_model::schema::SchemaEntry],
    params: &BTreeMap<String, acetone_cypher::exec::Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    use acetone_model::schema::SchemaEntry;

    let cypher = "MATCH (h:Host {os: 'debian'}) RETURN count(*) AS n";
    let parsed = acetone_cypher::parse(cypher)?;

    // Indexed: bind against the full schema (emits an IndexSeek hint) and run
    // over the loaded snapshot (which has the value index).
    let cat_indexed = catalogue_from_schema(schema.to_vec());
    let bound_indexed = bind(cypher, &parsed, &cat_indexed, BindMode::Strict)?;

    // Scan: the same graph and query with the index removed from the schema,
    // so the binder emits no hint and the adapter builds no value index.
    let schema_no_index: Vec<SchemaEntry> = schema
        .iter()
        .filter(|e| !matches!(e, SchemaEntry::Index { .. }))
        .cloned()
        .collect();
    let scan_graph =
        GraphSnapshot::from_records_with_schema(node_records, edge_records, &schema_no_index);
    let cat_scan = catalogue_from_schema(schema_no_index);
    let bound_scan = bind(cypher, &parsed, &cat_scan, BindMode::Strict)?;

    let best = |bound: &_, graph: &GraphSnapshot| -> Result<f64, Box<dyn std::error::Error>> {
        let mut best = f64::INFINITY;
        for _ in 0..7 {
            let start = Instant::now();
            let _ = execute(bound, graph, params)?;
            best = best.min(start.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(best)
    };

    let seek_ms = best(&bound_indexed, indexed)?;
    let scan_ms = best(&bound_scan, &scan_graph)?;
    println!("\nIndex acceleration, IN-MEMORY source ({cypher}):");
    println!("  IndexSeek (host_os):      {seek_ms:>8.3} ms");
    println!("  LabelScan + filter:       {scan_ms:>8.3} ms");
    if seek_ms > 0.0 {
        println!("  speedup:                  {:>8.1}x", scan_ms / seek_ms);
    }

    Ok(())
}

/// One criterion-3 case: what it measures and why its selectivity matters.
struct SeekCase {
    name: &'static str,
    cypher: String,
    /// What the case is meant to show, printed alongside the result so a
    /// reader can tell a designed decline from a disappointing win.
    expectation: &'static str,
}

/// Phase 9 criterion 3, measured on the **shipped read path**
/// (`acetone-2ck.16`).
///
/// The previous version of this ran every case against a `GraphSnapshot`
/// built in memory, where a "seek" is a vector lookup that costs nothing —
/// so any selectivity looked like a win and the published speed-ups
/// (13.8x range, 27.6x composite) were artefacts of the harness. On a real
/// store a seek does one **random point read per matching row** against a
/// **sequential** scan, so it wins only while selective.
///
/// This compares two real repositories built from the identical
/// deterministic generator, differing only in whether the secondary indexes
/// are declared, queried through `Session` — the same path the CLI uses.
/// Runs are **interleaved**: non-interleaved timing on this codebase once
/// invented a 2x difference that vanished entirely under interleaving.
fn criterion_3_measurements(
    indexed: &Repository,
    plain: &Repository,
    shape: acetone_lab::Shape,
) -> Result<(), Box<dyn std::error::Error>> {
    let certificates = shape.certificates;
    let hosts = shape.hosts;

    // Selectivities are stated against the generator's own distributions:
    // `not_after` is `i % 365`, `os` is `i % 5` and `criticality` is
    // `i % 7` (decorrelated, so an (os, criticality) bucket is ~1/35).
    let cases = vec![
        SeekCase {
            name: "IndexRange, expiring tomorrow (0.27%)",
            cypher: "MATCH (c:Certificate) WHERE c.not_after < 1 RETURN count(*) AS n".into(),
            expectation: "selective: seek should win",
        },
        SeekCase {
            name: "IndexRange, expiring this week (1.9%)",
            cypher: "MATCH (c:Certificate) WHERE c.not_after < 7 RETURN count(*) AS n".into(),
            expectation: "near break-even",
        },
        SeekCase {
            name: "IndexRange, expiring in 30 days (8.2%)",
            // The lab's ORIGINAL range case, kept deliberately. It reported
            // 13.8x against the in-memory source; on the store it is
            // genuinely scan-shaped, and the point of keeping it is to show
            // the cost model DECLINING to parity rather than losing 37x.
            cypher: "MATCH (c:Certificate) WHERE c.not_after < 30 RETURN count(*) AS n".into(),
            expectation: "unselective: should decline to ~parity",
        },
        SeekCase {
            name: "KeySeek, one host by primary key",
            // Derived from the shape so the key EXISTS at every scale: a
            // missing key makes the hinted side fall back to a scan and
            // silently measures scan-vs-scan (PR #209 review).
            cypher: format!(
                "MATCH (h:Host {{hostname: 'host-{}'}}) RETURN count(*) AS n",
                hosts.saturating_sub(1)
            ),
            expectation: "point lookup: seek should win outright",
        },
        SeekCase {
            name: "Composite seek, empty bucket",
            // i%5==0 and i%7==6 first coincide at i=20, so an in-range
            // pair is populated at any real scale; use an out-of-range
            // criticality for a structurally empty bucket.
            cypher: "MATCH (h:Host {os: 'debian', criticality: 9}) RETURN count(*) AS n".into(),
            expectation: "absence proof: seek should win outright",
        },
        SeekCase {
            name: "Composite seek, populated bucket (2.9%)",
            // The lab's ORIGINAL composite case, also kept: it reported
            // 27.6x in memory and 3.7x SLOWER on the store before the cost
            // model existed.
            cypher: "MATCH (h:Host {os: 'debian', criticality: 0}) RETURN count(*) AS n".into(),
            expectation: "unselective: should decline to ~parity",
        },
        SeekCase {
            name: "IndexSeek equality on os (20%)",
            cypher: "MATCH (h:Host {os: 'debian'}) RETURN count(*) AS n".into(),
            expectation: "unselective: should decline to ~parity",
        },
        SeekCase {
            name: "WHERE equality on os (20%)",
            // acetone-7qw.9: this form did not reach an index at all before
            // PR #224, so indexed and unindexed were identical.
            cypher: "MATCH (h:Host) WHERE h.os = 'debian' RETURN count(*) AS n".into(),
            expectation: "unselective: should decline to ~parity",
        },
    ];

    // The planner's own inputs, printed because they are not stable across
    // rebuilds: the cardinality estimator is sampled, and measurably
    // bimodal on skewed trees (PR #224 review). A lab timing without the
    // estimate beside it is not comparable run to run.
    let snapshot = indexed.workspace_snapshot()?;
    let estimate = snapshot.estimate_nodes();
    let true_nodes = shape.nodes();
    println!("\nPhase 9 criterion 3 — seek vs scan on the SHIPPED path (Session):");
    match estimate {
        Some(rows) => println!(
            "  planner inputs: nodes estimated {rows} (true {true_nodes}, ratio {:.2}), \
             candidate budget {}",
            rows as f64 / true_nodes as f64,
            acetone_cypher::exec::store_source::candidate_cap(rows)
        ),
        None => println!("  planner inputs: nodes map could not be sampled"),
    }
    println!(
        "  {certificates} certificates, {hosts} hosts; indexed vs an identical \
         unindexed repository, interleaved, best of {RUNS}"
    );

    for case in &cases {
        let (indexed_ms, plain_ms) = interleaved(indexed, plain, &case.cypher)?;
        let ratio = if indexed_ms > 0.0 {
            plain_ms / indexed_ms
        } else {
            f64::INFINITY
        };
        // A ratio ABOVE 1 is a speed-up; below 1 the index cost us.
        println!(
            "  {:<40} indexed {indexed_ms:>8.2} ms   unindexed {plain_ms:>8.2} ms   {:>7}   ({})",
            case.name,
            format!("{ratio:.2}x"),
            case.expectation
        );
    }
    Ok(())
}

/// Runs to take the best of. The minimum is the right statistic here: it is
/// the run least disturbed by other work on the machine.
const RUNS: usize = 7;

/// Time `cypher` against both repositories, **alternating** them so machine
/// drift lands on both sides, and assert they return the same rows.
///
/// The parity assertion is the load-bearing part: an "acceleration" that
/// returned different rows would otherwise be reported as a speed-up.
fn interleaved(
    indexed: &Repository,
    plain: &Repository,
    cypher: &str,
) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let (a, b) = (Session::new(indexed), Session::new(plain));
    let (mut best_indexed, mut best_plain) = (f64::INFINITY, f64::INFINITY);
    let (mut rows_indexed, mut rows_plain) = (String::new(), String::new());
    for _ in 0..RUNS {
        let start = Instant::now();
        let out = a.run(cypher)?;
        best_indexed = best_indexed.min(start.elapsed().as_secs_f64() * 1000.0);
        rows_indexed = format!("{out:?}");

        let start = Instant::now();
        let out = b.run(cypher)?;
        best_plain = best_plain.min(start.elapsed().as_secs_f64() * 1000.0);
        rows_plain = format!("{out:?}");
    }
    assert_eq!(
        rows_indexed, rows_plain,
        "indexed and unindexed must agree for: {cypher}"
    );
    Ok((best_indexed, best_plain))
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("{problem}\nusage: lab <repo> [--scale N]");
    ExitCode::FAILURE
}
