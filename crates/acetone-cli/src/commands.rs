//! One function per subcommand: call into `acetone-graph`, format output.
//! No graph logic lives here (CLAUDE.md: the CLI is a thin client).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use acetone_core::graph::merge::{ConflictMap, MergeConflict, MergeOutcome};
use acetone_core::graph::{InitOptions, Repository};
use acetone_core::model::Value;
use acetone_core::model::graph_keys::{EdgeKey, NodeKey};
use acetone_core::model::records::{EdgeRecord, NodeRecord};
use acetone_core::store::ObjectFormat;
use anyhow::{Context, Result, bail};

use crate::cli::Command;
use crate::json::{emit_json, key_tuple_to_json, value_to_json};
use crate::value::{
    format_label, format_value, parse_kv, parse_value, sanitise_identifier, sanitise_line,
};

use crate::output::{errln, outln};

use serde_json::{Value as Json, json};

/// Dispatch one parsed command.
pub fn run(repo_path: &Path, command: Command) -> Result<()> {
    match command {
        Command::Init {
            object_format,
            co_tenant,
            path,
        } => init(repo_path, &object_format, co_tenant.as_deref(), path),
        Command::Status { json } => status(repo_path, json),
        Command::Commit {
            message,
            trailer,
            allow_empty,
        } => commit(repo_path, &message, &trailer, allow_empty),
        Command::Log { all, json } => log(repo_path, all, json),
        Command::Branch {
            name,
            refspec,
            delete,
            json,
        } => branch(
            repo_path,
            name.as_deref(),
            refspec.as_deref(),
            delete.as_deref(),
            json,
        ),
        Command::Checkout { branch: name } => checkout(repo_path, &name),
        Command::Merge {
            refspec,
            message,
            abort,
        } => merge(repo_path, refspec.as_deref(), message.as_deref(), abort),
        Command::Resolve {
            all_ours,
            all_theirs,
        } => resolve(repo_path, all_ours, all_theirs),
        Command::DeclareLabel {
            label,
            key,
            require,
            unique,
        } => declare_label(repo_path, &label, &key, &require, &unique),
        Command::DeclareRelType { rtype } => declare_rel_type(repo_path, &rtype),
        Command::DeclareIndex {
            name,
            label,
            property,
        } => declare_index(repo_path, &name, &label, &property),
        Command::Reindex => reindex(repo_path),
        Command::Schema { at, json } => schema(repo_path, at.as_deref(), json),
        Command::Migrate {
            min_bytes,
            mask_bits,
            max_bytes,
        } => migrate(repo_path, min_bytes, mask_bits, max_bytes),
        Command::Export {
            format,
            label,
            edge,
            out,
        } => crate::export::run(
            repo_path,
            &format,
            label.as_deref(),
            edge.as_deref(),
            out.as_deref(),
        ),
        Command::PutNode { label, key, prop } => put_node(repo_path, &label, &key, &prop),
        Command::Rekey {
            label,
            old_key,
            new_key,
            message,
        } => rekey(repo_path, &label, &old_key, &new_key, &message),
        Command::Diff { from, to, json } => diff(repo_path, &from, &to, json),
        Command::GetNode { label, key, json } => get_node(repo_path, &label, &key, json),
        Command::PutEdge {
            src_label,
            src_key,
            rtype,
            dst_label,
            dst_key,
        } => put_edge(
            repo_path, &src_label, &src_key, &rtype, &dst_label, &dst_key,
        ),
        Command::ListNodes { label, json } => list_nodes(repo_path, label.as_deref(), json),
        Command::Query { cypher, at, format } => {
            let format = crate::query::Format::parse(&format)?;
            crate::query::run(repo_path, &cypher, at.as_deref(), format)
        }
        Command::Shell => crate::query::shell(repo_path),
        Command::Fsck => fsck(repo_path),
        Command::Gc => gc(repo_path),
        Command::Import {
            format,
            source,
            label,
            edge,
            from,
            to,
            disc,
            branch,
            message,
        } => crate::import::run(
            repo_path,
            &format,
            &source,
            label.as_deref(),
            edge.as_deref(),
            from.as_deref(),
            to.as_deref(),
            disc.as_deref(),
            branch.as_deref(),
            message.as_deref(),
        ),
    }
}

fn fsck(repo_path: &Path) -> Result<()> {
    // Open only the store (not a full Repository): fsck must run even when the
    // default workspace manifest is damaged — precisely when it is needed
    // (acetone-zhp). `Repository::open` would fail-fast decoding that manifest.
    let report = acetone_core::graph::fsck::check_path(repo_path)
        .with_context(|| format!("checking repository at {}", repo_path.display()))?;
    for finding in &report.findings {
        // Findings embed repository-controlled text (index names, ref
        // names, decode-error detail): sanitise at the terminal boundary.
        outln!("{}", sanitise_line(&finding.to_string()));
    }
    if report.is_clean() {
        outln!("fsck: clean");
    } else {
        outln!(
            "fsck: {} error(s), {} advisory(ies)",
            report.errors().count(),
            report.advisories().count()
        );
    }
    if report.has_errors() {
        bail!("repository has integrity errors");
    }
    Ok(())
}

