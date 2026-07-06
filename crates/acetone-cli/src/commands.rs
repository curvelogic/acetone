//! One function per subcommand: call into `acetone-graph`, format output.
//! No graph logic lives here (CLAUDE.md: the CLI is a thin client).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use acetone_graph::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_store::ObjectFormat;
use anyhow::{Context, Result, bail};

use crate::cli::Command;
use crate::value::{format_label, format_value, parse_kv, parse_value, sanitise_line};

use crate::output::outln;

/// Dispatch one parsed command.
pub fn run(repo_path: &Path, command: Command) -> Result<()> {
    match command {
        Command::Init {
            object_format,
            path,
        } => init(repo_path, &object_format, path),
        Command::Status => status(repo_path),
        Command::Commit { message, trailer } => commit(repo_path, &message, &trailer),
        Command::Log => log(repo_path),
        Command::Branch { name } => branch(repo_path, name.as_deref()),
        Command::Checkout { branch: name } => checkout(repo_path, &name),
        Command::DeclareLabel {
            label,
            key,
            require,
            unique,
        } => declare_label(repo_path, &label, &key, &require, &unique),
        Command::DeclareRelType { rtype } => declare_rel_type(repo_path, &rtype),
        Command::PutNode { label, key, prop } => put_node(repo_path, &label, &key, &prop),
        Command::GetNode { label, key } => get_node(repo_path, &label, &key),
        Command::PutEdge {
            src_label,
            src_key,
            rtype,
            dst_label,
            dst_key,
        } => put_edge(
            repo_path, &src_label, &src_key, &rtype, &dst_label, &dst_key,
        ),
        Command::ListNodes { label } => list_nodes(repo_path, label.as_deref()),
        Command::Query { cypher, at, format } => {
            let format = crate::query::Format::parse(&format)?;
            crate::query::run(repo_path, &cypher, at.as_deref(), format)
        }
        Command::Shell => crate::query::shell(repo_path),
        Command::Fsck => fsck(repo_path),
    }
}

