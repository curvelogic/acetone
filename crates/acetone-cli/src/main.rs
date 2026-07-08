//! The acetone CLI (spec §7) — a thin client over the library crates.

mod cli;
mod commands;
mod export;
mod import;
mod output;
mod query;
mod value;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(err) = commands::run(&cli.repo, cli.command) {
        // The Display chain (`{:#}`), never Debug: acetone-graph's errors
        // are already the friendly, typed messages a user should see.
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
