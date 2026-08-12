//! The structured change report (acetone-zavr.7): the diff engine's
//! natural product — a PR-style artefact a curation surface can render,
//! built from `from..to`'s property-level changes plus the workspace's
//! merge conflicts, as one JSON document or its markdown rendering.
//!
//! Thin client over reviewed machinery: the change list is
//! [`Repository::diff`]'s before/after records compared property by
//! property; the conflicts section reuses `CALL acetone.conflicts()`
//! verbatim (the reviewed cell/graph-violation derivation with live
//! re-derivation, ADR-0058) rather than re-deriving over encoded keys.
//! Deterministic by construction — commit hashes, never wall-clock.

use std::collections::BTreeSet;

use acetone_core::cypher::session::Session;
use acetone_core::graph::Repository;
use acetone_core::graph::diff::ChangeKind;
use acetone_core::model::Value;
use acetone_core::store::CommitStore;
use anyhow::{Context, Result};
use serde_json::{Map, Value as Json, json};

use crate::json::value_to_json;

/// Build the report document for `from..to` on this repository. The
/// `conflicts` section reflects the WORKSPACE's merge state (null when no
/// merge is in progress) — conflicts are workspace state, orthogonal to
/// the two report endpoints, and are included so one document carries
/// everything a review surface needs mid-merge.
pub(crate) fn build(repo: &Repository, from: &str, to: &str) -> Result<Json> {
    let diff = repo
        .diff(from, to)
        .with_context(|| format!("diffing {from:?}..{to:?}"))?;

    let mut nodes = Vec::new();
    let mut summary = [0usize; 6]; // n+ n- n~ e+ e- e~
    for change in &diff.nodes {
        let before = change.before.as_ref().map(|r| r.properties());
        let after = change.after.as_ref().map(|r| r.properties());
        summary[kind_index(change.kind)] += 1;
        nodes.push(json!({
            "kind": kind_name(change.kind),
            "label": change.key.label(),
            "key": crate::json::key_tuple_to_json(change.key.key()),
            "properties": property_deltas(before, after),
        }));
    }
    let mut edges = Vec::new();
    for change in &diff.edges {
        let before = change.before.as_ref().map(|r| r.properties());
        let after = change.after.as_ref().map(|r| r.properties());
        summary[3 + kind_index(change.kind)] += 1;
        let key = &change.key;
        edges.push(json!({
            "kind": kind_name(change.kind),
            "rel_type": key.rtype(),
            "src": {
                "label": key.src().label(),
                "key": crate::json::key_tuple_to_json(key.src().key()),
            },
            "dst": {
                "label": key.dst().label(),
                "key": crate::json::key_tuple_to_json(key.dst().key()),
            },
            "disc": value_to_json(key.disc()),
            "properties": property_deltas(before, after),
        }));
    }

    Ok(json!({
        "report_version": 1,
        "from": endpoint(repo, from)?,
        "to": endpoint(repo, to)?,
        "nodes": nodes,
        "edges": edges,
        "summary": {
            "nodes_added": summary[0],
            "nodes_removed": summary[1],
            "nodes_modified": summary[2],
            "edges_added": summary[3],
            "edges_removed": summary[4],
            "edges_modified": summary[5],
        },
        "conflicts": conflicts_section(repo)?,
    }))
}

/// Serialise the document exactly as `emit_json` prints it, so the CLI's
/// stdout and the daemon's chunk stream carry identical bytes.
pub(crate) fn rendered_json(doc: &Json) -> String {
    let text = serde_json::to_string_pretty(doc).unwrap_or_else(|_| "null".into());
    crate::json::escape_residual_controls(&text)
}

fn kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
    }
}

fn kind_index(kind: ChangeKind) -> usize {
    match kind {
        ChangeKind::Added => 0,
        ChangeKind::Removed => 1,
        ChangeKind::Modified => 2,
    }
}

/// One report endpoint: the refspec as given, resolved to its commit and
/// that commit's subject line.
fn endpoint(repo: &Repository, refspec: &str) -> Result<Json> {
    let id = repo
        .resolve_commit(refspec)
        .with_context(|| format!("resolving {refspec:?}"))?;
    let subject = repo
        .store()
        .read_commit(&id)?
        .map(|commit| commit.message.lines().next().unwrap_or("").to_owned())
        .unwrap_or_default();
    Ok(json!({
        "refspec": refspec,
        "commit": id.to_hex(),
        "subject": subject,
    }))
}

/// The per-property delta map: every property name present on either side
/// whose value differs, as `{name: {before?, after?}}` — an added entity
/// carries only afters, a removed one only befores, a modified one only
/// the properties that actually changed.
fn property_deltas(
    before: Option<&std::collections::BTreeMap<String, Value>>,
    after: Option<&std::collections::BTreeMap<String, Value>>,
) -> Json {
    let names: BTreeSet<&String> = before
        .iter()
        .flat_map(|m| m.keys())
        .chain(after.iter().flat_map(|m| m.keys()))
        .collect();
    let mut out = Map::new();
    for name in names {
        let b = before.and_then(|m| m.get(name));
        let a = after.and_then(|m| m.get(name));
        if b == a {
            continue;
        }
        let mut cell = Map::new();
        if let Some(b) = b {
            cell.insert("before".into(), value_to_json(b));
        }
        if let Some(a) = a {
            cell.insert("after".into(), value_to_json(a));
        }
        out.insert(name.clone(), Json::Object(cell));
    }
    Json::Object(out)
}