fn init(
    repo_path: &Path,
    object_format: &str,
    co_tenant: Option<&str>,
    path: Option<PathBuf>,
) -> Result<()> {
    let target = path.unwrap_or_else(|| repo_path.to_owned());
    let object_format = match object_format {
        "sha1" => ObjectFormat::Sha1,
        "sha256" => ObjectFormat::Sha256,
        // Unreachable: clap's value_parser restricts the flag to these two.
        other => bail!("unsupported object format {other:?}"),
    };
    let nondefault_format = !matches!(object_format, ObjectFormat::Sha1);
    let mut options = InitOptions::default();
    options.object_format = object_format;
    if let Some(graph) = co_tenant {
        // Co-tenant mode (ADR-0050): the graph lives inside an existing git
        // repository, on its own ref namespace, alongside the code. The object
        // format follows the host repository, so `--object-format` does not
        // apply here — warn if the user explicitly asked for a non-default one.
        if nondefault_format {
            errln!(
                "note: --object-format is ignored with --co-tenant; the graph shares the host repository's object format"
            );
        }
        Repository::init_co_tenant(&target, graph, options).with_context(|| {
            format!(
                "initialising co-tenant graph {graph:?} in the repository at {}",
                target.display()
            )
        })?;
        outln!(
            "Initialized co-tenant acetone graph {graph:?} in {} \
             (branches under refs/heads/acetone/{graph}/)",
            target.display()
        );
    } else {
        Repository::init(&target, options)
            .with_context(|| format!("initialising repository at {}", target.display()))?;
        outln!(
            "Initialized empty acetone repository in {}",
            target.display()
        );
    }
    Ok(())
}

pub(crate) fn open(repo_path: &Path) -> Result<Repository> {
    Repository::open(repo_path)
        .with_context(|| format!("opening repository at {}", repo_path.display()))
}

pub(crate) fn status(repo_path: &Path, json: bool) -> Result<()> {
    let repo = open(repo_path)?;
    // Short branch name (None when detached), head hash, dirtiness, merge
    // state and the workspace counts — the same facts both paths report.
    let branch = repo.current_branch()?.map(|full| {
        repo.namespace()
            .branch_name(&full)
            .unwrap_or(&full)
            .to_owned()
    });
    let head = repo.head_commit()?.map(|h| h.to_hex());
    let dirty = repo.is_dirty()?;
    // While a merge is in progress, report how many conflicts remain and
    // whether any is a graph-level violation (which resolves by editing the
    // graph, not by picking a side). Graph violations are re-derived live
    // (ADR-0058), so a violation left by a resolution shows up here too.
    let (merge_remaining, merge_has_graph) = if repo.merge_head()?.is_some() {
        let conflicts = repo.conflicts()?;
        let has_graph = conflicts.iter().any(|c| {
            matches!(
                c,
                acetone_core::graph::conflicts::WorkspaceConflict::Graph(_)
            )
        });
        (Some(conflicts.len()), has_graph)
    } else {
        (None, false)
    };
    // Streaming counts (security review LOW-2): no record decode, no Vec
    // materialisation — status stays cheap on a large graph.
    let snapshot = repo.workspace_snapshot()?;
    let nodes = snapshot.node_count()?;
    let edges = snapshot.edge_count()?;
    let schema_entries = snapshot.schema_entry_count()?;

    if json {
        let merge = merge_remaining
            .map(|remaining| json!({ "in_progress": true, "conflicts_remaining": remaining }))
            .unwrap_or(Json::Null);
        emit_json(&json!({
            "branch": branch,
            "head": head,
            "workspace": if dirty { "dirty" } else { "clean" },
            "nodes": nodes,
            "edges": edges,
            "schema_entries": schema_entries,
            "merge": merge,
        }));
        return Ok(());
    }

    match &branch {
        // Branch names are repository-controlled (a hostile clone's refs) and
        // ref validation permits multibyte bidi and zero-width characters, so
        // sanitise to the identifier bar before the terminal — as the shell
        // prompt does.
        Some(short) => outln!("On branch {}", sanitise_identifier(short)),
        None => outln!("Not on any branch (detached)"),
    }
    match &head {
        Some(hex) => outln!("HEAD: {hex}"),
        None => outln!("HEAD: (no commits yet)"),
    }
    outln!("workspace: {}", if dirty { "dirty" } else { "clean" });
    // A merge in progress: show how many conflicts remain to resolve.
    if let Some(remaining) = merge_remaining {
        if remaining == 0 {
            outln!("merge: in progress, all conflicts resolved — run `acetone commit` to finish");
        } else if merge_has_graph {
            outln!(
                "merge: in progress, {remaining} graph-level violation(s) — repair the \
                 graph (delete the dangling relationship, restore the endpoint, or fix \
                 the constraint), then `acetone commit`, or `acetone merge --abort`"
            );
        } else {
            outln!(
                "merge: in progress, {remaining} conflict(s) to resolve \
                 (`acetone resolve --all-ours|--all-theirs`, or write the \
                 conflicted entities directly), or `acetone merge --abort`"
            );
        }
    }
    outln!("nodes: {nodes}, edges: {edges}, schema entries: {schema_entries}");
    Ok(())
}