fn fsck(repo_path: &Path) -> Result<()> {
    let repo = open(repo_path)?;
    let report = acetone_graph::fsck::check(&repo)?;
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

fn init(repo_path: &Path, object_format: &str, path: Option<PathBuf>) -> Result<()> {
    let target = path.unwrap_or_else(|| repo_path.to_owned());
    let object_format = match object_format {
        "sha1" => ObjectFormat::Sha1,
        "sha256" => ObjectFormat::Sha256,
        // Unreachable: clap's value_parser restricts the flag to these two.
        other => bail!("unsupported object format {other:?}"),
    };
    let mut options = InitOptions::default();
    options.object_format = object_format;
    Repository::init(&target, options)
        .with_context(|| format!("initialising repository at {}", target.display()))?;
    outln!(
        "Initialized empty acetone repository in {}",
        target.display()
    );
    Ok(())
}

fn open(repo_path: &Path) -> Result<Repository> {
    Repository::open(repo_path)
        .with_context(|| format!("opening repository at {}", repo_path.display()))
}

fn status(repo_path: &Path) -> Result<()> {
    let repo = open(repo_path)?;
    match repo.current_branch()? {
        Some(branch) => {
            let short = branch
                .strip_prefix(acetone_graph::repo::BRANCH_REF_PREFIX)
                .unwrap_or(&branch);
            outln!("On branch {short}");
        }
        None => outln!("Not on any branch (detached)"),
    }
    match repo.head_commit()? {
        Some(head) => outln!("HEAD: {}", head.to_hex()),
        None => outln!("HEAD: (no commits yet)"),
    }
    outln!(
        "workspace: {}",
        if repo.is_dirty()? { "dirty" } else { "clean" }
    );
    let snapshot = repo.workspace_snapshot()?;
    outln!(
        "nodes: {}, edges: {}, schema entries: {}",
        snapshot.nodes()?.len(),
        snapshot.edges()?.len(),
        snapshot.schema_entries()?.len(),
    );
    Ok(())
}

fn commit(repo_path: &Path, message: &str, trailers: &[String]) -> Result<()> {
    let repo = open(repo_path)?;
    // Thin-client guard: acetone_graph::Transaction::commit has no
    // no-change guard of its own yet (library-level fix tracked
    // separately), so a bare `commit` on an already-committed workspace
    // would otherwise silently mint a pointless commit every time it is
    // run. This also refuses an empty root commit on a brand new
    // repository, which is the CLI's help text for `commit` documents as
    // accepted Phase-1 behaviour.
    if !repo.is_dirty()? {
        bail!("nothing to commit (workspace matches HEAD)");
    }
    let trailers: Vec<(String, String)> = trailers
        .iter()
        .map(|raw| parse_kv(raw, "--trailer").map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect::<Result<_>>()?;
    let txn = repo.begin_write()?;
    let id = txn
        .commit(message, &trailers, None)
        .context("committing workspace")?;
    outln!("committed {}", id.to_hex());
    Ok(())
}

fn log(repo_path: &Path) -> Result<()> {
    let repo = open(repo_path)?;
    for entry in repo.log(None)? {
        // Commit messages and trailers are raw bytes from potentially
        // hostile clones (lossily decoded, not constrained by git):
        // sanitise before they reach the terminal.
        let subject = entry.message.lines().next().unwrap_or("");
        outln!("{} {}", entry.id.to_hex(), sanitise_line(subject));
        for (key, value) in &entry.trailers {
            outln!("    {}: {}", sanitise_line(key), sanitise_line(value));
        }
    }
    Ok(())
}

fn branch(repo_path: &Path, name: Option<&str>) -> Result<()> {
    let repo = open(repo_path)?;
    match name {
        None => {
            let current = repo.current_branch()?;
            for (short, _hash) in repo.branches()? {
                let full = format!("{}{short}", acetone_graph::repo::BRANCH_REF_PREFIX);
                let marker = if current.as_deref() == Some(full.as_str()) {
                    "*"
                } else {
                    " "
                };
                outln!("{marker} {short}");
            }
        }
        Some(name) => {
            let target = repo
                .create_branch(name, None)
                .with_context(|| format!("creating branch {name:?}"))?;
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

fn single_key(label: &str, key: &str) -> Result<NodeKey> {
    NodeKey::new(label, vec![parse_value(key)])
        .with_context(|| format!("building key for label {label:?}"))
}

fn declare_label(
    repo_path: &Path,
    label: &str,
    key: &[String],
    require: &[String],
    unique: &[String],
) -> Result<()> {
    use acetone_model::schema::{LabelDef, SchemaEntry};
    let def = LabelDef::new(
        key.to_vec(),
        BTreeMap::new(),
        require.to_vec(),
        unique.to_vec(),
    )
    .with_context(|| format!("declaring schema for label {label:?}"))?;
    let entry = SchemaEntry::Label {
        name: label.to_owned(),
        def,
    };
    let repo = open(repo_path)?;
    let mut txn = repo.begin_write()?;
    txn.put_schema(&entry)?;
    txn.save().context("saving workspace")?;
    outln!(
        "declared label {} key [{}]",
        format_label(label),
        key.join(", ")
    );
    Ok(())
}

fn declare_rel_type(repo_path: &Path, rtype: &str) -> Result<()> {
    use acetone_model::schema::{RelTypeDef, SchemaEntry};
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

fn put_node(repo_path: &Path, label: &str, key: &str, props: &[String]) -> Result<()> {
    let repo = open(repo_path)?;
    let node_key = single_key(label, key)?;
    let mut properties = BTreeMap::new();
    for raw in props {
        let (name, value) = parse_kv(raw, "--prop")?;
        properties.insert(name.to_owned(), parse_value(value));
    }
    let record = NodeRecord::new(std::iter::empty::<String>(), properties);
    let mut txn = repo.begin_write()?;
    txn.put_node(&node_key, &record)?;
    txn.save().context("saving workspace")?;
    outln!("put node {}", format_node_key(&node_key));
    Ok(())
}

/// `Label [key, ...]`, escaped — the one place a node key is rendered, used
/// by every command that echoes one.
fn format_node_key(key: &NodeKey) -> String {
    let key_repr: Vec<String> = key.key().iter().map(format_value).collect();
    format!("{} [{}]", format_label(key.label()), key_repr.join(", "))
}

fn get_node(repo_path: &Path, label: &str, key: &str) -> Result<()> {
    let repo = open(repo_path)?;
    let node_key = single_key(label, key)?;
    let snapshot = repo.workspace_snapshot()?;
    match snapshot.get_node(&node_key)? {
        None => outln!("not found"),
        Some(record) => {
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

fn list_nodes(repo_path: &Path, label: Option<&str>) -> Result<()> {
    let repo = open(repo_path)?;
    let snapshot = repo.workspace_snapshot()?;
    for (key, _record) in snapshot.nodes()? {
        if label.is_some_and(|l| l != key.label()) {
            continue;
        }
        outln!("{}", format_node_key(&key));
    }
    Ok(())
}
