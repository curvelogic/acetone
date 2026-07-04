//! Spike B: run the representative query set through the hand-rolled
//! recursive-descent parser and report per-category results.

use cypher_parser_spike::handrolled;
use cypher_parser_spike::queries::{Category, QUERIES};

fn main() {
    let mut ok = 0usize;
    let mut failed: Vec<(&str, Category, String)> = Vec::new();

    println!("== hand-rolled parser against the representative set ==\n");

    for q in QUERIES {
        match handrolled::parse(q.text) {
            Ok(query) => {
                ok += 1;
                println!(
                    "PASS  [{:?}] {} ({} clause(s))",
                    q.category,
                    q.name,
                    query.clauses.len()
                );
            }
            Err(e) => {
                println!("FAIL  [{:?}] {}\n      error: {}", q.category, q.name, e);
                failed.push((q.name, q.category, e.to_string()));
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

    // Span probe: property expression spans point back into the source.
    println!("\n== span probe ==");
    let src = "MATCH (n:Host) RETURN n.hostname";
    if let Ok(query) = handrolled::parse(src) {
        println!(
            "  query span: {:?} covers {:?}",
            query.span,
            &src[query.span.start..query.span.end]
        );
    }
}
