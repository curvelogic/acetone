//! Argument grammar (spec §7, Phase 1 subset). Parsing only — no acetone
//! logic lives here; see `commands.rs`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// The role-group and git-relationship guide shown at the foot of
/// `acetone --help` (and `--help` in full). clap has no `help_heading` for
/// subcommands, so the grouping is carried here as free text rather than
/// inline on each variant.
const AFTER_HELP: &str = "\
Command groups:
  Everyday      init, status, commit, log, branch, checkout, diff, merge, resolve
  Schema        declare-label, declare-rel-type, declare-index, reindex, schema
  Data & query  import, export, query, shell
  Maintenance   fsck, gc, migrate
  Plumbing      put-node, get-node, put-edge, list-nodes, rekey

Relationship to git (an acetone version IS a git commit):
  Use acetone, not git   commit, merge, resolve, checkout, declare-*, reindex,
                         import, export, fsck, gc, migrate — the git equivalents
                         would write commits acetone cannot read.
  Either works           log, status, diff, branch — acetone's are graph-aware;
                         plain git still works on the same repo.
  Git only (transport)   clone, fetch, push, pull, remote — no acetone command;
                         any git remote is backup and transport.";

#[derive(Debug, Parser)]
#[command(
    name = "acetone",
    version,
    about = "The acetone command-line workbench",
    after_help = AFTER_HELP,
    after_long_help = AFTER_HELP,
    // Unique command prefixes resolve (`acetone st` → status); ambiguous ones
    // (`acetone c`, `co`) error with the candidates. This makes bare prefixes
    // script-fragile: release/CI scripts and docs must use FULL command names
    // (verified: .github/workflows/*.yml and docs/RELEASING.md do).
    infer_subcommands = true,
    // Bare `acetone` prints help rather than a terse usage error.
    arg_required_else_help = true
)]
pub struct Cli {
    /// Path to the repository, or any subdirectory of it — the enclosing
    /// repository is discovered by walking up parents (like `git -C`).
    /// Ignored by `init` when it is given its own PATH argument.
    #[arg(long, global = true, default_value = ".")]
    pub repo: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

// Variants are declaration-ordered to match the command groups in AFTER_HELP:
// Everyday, then Schema, Data & query, Maintenance, and Plumbing. clap lists
// subcommands in declaration order, so this keeps `Commands:` grouped.
#[derive(Debug, Subcommand)]
pub enum Command {
    // ---- Everyday ----
    /// Create a new acetone repository.
    Init {
        /// Object format (hash function) for the new repository.
        #[arg(long, default_value = "sha1", value_parser = ["sha1", "sha256"])]
        object_format: String,
        /// Directory to create the repository in (default: --repo, or `.`).
        path: Option<PathBuf>,
    },
    /// Show the current branch, head commit and workspace state.
    Status {
        /// Emit machine-readable JSON. The JSON shape is unstable and may
        /// change before 0.2.
        #[arg(long)]
        json: bool,
    },
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
    Log {
        /// Emit machine-readable JSON. The JSON shape is unstable and may
        /// change before 0.2.
        #[arg(long)]
        json: bool,
    },
    /// List branches, or create one.
    Branch {
        /// Name of a new branch to create at the current head commit.
        /// Omit to list existing branches.
        name: Option<String>,
        /// Emit machine-readable JSON. The JSON shape is unstable and may
        /// change before 0.2.
        #[arg(long)]
        json: bool,
    },
    /// Switch the checked-out branch.
    Checkout {
        /// Branch to switch to.
        branch: String,
    },
    /// Show the graph-level difference between two versions.
    ///
    /// Compares two versions (branch short names, full ref names or commit
    /// hashes): the nodes and relationships added (`+`), removed (`-`) or
    /// modified (`~`) from `from` to `to`.
    Diff {
        /// The base version.
        from: String,
        /// The target version.
        to: String,
        /// Emit machine-readable JSON. The JSON shape is unstable and may
        /// change before 0.2.
        #[arg(long)]
        json: bool,
    },
    /// Merge another version into the current branch.
    ///
    /// Merges another version (spec §7). The workspace must be clean and a
    /// branch checked out; fast-forwards when possible, otherwise a clean
    /// three-way merge writes a two-parent merge commit. Cell-level conflicts
    /// enter a merge-in-progress state: resolve them with
    /// `acetone resolve --all-ours|--all-theirs` (or by editing the graph),
    /// then `acetone commit` to complete. Graph-level breaches (a dangling edge
    /// or a broken schema constraint) are reported and make no commit.
    Merge {
        /// The version to merge in (branch short name, full ref name or
        /// commit hash).
        #[arg(value_name = "REF")]
        refspec: String,
        /// Commit message for the merge commit (default: `Merge <ref>`).
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    /// Resolve a merge in progress by taking one whole side.
    ///
    /// Resolves the conflicts of a merge in progress, then `commit` to complete
    /// the merge (spec §6). Cell conflicts only; graph-level violations and
    /// per-key resolution arrive later.
    Resolve {
        /// Take the current branch's value for every conflict.
        #[arg(long = "all-ours")]
        all_ours: bool,
        /// Take the merged-in version's value for every conflict.
        #[arg(long = "all-theirs", conflicts_with = "all_ours")]
        all_theirs: bool,
    },

    // ---- Schema ----
    /// Declare a primary label's key and constraints (schema).
    ///
    /// Declares the ordered key property names that give nodes of this label
    /// their identity. Required before Cypher `CREATE`/`MERGE` can persist
    /// nodes of the label (Invariant #3).
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
    /// Declare a relationship type (schema).
    ///
    /// Required before Cypher can create relationships of this type under a
    /// declared schema.
    DeclareRelType {
        /// The relationship type.
        rtype: String,
    },
    /// Declare a property index over `(label, properties)`.
    ///
    /// Declares a property index `idx/<name>` (spec §3.3). The index is built
    /// from the current nodes and maintained transactionally thereafter; it
    /// accelerates equality lookups. Repeat `--property` for a **composite**
    /// index — its key is the ordered tuple of those property values. Indexes
    /// are null- and NaN-blind.
    DeclareIndex {
        /// The index name (the `idx/<name>` map).
        name: String,
        /// The indexed primary label.
        #[arg(short = 'l', long)]
        label: String,
        /// An indexed property; repeat, in order, for a composite index.
        #[arg(long = "property", required = true)]
        property: Vec<String>,
    },
    /// Rebuild every declared index from the nodes map.
    ///
    /// Rebuilds every declared index (spec §3.3). A no-op when the indexes are
    /// already consistent; repairs any divergence `fsck` reports.
    Reindex,
    /// Show the declared schema — labels and keys, relationship types and indexes.
    ///
    /// Prints the repository's declared schema, grouped into labels (with their
    /// ordered key tuple and existence/unique constraints), relationship types,
    /// and property indexes. Read-only. `--at <ref>` inspects any version — a
    /// branch short name, full ref name or commit hash — without checking it
    /// out; with no `--at`, the current workspace's schema is shown.
    Schema {
        /// Read the schema at a specific ref (branch, tag or commit hash)
        /// instead of the current workspace state.
        #[arg(long)]
        at: Option<String>,
        /// Emit machine-readable JSON. The JSON shape is unstable and may
        /// change before 0.2.
        #[arg(long)]
        json: bool,
    },

    // ---- Data & query ----
    /// Import a source file into the graph.
    ///
    /// Imports a source file, recording provenance trailers
    /// (`Acetone-Source`/`-Extractor`/`-Source-Hash`) and detecting a no-op
    /// when the source is unchanged (spec §7). Node mode (`--label`) maps each
    /// row to a node; edge mode (`--edge`) maps each row to a relationship.
    /// Requires a clean workspace — declare and `commit` the target label's
    /// schema (and any relationship type) before importing.
    Import {
        /// Source format.
        #[arg(short = 'f', long, value_parser = ["csv", "json", "ndjson"])]
        format: String,
        /// Path to the source file.
        source: PathBuf,
        /// Node mode: the primary label for every imported row. The label's
        /// declared key selects which fields form the node key.
        #[arg(
            short = 'l',
            long,
            required_unless_present = "edge",
            conflicts_with = "edge"
        )]
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
    /// Export a graph version as per-label and per-type tables.
    ///
    /// Exports node tables per label and edge tables per type (spec §7, §9).
    /// The inverse of `import`: exporting then importing into a fresh repo with
    /// the same schema reproduces identical map roots.
    Export {
        /// Output format.
        #[arg(short = 'f', long, value_parser = ["csv", "json", "ndjson"])]
        format: String,
        /// Export only this label's nodes.
        #[arg(short = 'l', long, conflicts_with = "edge")]
        label: Option<String>,
        /// Export only this relationship type's edges.
        #[arg(long)]
        edge: Option<String>,
        /// Output file (single table) or, with neither `--label` nor `--edge`,
        /// the directory to write one table per label and type into. A single
        /// table with no `--out` goes to stdout.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Run an openCypher read query against the graph.
    #[command(visible_alias = "cypher")]
    Query {
        /// The query text.
        cypher: String,
        /// Read at a specific ref (branch, tag or commit hash) instead of
        /// the current workspace state — whole-query time travel.
        #[arg(long)]
        at: Option<String>,
        /// Output format.
        #[arg(short = 'f', long, default_value = "table", value_parser = ["table", "json", "csv"])]
        format: String,
    },
    /// Start an interactive Cypher shell (readline REPL).
    ///
    /// Enter queries — read or write — to run them against the current
    /// workspace state; a write advances the workspace (commit separately with
    /// `acetone commit`). Conveniences: `:checkout <ref>`, `:log`,
    /// `:format <table|json|csv>`, `:quit`.
    Shell,

    // ---- Maintenance ----
    /// Verify repository integrity.
    ///
    /// Checks manifest decode, chunk reachability and prolly-tree structure for
    /// every version reachable from workspaces, branches and tags; edge-map
    /// symmetry and index consistency as advisories, and a history-independence
    /// spot-check (a non-canonical map is an error). Exits non-zero when any
    /// error-severity finding exists.
    Fsck,
    /// Consolidate the object store into a packfile.
    ///
    /// Packs the object store into a self-contained packfile (spec §3.1,
    /// ADR-0011): delta rewritten chunks against their predecessors and prune
    /// superseded loose objects and packs. Representation-only — preserves
    /// every object exactly. Run periodically after churn to reclaim space.
    Gc,
    /// Rewrite all history under new chunk parameters.
    ///
    /// Produces new hashes (ADR-0025). A version-preserving re-chunk —
    /// `format_version` is unchanged (chunk parameters are manifest data) —
    /// that re-encodes every version and rebuilds the commit graph, preserving
    /// each commit's message, author and committer (identity and timestamp).
    /// This is the generic history-rewrite engine; a future `format_version`
    /// bump plugs into the same command. Requires a clean, non-merging
    /// workspace, which it resets to the rewritten head. Each chunk-parameter
    /// flag defaults to the repo's current value, so a no-flag `migrate`
    /// re-chunks under the same parameters (a repair that leaves hashes
    /// unchanged by history-independence); override a subset to re-chunk.
    Migrate {
        /// Target minimum chunk size in bytes (default: the repo's current value).
        #[arg(long)]
        min_bytes: Option<u32>,
        /// Target rolling-hash mask bits, mean chunk size ≈ 2^mask_bits (default: the repo's current value).
        #[arg(long)]
        mask_bits: Option<u32>,
        /// Target maximum chunk size in bytes (default: the repo's current value).
        #[arg(long)]
        max_bytes: Option<u32>,
    },

    // ---- Plumbing ----
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
        /// Emit machine-readable JSON (the node object, or `null` on a miss
        /// with a non-zero exit). The JSON shape is unstable and may change
        /// before 0.2.
        #[arg(long)]
        json: bool,
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
        #[arg(short = 'l', long)]
        label: Option<String>,
        /// Emit machine-readable JSON. The JSON shape is unstable and may
        /// change before 0.2.
        #[arg(long)]
        json: bool,
    },
    /// Change a node's key (single-column keys).
    ///
    /// A key change is modelled as delete-plus-create in one commit
    /// (Invariant #3); incident edges are rewritten onto the new key. `SET`
    /// cannot change a key.
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
}
