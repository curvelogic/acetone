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
    let catalogue = catalogue_from_schema(schema);
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
    Ok(())
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("{problem}\nusage: lab <repo> [--scale N]");
    ExitCode::FAILURE
}
