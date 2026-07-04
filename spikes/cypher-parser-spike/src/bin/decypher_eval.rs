//! Spike A: run the representative query set through decypher and report
//! per-category results, error quality on the invalid inputs, and whether
//! the acetone extension queries are representable at all.

use cypher_parser_spike::queries::{Category, QUERIES};

fn main() {
    let mut ok = 0usize;
    let mut failed: Vec<(&str, Category, String)> = Vec::new();

    println!(
        "== decypher {} against the representative set ==\n",
        version()
    );

    for q in QUERIES {
        match decypher::parse(q.text) {
            Ok(query) => {
                ok += 1;
                let statements = query.statements.len();
                println!(
                    "PASS  [{:?}] {} ({} statement(s))",
                    q.category, q.name, statements
                );
            }
            Err(e) => {
                let msg = format!("{e}");
                println!("FAIL  [{:?}] {}\n      error: {}", q.category, q.name, msg);
                failed.push((q.name, q.category, msg));
            }
        }
    }

    println!("\n== summary: {}/{} parsed ==", ok, QUERIES.len());
    for cat in [
        Category::Read,
        Category::Write,
        Category::Extension,
        Category::Procedure,
        Category::Invalid,
    ] {
        let total = QUERIES.iter().filter(|q| q.category == cat).count();
        let bad = failed.iter().filter(|(_, c, _)| *c == cat).count();
        println!("  {:?}: {}/{} parsed", cat, total - bad, total);
    }

    // Error-recovery probe: does parse_all yield diagnostics plus a partial
    // tree on broken input? That matters for the shell REPL's UX.
    println!("\n== error recovery (parse_all) on invalid inputs ==");
    for q in QUERIES.iter().filter(|q| q.category == Category::Invalid) {
        let (tree, diagnostics) = decypher::parse_all(q.text);
        println!(
            "  {}: partial-tree={} diagnostics={}",
            q.name,
            tree.is_some(),
            diagnostics.len()
        );
        for d in &diagnostics {
            println!("    - {d}");
        }
    }

    // Span probe: confirm AST nodes carry usable byte offsets.
    println!("\n== span probe ==");
    if let Ok(query) = decypher::parse("MATCH (n:Host) RETURN n.hostname") {
        println!("  top-level span: {:?}", query.span);
    }
}

fn version() -> &'static str {
    // Keep in step with Cargo.toml; cargo would need a build script to
    // resolve the dependency version at compile time, which the spike
    // doesn't warrant.
    "0.2.0-alpha.6"
}