pub(crate) fn commit(
    repo_path: &Path,
    message: &str,
    trailers: &[String],
    allow_empty: bool,
) -> Result<()> {
    let repo = open(repo_path)?;
    // Completing a merge always commits (it records the two-parent history)
    // even when the resolved result happens to match HEAD. It still refuses if
    // conflicts remain unresolved (the library errors, but this is friendlier).
    if repo.merge_head()?.is_some() {
        // Only unresolved *cell* conflicts are counted for this friendly early
        // bail — graph violations are never cleared by a write; they are gated
        // by the library's completion re-validation (acetone-mws), so let
        // `commit` run and re-validate rather than blocking here.
        let cell_remaining = repo
            .conflicts()?
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    acetone_core::graph::conflicts::WorkspaceConflict::Cell { .. }
                )
            })
            .count();
        if cell_remaining > 0 {
            bail!(
                "cannot commit: {cell_remaining} unresolved conflict(s) — \
                 resolve with `acetone resolve --all-ours|--all-theirs`"
            );
        }
    }
    let trailers: Vec<(String, String)> = trailers
        .iter()
        .map(|raw| parse_kv(raw, "--trailer").map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect::<Result<_>>()?;
    let txn = repo.begin_write()?;
    // The no-change guard lives in the library (acetone-k78): a bare `commit`
    // on an already-committed workspace (or a brand-new repository's empty
    // root) surfaces GraphError::NothingToCommit unless --allow-empty.
    let result = if allow_empty {
        txn.commit_allow_empty(message, &trailers, None)
    } else {
        txn.commit(message, &trailers, None)
    };
    let id = result.context("committing workspace")?;
    outln!("committed {}", id.to_hex());
    Ok(())
}

fn log(repo_path: &Path, all: bool, json: bool) -> Result<()> {
    let repo = open(repo_path)?;
    // Default: the current branch's first-parent chain (its own changelog).
    // `--all`: every commit reachable from any branch, deterministic
    // newest-first topological order (acetone-b6q).
    let entries = if all { repo.log_all()? } else { repo.log(None)? };
    if json {
        // serde_json escapes control characters, so hostile-clone messages
        // and trailers cannot inject raw terminal escapes here (no
        // sanitise_line needed on the JSON path).
        let rows: Vec<Json> = entries
            .iter()
            .map(|entry| {
                let subject = entry.message.lines().next().unwrap_or("");
                let trailers: Vec<Json> = entry
                    .trailers
                    .iter()
                    .map(|(k, v)| json!({ "key": k, "value": v }))
                    .collect();
                let parents: Vec<Json> = entry
                    .parents
                    .iter()
                    .map(|p| Json::String(p.to_hex()))
                    .collect();
                json!({
                    "hash": entry.id.to_hex(),
                    "subject": subject,
                    "message": entry.message,
                    "trailers": trailers,
                    "parents": parents,
                })
            })
            .collect();
        emit_json(&Json::Array(rows));
        return Ok(());
    }
    for entry in &entries {
        // Commit messages and trailers are raw bytes from potentially
        // hostile clones (lossily decoded, not constrained by git):
        // sanitise before they reach the terminal.
        let subject = entry.message.lines().next().unwrap_or("");
        outln!("{} {}", entry.id.to_hex(), sanitise_line(subject));
        // Under --all, merge structure is shown honestly: a merge commit's
        // parent hashes, printed before any trailers (so a hostile
        // `merge:` trailer cannot pre-empt the real line). The default
        // first-parent output is unchanged.
        if all && entry.parents.len() > 1 {
            let parents: Vec<String> = entry.parents.iter().map(|p| p.to_hex()).collect();
            outln!("    merge: {}", parents.join(" "));
        }
        for (key, value) in &entry.trailers {
            outln!("    {}: {}", sanitise_line(key), sanitise_line(value));
        }
    }
    Ok(())
}

