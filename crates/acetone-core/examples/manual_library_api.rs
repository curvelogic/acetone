use std::collections::BTreeMap;

use acetone_core::{InitOptions, Repository, Session};
// Deep access (unstable): schema DDL has no stable-surface entry point yet.
use acetone_core::model::schema::{LabelDef, SchemaEntry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A fresh repository in a temporary directory (use `Repository::open`
    // for an existing one).
    let dir = std::env::temp_dir().join(format!("acetone-api-example-{}", std::process::id()));
    let repo = Repository::init(&dir, InitOptions::default())?;

    // Declare the Host label's natural key, in a write transaction.
    let def = LabelDef::new(vec!["name".into()], BTreeMap::new(), [], [])?;
    let mut txn = repo.begin_write()?;
    txn.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def,
    })?;
    txn.save()?;

    // Write through Cypher: a write query advances the workspace atomically.
    let session = Session::new(&repo);
    session.run("CREATE (:Host {name: 'web1', cpus: 8})")?;

    // Turn the workspace's changes into a commit.
    let commit = repo.begin_write()?.commit("add web1", &[], None)?;
    println!("committed {}", commit.to_hex());

    // Read it back.
    let outcome = session.run("MATCH (h:Host) RETURN h.name, h.cpus")?;
    for row in &outcome.result().rows {
        println!("{row:?}");
    }
    Ok(())
}
