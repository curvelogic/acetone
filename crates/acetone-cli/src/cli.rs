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
    /// Declare a property index `idx/<name>` over `(label, properties)` (spec
    /// §3.3). The index is built from the current nodes and maintained
    /// transactionally thereafter; it accelerates equality lookups. Repeat
    /// `--property` for a **composite** index — its key is the ordered tuple of
    /// those property values. Indexes are null- and NaN-blind.
    DeclareIndex {
        /// The index name (the `idx/<name>` map).
        name: String,
        /// The indexed primary label.
        #[arg(long)]
        label: String,
        /// An indexed property; repeat, in order, for a composite index.
        #[arg(long = "property", required = true)]
        property: Vec<String>,
    },
    /// Rebuild every declared index from the nodes map (spec §3.3). A no-op
    /// when the indexes are already consistent; repairs any divergence `fsck`
    /// reports.
    Reindex,
    /// Export a graph version as per-label node tables and per-type edge
    /// tables (spec §7, §9). The inverse of `import`: exporting then importing
    /// into a fresh repo with the same schema reproduces identical map roots.
    Export {
        /// Output format.
        #[arg(value_parser = ["csv", "json", "ndjson"])]
        format: String,
        /// Export only this label's nodes.
        #[arg(long, conflicts_with = "edge")]
        label: Option<String>,
        /// Export only this relationship type's edges.
        #[arg(long)]
        edge: Option<String>,
        /// Output file (single table) or, with neither `--label` nor `--edge`,
        /// the directory to write one table per label and type into. A single
        /// table with no `--out` goes to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// workspaces, branches and tags; edge-map symmetry and index consistency
    /// as advisories, and a history-independence spot-check (a non-canonical
    /// map is an error). Exits non-zero when any error-severity finding exists.
    Fsck,
    /// Consolidate the object store into a self-contained packfile (spec §3.1,
    /// ADR-0011): delta rewritten chunks against their predecessors and prune
    /// superseded loose objects and packs. Representation-only — preserves
    /// every object exactly. Run periodically after churn to reclaim space.
    Gc,
    /// Rewrite all history under new chunk parameters, producing new hashes
    /// (ADR-0025). A version-preserving re-chunk — `format_version` is
    /// unchanged (chunk parameters are manifest data) — that re-encodes every
    /// version and rebuilds the commit graph, preserving each commit's
    /// message, author and committer (identity and timestamp). This is the
    /// generic history-rewrite engine; a future `format_version` bump plugs
    /// into the same command. Requires a clean, non-merging workspace, which it
    /// resets to the rewritten head.
    Migrate {
        /// Target minimum chunk size in bytes.
        #[arg(long)]
        min_bytes: u32,
        /// Target rolling-hash mask bits (mean chunk size ≈ 2^mask_bits).
        #[arg(long)]
        mask_bits: u32,
        /// Target maximum chunk size in bytes.
        #[arg(long)]
        max_bytes: u32,
    },
    /// Import a source file into the graph, recording provenance trailers
    /// (`Acetone-Source`/`-Extractor`/`-Source-Hash`) and detecting a no-op
    /// when the source is unchanged (spec §7). Node mode (`--label`) maps each
    /// row to a node; edge mode (`--edge`) maps each row to a relationship.
    /// Requires a clean workspace — declare and `commit` the target label's
    /// schema (and any relationship type) before importing.
    Import {
        /// Source format.
        #[arg(value_parser = ["csv", "json", "ndjson"])]
        format: String,
        /// Path to the source file.
        source: PathBuf,
        /// Node mode: the primary label for every imported row. The label's
        /// declared key selects which fields form the node key.
        #[arg(long, required_unless_present = "edge", conflicts_with = "edge")]
        label: Option<String>,
        /// Edge mode: the relationship type for every imported row. Requires
        /// `--from` and `--to`.
        #[arg(long)]
        edge: Option<String>,
        /// Edge mode: the source endpoint, as `LABEL=field[,field...]` (the
        /// fields carry the endpoint's key, in key order).
        #[arg(long, requires = "edge")]
        from: Option<String>,
        /// Edge mode: the destination endpoint, as `LABEL=field[,field...]`.
        #[arg(long, requires = "edge")]
        to: Option<String>,
        /// Edge mode: the field carrying the discriminator (optional).
        #[arg(long, requires = "edge")]
        disc: Option<String>,
        /// Import onto this branch in isolation, leaving the current branch
        /// unchanged (created if absent, appended to if present).
        #[arg(long)]
        branch: Option<String>,
        /// Commit message (default synthesised from the source and counts).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
}