fn branch(
    repo_path: &Path,
    name: Option<&str>,
    refspec: Option<&str>,
    delete: Option<&str>,
    json: bool,
) -> Result<()> {
    let repo = open(repo_path)?;
    if let Some(name) = delete {
        // Ref removal only: the commits stay in the store, reachable by
        // hash, until a git-level gc expires them (acetone gc never
        // deletes). The library refuses to delete the checked-out branch.
        let was = repo
            .delete_branch(name)
            .with_context(|| format!("deleting branch {name:?}"))?;
        if json {
            emit_json(&json!({ "deleted": name, "hash": was.to_hex() }));
            return Ok(());
        }
        outln!("deleted branch {name:?} (was {})", was.to_hex());
        return Ok(());
    }
    match name {
        None => {
            let current = repo.current_branch()?;
            let current_short = current.as_deref().map(|full| {
                repo.namespace()
                    .branch_name(full)
                    .unwrap_or(full)
                    .to_owned()
            });
            let branches = repo.branches()?;
            if json {
                let names: Vec<Json> = branches
                    .iter()
                    .map(|(short, _hash)| Json::String(short.clone()))
                    .collect();
                emit_json(&json!({
                    "current": current_short,
                    "branches": names,
                }));
                return Ok(());
            }
            for (short, _hash) in branches {
                let marker = if current_short.as_deref() == Some(short.as_str()) {
                    "*"
                } else {
                    " "
                };
                // Branch names are repository-controlled identifiers;
                // sanitise (control, bidi and zero-width characters) before
                // the terminal.
                outln!("{marker} {}", sanitise_identifier(&short));
            }
        }
        Some(name) => {
            let target = repo
                .create_branch(name, refspec)
                .with_context(|| format!("creating branch {name:?}"))?;
            if json {
                emit_json(&json!({ "created": name, "hash": target.to_hex() }));
                return Ok(());
            }
            outln!("created branch {name:?} at {}", target.to_hex());
        }
    }
    Ok(())
}

fn checkout(repo_path: &Path, name: &str) -> Result<()> {
    let repo = open(repo_path)?;
    repo.checkout_branch(name)
        .with_context(|| format!("checking out branch {name:?}"))?;
    outln!("switched to branch {name:?}");
    Ok(())
}

fn merge(
    repo_path: &Path,
    refspec: Option<&str>,
    message: Option<&str>,
    abort: bool,
) -> Result<()> {
    let repo = open(repo_path)?;
    if abort {
        repo.abort_merge().context("aborting merge")?;
        outln!("merge aborted — workspace restored to the branch tip");
        return Ok(());
    }
    // clap guarantees a refspec unless --abort was given.
    let refspec = refspec.expect("clap requires REF unless --abort");
    let message = message
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Merge {refspec}"));
    match repo
        .merge(refspec, &message)
        .with_context(|| format!("merging {refspec:?}"))?
    {
        MergeOutcome::AlreadyUpToDate => outln!("already up to date"),
        MergeOutcome::FastForward(head) => {
            outln!("fast-forwarded to {}", head.to_hex());
        }
        MergeOutcome::Merged(commit) => {
            outln!("merge commit {}", commit.to_hex());
        }
        MergeOutcome::Conflicts(conflicts) => {
            outln!("merge produced {} conflict(s):", conflicts.len());
            for c in &conflicts {
                outln!("  {}", render_conflict(c));
            }
            // Both cell and graph conflicts now enter merge-in-progress
            // (acetone-mws): cell conflicts resolve by picking a side or writing
            // the entity; graph violations resolve by repairing the graph. Either
            // way, `commit` re-validates before completing, or `merge --abort`
            // backs out.
            let is_graph = conflicts
                .iter()
                .any(|c| matches!(c, MergeConflict::Graph(_)));
            if is_graph {
                outln!(
                    "repair the graph (delete the dangling relationship, restore the \
                     endpoint, or fix the constraint breach), then `acetone commit` to \
                     complete — or `acetone merge --abort` to back out"
                );
            } else {
                outln!(
                    "resolve with `acetone resolve --all-ours|--all-theirs` (or write \
                     the conflicted entities), then `acetone commit` to complete — or \
                     `acetone merge --abort` to back out"
                );
            }
            // Non-zero exit: the merge did not finish.
            bail!("merge conflicts remain");
        }
    }
    Ok(())
}

fn resolve(repo_path: &Path, all_ours: bool, all_theirs: bool) -> Result<()> {
    let side = match (all_ours, all_theirs) {
        (true, false) => acetone_core::graph::repo::ResolveSide::Ours,
        (false, true) => acetone_core::graph::repo::ResolveSide::Theirs,
        (false, false) => bail!(
            "choose a side: --all-ours or --all-theirs \
             (per-key resolution arrives with a later change)"
        ),
        (true, true) => bail!("--all-ours and --all-theirs are mutually exclusive"),
    };
    let repo = open(repo_path)?;
    let count = repo.resolve_all(side).context("resolving conflicts")?;
    // The resolved graph may still carry (or newly compose) graph-level
    // violations — e.g. picking the side that deleted a node while the merge
    // kept an edge to it (acetone-jm8). They are re-derived live (ADR-0058):
    // tell the operator now, not at the commit refusal.
    let violations: Vec<String> = repo
        .conflicts()?
        .iter()
        .filter_map(|c| match c {
            acetone_core::graph::conflicts::WorkspaceConflict::Graph(v) => Some(v.to_string()),
            acetone_core::graph::conflicts::WorkspaceConflict::Cell { .. } => None,
        })
        .collect();
    if violations.is_empty() {
        outln!("resolved {count} conflict(s) — run `acetone commit` to complete the merge");
    } else {
        outln!(
            "resolved {count} conflict(s), but the resolved graph has {} graph-level violation(s):",
            violations.len()
        );
        for v in &violations {
            outln!("  {v}");
        }
        outln!(
            "repair the graph (delete the dangling relationship, restore the endpoint, \
             or fix the constraint breach), then `acetone commit` to complete — or \
             `acetone merge --abort` to back out"
        );
    }
    Ok(())
}

