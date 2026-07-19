//! # acetone-core — the acetone library API
//!
//! acetone is an embedded, single-node, **version-controlled labelled property
//! graph**: Dolt-style prolly trees stored in a git-compatible object store,
//! queried with openCypher, operated as a workbench (spec §7, §8). This crate
//! is the **real product surface** — the single dependency a library consumer
//! adds. The `acetone` CLI is a thin client over the same crates.
//!
//! ## Stability (0.2)
//!
//! The **curated headline surface** — the types and functions re-exported flat
//! at this crate root (below) — is **frozen at 0.2** (ADR-0046): it follows
//! semver, additive-only within the 0.2.x series, and a breaking change to it
//! requires 0.3. A committed public-API snapshot
//! (`crates/acetone-core/public-api.txt`) guards this list against silent
//! drift, and `crates/acetone-cypher/public-api.txt` guards the full signatures
//! of the query surface (`Session`, `QueryLimits`, `QueryResult`, `QueryValue`)
//! — the API analogue of the format goldens.
//!
//! ```no_run
//! use acetone_core::{InitOptions, Repository, Session};
//!
//! let repo = Repository::init("graph.git".as_ref(), InitOptions::default())?;
//! let session = Session::new(&repo);
//! let outcome = session.run("MATCH (n) RETURN count(n)")?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Deep access (unstable)
//!
//! The constituent crates are also re-exported as modules — [`graph`],
//! [`model`], [`cypher`], [`store`] — for full access to everything they
//! expose. This **deep-access surface is not part of the 0.2 stability
//! guarantee**: items reachable only through these modules may change in any
//! 0.2.x release. Depend on the flat crate-root re-exports for a stable API;
//! reach into the modules only when you knowingly accept the churn. (The
//! `acetone-cypher` snapshot signature-tracks that crate's whole surface; the
//! `graph`/`model`/`store` deep surfaces are guarded only by the fresh-review
//! gate, not a snapshot — see `STABILITY.md`, which lists the frozen surface
//! and the policy in full.)

#![forbid(unsafe_code)]

// ─── Deep access (unstable — see the crate docs) ────────────────────────────
// The whole constituent-crate surface, for consumers who accept 0.2.x churn.
pub use acetone_cypher as cypher;
pub use acetone_graph as graph;
pub use acetone_model as model;
pub use acetone_store as store;

// ─── The curated headline surface — frozen at 0.2 (ADR-0046) ─────────────────
// Changes here are semver-significant: additive within 0.2.x, breaking → 0.3.

// Repository, transactions, history, and the migrate escape hatch.
pub use acetone_graph::repo::{DEFAULT_BRANCH, DEFAULT_WORKSPACE, InitOptions, LogEntry};
pub use acetone_graph::{
    FormatTransform, GraphError, MigrateReport, Rechunk, Repository, Snapshot, Transaction,
    rewrite_history,
};

// The governed query entry point (ADR-0039) and its caps/result (ADR-0036/0043).
pub use acetone_cypher::exec::{QueryLimits, QueryResult, ResourceLimit};
pub use acetone_cypher::session::{Outcome, QueryError, Session};
// The runtime value type of the query interface — the element type of
// `QueryResult` rows and of the `run_with` parameter map (ADR-0038). Distinct
// from the stored-domain `Value` below: this is what queries *return and take*.
// Re-exported (and thus frozen by name, not only via the cypher snapshot) so
// the query API is nameable end-to-end without reaching into the unstable
// `cypher` module.
pub use acetone_cypher::exec::Value as QueryValue;

// The stored value domain, keys and records — what you put and get.
pub use acetone_model::Value;
pub use acetone_model::graph_keys::{EdgeKey, NodeKey};
pub use acetone_model::records::{EdgeRecord, NodeRecord};

// Store identity and object format.
pub use acetone_store::{Hash, ObjectFormat};
