//! # acetone-core — the acetone library API
//!
//! acetone is an embedded, single-node, **version-controlled labelled property
//! graph**: Dolt-style prolly trees stored in a git-compatible object store,
//! queried with openCypher, operated as a workbench (spec §7, §8). This crate
//! is the **real product surface** — the single dependency a library consumer
//! adds. The `acetone` CLI is a thin client over the same crates (routing it
//! through this façade is a planned tidy-up).
//!
//! The constituent crates are re-exported as modules for full access:
//!
//! - [`graph`] — [`Repository`], transactions, merge, diff, `fsck`, import,
//!   `migrate` (the main entry point);
//! - [`model`] — [`Value`], node/edge keys and records, schema, the manifest;
//! - [`cypher`] — the openCypher parser, binder and executor;
//! - [`store`] — the `ChunkStore` trait and its git object-database backend.
//!
//! The headline types are also re-exported at the crate root for convenience.
//!
//! ```no_run
//! use acetone_core::{InitOptions, Repository};
//!
//! let repo = Repository::init("graph.git".as_ref(), InitOptions::default())?;
//! # Ok::<(), acetone_core::GraphError>(())
//! ```

#![forbid(unsafe_code)]

pub use acetone_cypher as cypher;
pub use acetone_graph as graph;
pub use acetone_model as model;
pub use acetone_store as store;

// Headline types — the everyday surface, flat at the crate root.
pub use acetone_cypher::session::{Outcome, QueryError, Session};
pub use acetone_graph::repo::{DEFAULT_BRANCH, DEFAULT_WORKSPACE, InitOptions, LogEntry};
pub use acetone_graph::{
    FormatTransform, GraphError, MigrateReport, Rechunk, Repository, Snapshot, Transaction,
    rewrite_history,
};
pub use acetone_model::Value;
pub use acetone_model::graph_keys::{EdgeKey, NodeKey};
pub use acetone_model::records::{EdgeRecord, NodeRecord};
pub use acetone_store::{Hash, ObjectFormat};