/// Render a hex string for an undecodable key.
fn hex_key(key: &[u8]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a node key for display, falling back to hex.
fn render_node_key(key: &[u8]) -> String {
    NodeKey::decode(key)
        .map(|k| format_node_key(&k))
        .unwrap_or_else(|_| hex_key(key))
}

/// Render a forward edge key for display, falling back to hex.
fn render_edge_key(key: &[u8]) -> String {
    EdgeKey::decode_fwd(key)
        .map(|k| format_edge_key(&k))
        .unwrap_or_else(|_| hex_key(key))
}

/// Render one merge conflict — cell clash or graph-level violation — as a
/// single human-readable line.
fn render_conflict(c: &MergeConflict) -> String {
    match c {
        MergeConflict::Cell(cell) => {
            // A cell-wise merge (ADR-0035) names the single property that
            // diverged; a whole-record conflict (disputed existence, or a
            // schema key) has none.
            let at = match &cell.property {
                Some(property) => format!(" property {}", format_label(property)),
                None => String::new(),
            };
            match cell.map {
                ConflictMap::Nodes => format!("node {}{at}", render_node_key(&cell.key)),
                ConflictMap::Edges => format!("edge {}{at}", render_edge_key(&cell.key)),
                ConflictMap::Schema => format!("schema {}{at}", hex_key(&cell.key)),
            }
        }
        // A graph violation renders via its canonical `Display` (shared with
        // `GraphError::MergeViolations`, acetone-jm8), which escapes
        // attacker-writable labels and keys (the PR #25 bar).
        MergeConflict::Graph(violation) => violation.to_string(),
    }
}

fn single_key(label: &str, key: &str) -> Result<NodeKey> {
    NodeKey::new(label, vec![parse_value(key)])
        .with_context(|| format!("building key for label {label:?}"))
}

pub(crate) fn declare_label(
    repo_path: &Path,
    label: &str,
    key: &[String],
    require: &[String],
    unique: &[String],
) -> Result<()> {
    use acetone_core::model::schema::{LabelDef, SchemaEntry};
    let def = LabelDef::new(
        key.to_vec(),
        BTreeMap::new(),
        require.to_vec(),
        unique.to_vec(),
    )
    .with_context(|| format!("declaring schema for label {label:?}"))?;
    let repo = open(repo_path)?;
    // Backfill check (acetone-9gw): a `--require`/`--unique` set declared
    // over existing data the data already violates is refused with the
    // violating nodes named — accepting it silently would leave violations
    // that fail unrelated writes later and that neither commit nor fsck
    // reported at declare time.
    {
        let snapshot = repo.workspace_snapshot()?;
        let violations = acetone_core::graph::constraints::check_label(&snapshot, label, &def)?;
        if !violations.is_empty() {
            // The rendering escapes every repository-controlled string via
            // the model display helpers (like render_conflict); the only
            // control characters are its own line breaks.
            bail!(
                "cannot declare label {}: existing data violates the declared \
                 constraints — {}",
                format_label(label),
                acetone_core::graph::constraints::ConstraintViolations(violations)
            );
        }
    }
    let entry = SchemaEntry::Label {
        name: label.to_owned(),
        def,
    };
    let mut txn = repo.begin_write()?;
    txn.put_schema(&entry)?;
    txn.save().context("saving workspace")?;
    // Escape every echoed name at the terminal boundary — key/require/unique
    // property names are user- (or schema-) controlled, like the label.
    let escaped = |names: &[String]| {
        names
            .iter()
            .map(|n| format_label(n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    outln!(
        "declared label {} key [{}]",
        format_label(label),
        escaped(key)
    );
    Ok(())
}

pub(crate) fn declare_rel_type(repo_path: &Path, rtype: &str) -> Result<()> {
    use acetone_core::model::schema::{RelTypeDef, SchemaEntry};
    let def = RelTypeDef::new(None, BTreeMap::new(), [])
        .with_context(|| format!("declaring relationship type {rtype:?}"))?;
    let entry = SchemaEntry::RelType {
        name: rtype.to_owned(),
        def,
    };
    let repo = open(repo_path)?;
    let mut txn = repo.begin_write()?;
    txn.put_schema(&entry)?;
    txn.save().context("saving workspace")?;
    outln!("declared relationship type {}", format_label(rtype));
    Ok(())
}

pub(crate) fn declare_index(
    repo_path: &Path,
    name: &str,
    label: &str,
    properties: &[String],
) -> Result<()> {
    use acetone_core::model::schema::{IndexDef, SchemaEntry};
    let def = IndexDef::new(label, properties.to_vec())
        .with_context(|| format!("declaring index {name:?}"))?;
    let entry = SchemaEntry::Index {
        name: name.to_owned(),
        def,
    };
    let repo = open(repo_path)?;
    let mut txn = repo.begin_write()?;
    // Declaring the index stages its schema entry; the flush builds the
    // `idx/<name>` map from the current nodes (spec §3.3, Invariant #5).
    txn.put_schema(&entry)?;
    txn.save().context("saving workspace")?;
    let props = properties
        .iter()
        .map(|p| format_label(p))
        .collect::<Vec<_>>()
        .join(", ");
    outln!(
        "declared index {} on {}({})",
        format_label(name),
        format_label(label),
        props
    );
    Ok(())
}

fn reindex(repo_path: &Path) -> Result<()> {
    let repo = open(repo_path)?;
    repo.reindex().context("reindexing")?;
    outln!("reindexed");
    Ok(())
}

pub(crate) fn schema(repo_path: &Path, at: Option<&str>, json: bool) -> Result<()> {
    use acetone_core::model::schema::SchemaEntry;

    let repo = open(repo_path)?;
    let snapshot = match at {
        Some(refspec) => repo
            .snapshot(refspec)
            .with_context(|| format!("reading schema at {refspec:?}"))?,
        None => repo.workspace_snapshot()?,
    };
    let entries = snapshot.schema_entries()?;

    // Partition the entries by kind. `schema_entries()` returns them in the
    // schema map's key order, which is grouped and sorted by (kind, name); we
    // keep that order within each group.
    let mut labels: Vec<(&str, &acetone_core::model::schema::LabelDef)> = Vec::new();
    let mut rel_types: Vec<&str> = Vec::new();
    let mut indexes: Vec<(&str, &acetone_core::model::schema::IndexDef)> = Vec::new();
    for entry in &entries {
        match entry {
            SchemaEntry::Label { name, def } => labels.push((name, def)),
            SchemaEntry::RelType { name, .. } => rel_types.push(name),
            SchemaEntry::Index { name, def } => indexes.push((name, def)),
        }
    }

    if json {
        let strings = |names: &[String]| -> Json {
            Json::Array(names.iter().map(|n| Json::String(n.clone())).collect())
        };
        let label_json: Vec<Json> = labels
            .iter()
            .map(|(name, def)| {
                json!({
                    "name": name,
                    "key": strings(def.key()),
                    "required": strings(def.exists()),
                    "unique": strings(def.unique()),
                    "surrogate": def.is_surrogate(),
                })
            })
            .collect();
        let rel_json: Vec<Json> = rel_types
            .iter()
            .map(|n| Json::String((*n).to_owned()))
            .collect();
        let index_json: Vec<Json> = indexes
            .iter()
            .map(|(name, def)| {
                json!({
                    "name": name,
                    "label": def.label(),
                    "properties": strings(def.properties()),
                })
            })
            .collect();
        emit_json(&json!({
            "labels": label_json,
            "relationship_types": rel_json,
            "indexes": index_json,
        }));
        return Ok(());
    }

    if entries.is_empty() {
        outln!("(no schema declared)");
        return Ok(());
    }

    // A parenthesised, comma-separated list of names, each escaped through
    // format_label — schema names can be hostile-clone data.
    let name_list = |names: &[String]| -> String {
        let parts: Vec<String> = names.iter().map(|n| format_label(n)).collect();
        format!("({})", parts.join(", "))
    };

    outln!("Labels");
    if labels.is_empty() {
        outln!("  (none)");
    } else {
        // Pad the (escaped) label names to a common width so the clauses line
        // up; cap the padding so one long name cannot push everything out.
        let width = labels
            .iter()
            .map(|(name, _)| format_label(name).chars().count())
            .max()
            .unwrap_or(0)
            .min(24);
        for (name, def) in &labels {
            let mut clauses = vec![format!("key {}", name_list(def.key()))];
            if def.is_surrogate() {
                clauses.push("surrogate".to_owned());
            }
            if !def.exists().is_empty() {
                clauses.push(format!("required {}", name_list(def.exists())));
            }
            if !def.unique().is_empty() {
                clauses.push(format!("unique {}", name_list(def.unique())));
            }
            outln!(
                "  {:<width$}  {}",
                format_label(name),
                clauses.join("  "),
                width = width
            );
        }
    }

    outln!("Relationship types");
    if rel_types.is_empty() {
        outln!("  (none)");
    } else {
        for name in &rel_types {
            outln!("  {}", format_label(name));
        }
    }

    outln!("Indexes");
    if indexes.is_empty() {
        outln!("  (none)");
    } else {
        let width = indexes
            .iter()
            .map(|(name, _)| format_label(name).chars().count())
            .max()
            .unwrap_or(0)
            .min(24);
        for (name, def) in &indexes {
            outln!(
                "  {:<width$}  on {} {}",
                format_label(name),
                format_label(def.label()),
                name_list(def.properties()),
                width = width
            );
        }
    }

    Ok(())
}

fn migrate(
    repo_path: &Path,
    min_bytes: Option<u32>,
    mask_bits: Option<u32>,
    max_bytes: Option<u32>,
) -> Result<()> {
    use acetone_core::graph::{Rechunk, rewrite_history};

    let repo = open(repo_path)?;
    // Each unspecified parameter defaults to the repo's current value, so a
    // no-flag `migrate` re-chunks under the same parameters — a repair that
    // leaves every hash unchanged (history-independence), never a silent
    // profile change.
    let current = repo
        .workspace_manifest()
        .context("reading the current chunk parameters")?
        .chunk_params;
    let min_bytes = min_bytes.unwrap_or(current.min_bytes());
    let mask_bits = mask_bits.unwrap_or(current.mask_bits());
    let max_bytes = max_bytes.unwrap_or(current.max_bytes());
    let transform = Rechunk::from_raw(min_bytes, mask_bits, max_bytes)
        .context("invalid target chunk parameters")?;
    let report = rewrite_history(&repo, &transform).context("rewriting history")?;
    outln!(
        "migrate: rewrote {} commit(s), updated {} ref(s), rewrote {} annotated tag(s)",
        report.commits_rewritten,
        report.refs_updated,
        report.tags_rewritten
    );
    Ok(())
}

fn gc(repo_path: &Path) -> Result<()> {
    let repo = open(repo_path)?;
    let stats = repo.gc().context("consolidating the object store")?;
    outln!(
        "gc: packed {} object(s) ({} delta, {} whole) into {} bytes; \
         pruned {} loose object(s), {} superseded pack(s)",
        stats.objects,
        stats.deltas,
        stats.whole,
        stats.pack_bytes,
        stats.pruned_loose,
        stats.pruned_packs,
    );
    Ok(())
}

fn rekey(repo_path: &Path, label: &str, old_key: &str, new_key: &str, message: &str) -> Result<()> {
    let repo = open(repo_path)?;
    let old = single_key(label, old_key)?;
    let new = single_key(label, new_key)?;
    let commit = repo
        .rekey(&old, &new, message)
        .with_context(|| format!("rekeying {}", format_node_key(&old)))?;
    outln!(
        "rekeyed {} -> {} in {}",
        format_node_key(&old),
        format_node_key(&new),
        commit.to_hex()
    );
    Ok(())
}

fn put_node(repo_path: &Path, label: &str, key: &str, props: &[String]) -> Result<()> {
    let repo = open(repo_path)?;
    let node_key = single_key(label, key)?;
    let mut properties = BTreeMap::new();
    for raw in props {
        let (name, value) = parse_kv(raw, "--prop")?;
        properties.insert(name.to_owned(), parse_value(value));
    }
    let record = NodeRecord::new(std::iter::empty::<String>(), properties);
    // Constraint guard (acetone-9gw, PR #184 review): plumbing must not
    // bypass what Cypher CREATE and import enforce. Judged against the
    // post-put state with a one-key focus, so an upsert of the same node is
    // never a self-collision. Undeclared labels stay raw, as before.
    {
        let snapshot = repo.workspace_snapshot()?;
        let violations =
            acetone_core::graph::constraints::check_upsert(&snapshot, &node_key, &record)?;
        if !violations.is_empty() {
            bail!(
                "cannot put node {}: {}",
                format_node_key(&node_key),
                acetone_core::graph::constraints::ConstraintViolations(violations)
            );
        }
    }
    let mut txn = repo.begin_write()?;
    txn.put_node(&node_key, &record)?;
    txn.save().context("saving workspace")?;
    outln!("put node {}", format_node_key(&node_key));
    Ok(())
}

/// `Label [key, ...]`, escaped — the one place a node key is rendered, used
/// by every command that echoes one.
pub(crate) fn format_node_key(key: &NodeKey) -> String {
    acetone_core::model::display::format_node_key(key)
}

pub(crate) fn format_edge_key(key: &EdgeKey) -> String {
    let base = format!(
        "{} -{}-> {}",
        format_node_key(key.src()),
        format_label(key.rtype()),
        format_node_key(key.dst()),
    );
    // Parallel edges are distinguished by a discriminator; show it when set so
    // two edges between the same endpoints render distinctly (14c.1 note).
    match key.disc() {
        Value::Null => base,
        disc => format!("{base} [{}]", format_value(disc)),
    }
}

fn diff(repo_path: &Path, from: &str, to: &str, json: bool) -> Result<()> {
    use acetone_core::graph::diff::ChangeKind;
    let repo = open(repo_path)?;
    let diff = repo
        .diff(from, to)
        .with_context(|| format!("diffing {from:?}..{to:?}"))?;

    if json {
        // Node changes first, then edge changes — the same deterministic
        // order the human path prints, mirrored into the `changes` array.
        let node_kind = |kind: ChangeKind| match kind {
            ChangeKind::Added => "node_added",
            ChangeKind::Removed => "node_removed",
            ChangeKind::Modified => "node_modified",
        };
        let rel_kind = |kind: ChangeKind| match kind {
            ChangeKind::Added => "rel_added",
            ChangeKind::Removed => "rel_removed",
            ChangeKind::Modified => "rel_modified",
        };
        let mut changes: Vec<Json> = Vec::new();
        for change in &diff.nodes {
            changes.push(json!({
                "kind": node_kind(change.kind),
                "label": change.key.label(),
                "key": key_tuple_to_json(change.key.key()),
            }));
        }
        for change in &diff.edges {
            let key = &change.key;
            changes.push(json!({
                "kind": rel_kind(change.kind),
                "rel_type": key.rtype(),
                "src": json!({
                    "label": key.src().label(),
                    "key": key_tuple_to_json(key.src().key()),
                }),
                "dst": json!({
                    "label": key.dst().label(),
                    "key": key_tuple_to_json(key.dst().key()),
                }),
                "disc": value_to_json(key.disc()),
            }));
        }
        emit_json(&json!({
            "from": from,
            "to": to,
            "changes": changes,
        }));
        return Ok(());
    }

    // `+` added, `-` removed, `~` modified — the sign is the change's own
    // meaning, so it reads at a glance and matches the diff graph's labels.
    let sign = |kind: ChangeKind| match kind {
        ChangeKind::Added => '+',
        ChangeKind::Removed => '-',
        ChangeKind::Modified => '~',
    };
    for change in &diff.nodes {
        outln!(
            "{} node {}",
            sign(change.kind),
            format_node_key(&change.key)
        );
    }
    for change in &diff.edges {
        outln!(
            "{} edge {}",
            sign(change.kind),
            format_edge_key(&change.key)
        );
    }
    if diff.is_empty() {
        outln!("(no changes)");
    }
    Ok(())
}

fn get_node(repo_path: &Path, label: &str, key: &str, json: bool) -> Result<()> {
    let repo = open(repo_path)?;
    let node_key = single_key(label, key)?;
    let snapshot = repo.workspace_snapshot()?;
    match snapshot.get_node(&node_key)? {
        // Absence is a non-zero exit so scripts can detect it. On the JSON
        // path, emit `null` to stdout first (so a script can parse it) and
        // still exit non-zero; the human path leaves stdout empty. Either
        // way the error goes to stderr as `error: not found`.
        None => {
            if json {
                emit_json(&Json::Null);
            }
            bail!("not found");
        }
        Some(record) => {
            if json {
                emit_json(&node_to_json(&node_key, &record));
                return Ok(());
            }
            // Echo the canonical parsed key, not the raw argument: the two
            // agree today (single-column keys only), but this stays
            // correct if a richer key grammar ever changes how a raw
            // argument maps to a value.
            outln!("node: {}", format_node_key(&node_key));
            // Secondary labels are repository-controlled and content-
            // unvalidated: escape each like every other label.
            let labels: Vec<String> = record
                .secondary_labels()
                .iter()
                .map(|l| format_label(l))
                .collect();
            outln!("secondary_labels: [{}]", labels.join(", "));
            outln!("properties:");
            for (name, value) in record.properties() {
                outln!("  {}: {}", format_label(name), format_value(value));
            }
        }
    }
    Ok(())
}

/// A node's identity and record as a JSON object, shared by `get-node` and
/// `list-nodes`.
fn node_to_json(key: &NodeKey, record: &NodeRecord) -> Json {
    let secondary: Vec<Json> = record
        .secondary_labels()
        .iter()
        .map(|l| Json::String(l.clone()))
        .collect();
    let properties: serde_json::Map<String, Json> = record
        .properties()
        .iter()
        .map(|(name, value)| (name.clone(), value_to_json(value)))
        .collect();
    json!({
        "label": key.label(),
        "key": key_tuple_to_json(key.key()),
        "secondary_labels": secondary,
        "properties": Json::Object(properties),
    })
}

fn put_edge(
    repo_path: &Path,
    src_label: &str,
    src_key: &str,
    rtype: &str,
    dst_label: &str,
    dst_key: &str,
) -> Result<()> {
    let repo = open(repo_path)?;
    let src = single_key(src_label, src_key)?;
    let dst = single_key(dst_label, dst_key)?;
    let edge_key = EdgeKey::new(src, rtype, dst, Value::Null).context("building edge key")?;
    let record = EdgeRecord::new(BTreeMap::new());
    let mut txn = repo.begin_write()?;
    txn.put_edge(&edge_key, &record)?;
    txn.save().context("saving workspace")?;
    outln!(
        "put edge {} -{}-> {}",
        format_node_key(edge_key.src()),
        format_label(edge_key.rtype()),
        format_node_key(edge_key.dst()),
    );
    Ok(())
}

fn list_nodes(repo_path: &Path, label: Option<&str>, json: bool) -> Result<()> {
    let repo = open(repo_path)?;
    let snapshot = repo.workspace_snapshot()?;
    let nodes = snapshot.nodes()?;
    if json {
        let rows: Vec<Json> = nodes
            .iter()
            .filter(|(key, _)| !label.is_some_and(|l| l != key.label()))
            .map(|(key, record)| node_to_json(key, record))
            .collect();
        emit_json(&Json::Array(rows));
        return Ok(());
    }
    for (key, _record) in &nodes {
        if label.is_some_and(|l| l != key.label()) {
            continue;
        }
        outln!("{}", format_node_key(key));
    }
    Ok(())
}
