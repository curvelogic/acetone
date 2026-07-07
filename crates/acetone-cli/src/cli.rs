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
    /// Merge another version into the current branch, creating a merge
    /// commit on a clean three-way merge (spec §7). The workspace must be
    /// clean and a branch checked out. Fast-forwards when possible. A clean
    /// merge is graph-validated (dangling edges and schema constraints); any
    /// breach — like a cell-level clash — is reported as a conflict and makes
    /// no commit (conflict resolution is not yet available).
    Merge {
        /// The version to merge in (branch short name, full ref name or
        /// commit hash).
        #[arg(value_name = "REF")]
        refspec: String,
        /// Commit message for the merge commit (default: `Merge <ref>`).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Declare a primary label's key (schema): the ordered key property
    /// names that give nodes of this label their identity. Required before
    /// Cypher `CREATE`/`MERGE` can persist nodes of the label (Invariant #3).
    DeclareLabel {
        /// The primary label.
        label: String,
        /// A key property name; repeat for a composite key, in order.
        #[arg(long = "key", required = true)]
        key: Vec<String>,
        /// A property that must be present (existence constraint); repeat.
        #[arg(long = "require")]
        require: Vec<String>,
        /// A non-key property that must be unique across nodes of this
        /// label (UNIQUE constraint); repeat.
        #[arg(long = "unique")]
        unique: Vec<String>,
    },
    /// Declare a relationship type (schema). Required before Cypher can
    /// create relationships of this type under a declared schema.
    DeclareRelType {
        /// The relationship type.
        rtype: String,
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
    /// Change a node's key (single-column keys). A key change is modelled
    /// as delete-plus-create in one commit (Invariant #3); incident edges
    /// are rewritten onto the new key. `SET` cannot change a key.
    Rekey {
        /// The node's primary label.
        label: String,
        /// The current key value.
        old_key: String,
        /// The new key value.
        new_key: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Resolve the conflicts of a merge in progress by taking one whole side,
    /// then `commit` to complete the merge (spec §6). Cell conflicts only;
    /// graph-level violations and per-key resolution arrive later.
    Resolve {
        /// Take the current branch's value for every conflict.
        #[arg(long = "all-ours")]
        all_ours: bool,
        /// Take the merged-in version's value for every conflict.
        #[arg(long = "all-theirs", conflicts_with = "all_ours")]
        all_theirs: bool,
    },
    /// Show the graph-level difference between two versions (branch short
    /// names, full ref names or commit hashes): the nodes and relationships
    /// added (`+`), removed (`-`) or modified (`~`) from `from` to `to`.
    Diff {
        /// The base version.
        from: String,
        /// The target version.
        to: String,
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