/// The conflicts section: null when no merge is in progress, else the rows
/// of `CALL acetone.conflicts()` — kind, label, key, property, and the
/// three-way base/ours/theirs values — serialised in yield order. Reuses
/// the session procedure so the report and the query surface can never
/// disagree about what is conflicted.
fn conflicts_section(repo: &Repository) -> Result<Json> {
    if repo.merge_head()?.is_none() {
        return Ok(Json::Null);
    }
    let outcome = Session::new(repo)
        .run(
            "CALL acetone.conflicts() \
             YIELD kind, label, key, property, base, ours, theirs \
             RETURN kind, label, key, property, base, ours, theirs",
        )
        .context("deriving the merge's conflicts")?;
    let result = outcome.result();
    let items: Vec<Json> = result
        .rows
        .iter()
        .map(|row| {
            let mut item = Map::new();
            for (name, value) in result.columns.iter().zip(row) {
                // The same JSON rendering `query --format json` uses, so
                // the report and the query surface serialise identically.
                let rendered = crate::query::json_value(value);
                item.insert(
                    name.clone(),
                    serde_json::from_str(&rendered).unwrap_or(Json::Null),
                );
            }
            Json::Object(item)
        })
        .collect();
    Ok(json!({ "in_progress": true, "items": items }))
}

/// The human artefact: markdown a reviewer pastes into a PR or reads in a
/// terminal. Property VALUES render through JSON serialisation (quoted,
/// control bytes escaped); free text (subjects, labels, property names) is
/// embedded raw — the artefact is data, and the DISPLAYING side sanitises
/// at its own boundary (the CLI sanitises line-wise before its terminal;
/// the daemon streams it raw to the peer, the fsck-findings precedent).
pub(crate) fn render_markdown(doc: &Json) -> String {
    let mut out = String::new();
    let endpoint = |side: &str| {
        let subject = doc[side]["subject"].as_str().unwrap_or("");
        let commit = doc[side]["commit"].as_str().unwrap_or("");
        let short = &commit[..commit.len().min(12)];
        format!("{subject} ({short})")
    };
    out.push_str(&format!(
        "# Change report: {} \u{2192} {}\n\n",
        endpoint("from"),
        endpoint("to")
    ));
    let s = &doc["summary"];
    out.push_str(&format!(
        "nodes: +{} \u{2212}{} ~{} \u{00b7} edges: +{} \u{2212}{} ~{}\n",
        s["nodes_added"],
        s["nodes_removed"],
        s["nodes_modified"],
        s["edges_added"],
        s["edges_removed"],
        s["edges_modified"]
    ));

    let sign = |kind: &str| match kind {
        "added" => '+',
        "removed" => '-',
        _ => '~',
    };
    let props = |change: &Json, out: &mut String| {
        if let Some(map) = change["properties"].as_object() {
            for (name, cell) in map {
                let arrow = match (cell.get("before"), cell.get("after")) {
                    (Some(b), Some(a)) => format!("{b} \u{2192} {a}"),
                    (None, Some(a)) => format!("{a}"),
                    (Some(b), None) => format!("{b} \u{2192} (removed)"),
                    (None, None) => String::new(),
                };
                out.push_str(&format!("    - `{name}`: {arrow}\n"));
            }
        }
    };
    if let Some(nodes) = doc["nodes"].as_array()
        && !nodes.is_empty()
    {
        out.push_str("\n## Nodes\n\n");
        for change in nodes {
            let kind = change["kind"].as_str().unwrap_or("");
            out.push_str(&format!(
                "{} `{}` {}\n",
                sign(kind),
                change["label"].as_str().unwrap_or(""),
                change["key"]
            ));
            props(change, &mut out);
        }
    }
    if let Some(edges) = doc["edges"].as_array()
        && !edges.is_empty()
    {
        out.push_str("\n## Edges\n\n");
        for change in edges {
            let kind = change["kind"].as_str().unwrap_or("");
            out.push_str(&format!(
                "{} `{}` {} {} \u{2192} `{}` {}\n",
                sign(kind),
                change["src"]["label"].as_str().unwrap_or(""),
                change["src"]["key"],
                change["rel_type"].as_str().unwrap_or(""),
                change["dst"]["label"].as_str().unwrap_or(""),
                change["dst"]["key"],
            ));
            props(change, &mut out);
        }
    }
    if let Some(conflicts) = doc["conflicts"].as_object() {
        out.push_str("\n## Conflicts (merge in progress)\n\n");
        if let Some(items) = conflicts["items"].as_array() {
            for item in items {
                out.push_str(&format!(
                    "- {} `{}` {}: `{}` base {} \u{00b7} ours {} \u{00b7} theirs {}\n",
                    item["kind"].as_str().unwrap_or(""),
                    item["label"].as_str().unwrap_or(""),
                    item["key"],
                    item["property"].as_str().unwrap_or("(record)"),
                    item["base"],
                    item["ours"],
                    item["theirs"],
                ));
            }
        }
    }
    out
}
