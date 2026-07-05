//! Argument grammar (spec §7, Phase 1 subset). Parsing only — no acetone
//! logic lives here; see `commands.rs`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "acetone",
    version,
    about = "The acetone command-line workbench"
)]
pub struct Cli {
    /// Path to the repository. Ignored by `init` when it is given its own
    /// PATH argument.
    #[arg(long, global = true, default_value = ".")]
    pub repo: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new acetone repository.
    Init {
        /// Object format (hash function) for the new repository.
        #[arg(long, default_value = "sha1", value_parser = ["sha1", "sha256"])]
        object_format: String,
        /// Directory to create the repository in (default: --repo, or `.`).
        path: Option<PathBuf>,
    },
    /// Show the current branch, head commit and workspace state.
    Status,
    /// Turn the workspace's staged changes into a commit.
    ///
    /// Refuses when the workspace has no changes since HEAD — including,
    /// on a brand new repository, an empty root commit. There is no
    /// `--allow-empty` yet.
    Commit {
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
        /// A `KEY=VALUE` commit trailer; may be repeated.
        #[arg(long = "trailer")]
        trailer: Vec<String>,
    },
    /// Show commit history, newest first.
    Log,
    /// List branches, or create one.
    Branch {
        /// Name of a new branch to create at the current head commit.
        /// Omit to list existing branches.
        name: Option<String>,
    },
    /// Switch the checked-out branch.
    Checkout {
        /// Branch to switch to.
        branch: String,
    },
    /// Insert or replace a node (plumbing; single-column keys only).
    PutNode {
        /// Primary label.
        label: String,
        /// Key value (parsed as an integer if it looks like one, else a
        /// string — see the CLI-level docs).
        key: String,
        /// A `KEY=VALUE` non-key property; may be repeated.
        #[arg(long = "prop")]
        prop: Vec<String>,
    },
    /// Look up a node by label and key.
    GetNode {
        /// Primary label.
        label: String,
        /// Key value (same parsing rule as `put-node`).
        key: String,
    },
    /// Insert or replace an edge (plumbing; no properties, no discriminator).
    PutEdge {
        /// Source node's primary label.
        src_label: String,
        /// Source node's key value.
        src_key: String,
        /// Relationship type.
        rtype: String,
        /// Destination node's primary label.
        dst_label: String,
        /// Destination node's key value.
        dst_key: String,
    },
    /// List nodes, in key order.
    ListNodes {
        /// Restrict to one primary label.
        #[arg(long)]
        label: Option<String>,
    },
    /// Run an openCypher read query against the graph.
    Query {
        /// The query text.
        cypher: String,
        /// Read at a specific ref (branch, tag or commit hash) instead of
        /// the current workspace state — whole-query time travel.
        #[arg(long)]
        at: Option<String>,
        /// Output format.
        #[arg(long, default_value = "table", value_parser = ["table", "json", "csv"])]
        format: String,
    },
    /// Start an interactive Cypher shell (readline REPL).
    ///
    /// Enter queries to run them against the current workspace state.
    /// Conveniences: `:checkout <ref>`, `:log`, `:format <table|json|csv>`,
    /// `:quit`. (`:diff` from spec §7 arrives with the Phase 4 diff
    /// machinery.)
    Shell,
    /// Verify repository integrity: manifest decode, chunk reachability
    /// and prolly-tree structure for every version reachable from
    /// workspaces, branches and tags; edge-map symmetry as an advisory.
    /// Exits non-zero when any error-severity finding exists.
    Fsck,
}
