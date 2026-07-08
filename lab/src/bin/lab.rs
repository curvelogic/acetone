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
        let result = execute(&bound, &graph, &params)?;
        let elapsed = start.elapsed();
        println!(
            "  {name:<48} {:>7} rows   {:>8.2} ms",
            result.rows.len(),
            elapsed.as_secs_f64() * 1000.0
        );
    }

    index_vs_scan_demo(&graph, &node_records, &edge_records, &schema, &params)?;
    Ok(())
}

/// Demonstrate IndexSeek acceleration (acetone-6g5.3.2): the same pinned
/// equality on the indexed `Host.os`, served by an IndexSeek (the declared
/// `host_os` index) versus a LabelScan+filter (the identical graph with the
/// index removed from the schema). Reports the best of several runs each.
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
    println!("\nIndex acceleration ({cypher}):");
    println!("  IndexSeek (host_os):      {seek_ms:>8.3} ms");
    println!("  LabelScan + filter:       {scan_ms:>8.3} ms");
    if seek_ms > 0.0 {
        println!("  speedup:                  {:>8.1}x", scan_ms / seek_ms);
    }
    Ok(())
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("{problem}\nusage: lab <repo> [--scale N]");
    ExitCode::FAILURE
}
