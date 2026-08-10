//! `acetone serve` — the per-repository daemon (ADR-0074, `acetone-pz0k`).
//!
//! A `0600` unix domain socket speaking the length-prefixed JSON frame
//! protocol (ADR-0074) — versioned hello, then the `query` verb (read
//! AND write) with streamed rows, the advisory channel, and typed
//! terminal frames (a write's terminal `ok` carries its summary counts;
//! a write may opt into relationship-type coinage via
//! `params.autodeclare`), a read-only `status` verb, and the first
//! payload verbs `schema-apply` and `import` (the document/source
//! streams in as `chunk` frames — no path over the wire, ADR-0074 §4)
//! — under exactly the CLI's per-query budgets, bounded by
//! `--max-concurrent`. One writer
//! wins the single-writer lock; a concurrent write returns a typed
//! `graph` (locked) error to retry, exactly as two concurrent CLI
//! processes would. On a write that hits a lock left by a SIGKILLed
//! writer, the daemon (only) breaks it if its pid is dead and retries
//! once (ADR-0074 §8). The ref-advancing verbs `commit`/`branch`/
//! `checkout`/`merge`/`resolve` operate on the ONE shared per-worktree
//! workspace — a connection is a view onto it, like concurrent CLI
//! processes (per-connection isolated sessions are anticipated future
//! work).
//!
//! Security model (ADR-0074 §1): the socket IS the authentication — the
//! kernel enforces `0600`; the daemon holds no auth code and trusts every
//! connected peer, treating only their *data* as untrusted.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value as Json, json};

use acetone_core::cypher::session::{Outcome, QueryError, Session};
use acetone_core::graph::GraphError;
use acetone_core::graph::lock::{StaleLockOutcome, break_stale_lock};
use acetone_core::graph::repo::Repository;

/// Serialises stale-writer-lock recovery across this daemon's connection
/// threads (ADR-0074 §8): with at most one recoverer active in the process,
/// the double-recoverer race — two threads each unlinking and recreating the
/// lock — cannot occur, so no thread ever removes another's freshly-acquired
/// lock. This holds **within one daemon process**. It relies on ADR-0074's
/// one-daemon-per-repository model: two daemons on ONE repository (started on
/// different sockets) share no mutex, and `remove_file` in `break_stale_lock`
/// is unconditional, so they could reopen the double-writer window — that
/// configuration is UNSUPPORTED and would need an enforced daemon-exclusivity
/// lock or a stale-file-immune (flock-style) writer lock (ADR §8, a filed
/// decision). A plain `()` mutex, held only for the brief break decision,
/// never across a write.
static LOCK_RECOVERY: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Break a stale writer lock and report whether the caller should retry
/// (ADR-0074 §8), serialised behind [`LOCK_RECOVERY`]. Returns `true` only
/// when the lock was genuinely stale (dead pid) and removed, or was already
/// gone — never when a live process holds it.
fn recover_stale_writer_lock(repo: &Repository) -> bool {
    let _guard = LOCK_RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
    matches!(
        break_stale_lock(repo.store().git_dir()),
        Ok(StaleLockOutcome::Broken | StaleLockOutcome::Absent)
    )
}

/// Run a write operation and, if it failed on the single-writer lock left by
/// a dead writer, break that lock and retry ONCE (ADR-0074 §8). Every
/// lock-taking daemon verb — `query` writes, `import`, and the ref-advancing
/// verbs — goes through this, so a SIGKILLed writer's stale lock never
/// crash-loops the daemon whatever the first write is (PR #270 review). A
/// *live* lock is NOT retried — `recover_stale_writer_lock` returns false and
/// short-circuits the retry — so genuine contention returns its typed error
/// to the client unchanged, and a live lock is never broken.
fn with_lock_recovery<T, E>(
    repo: &Repository,
    is_locked: impl Fn(&E) -> bool,
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let first = op();
    match &first {
        Err(e) if is_locked(e) && recover_stale_writer_lock(repo) => op(),
        _ => first,
    }
}

/// Whether a `GraphError` is the single-writer-lock conflict.
fn is_graph_locked(e: &GraphError) -> bool {
    matches!(e, GraphError::Locked { .. })
}

/// Frames above this are refused at the framing layer, before any parse
/// (ADR-0074 §2 — the import-bound precedent applied to the wire).
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// The protocol major this build speaks (changes only by ADR).
const PROTOCOL: u64 = 1;

/// Cap on simultaneously-open connections (ADR-0074 §6, PR #259 review
/// major 3): thread-per-connection with no cap let a peer exhaust threads
/// and memory by opening sockets that send nothing. The host owns
/// admission above this.
const MAX_CONNECTIONS: usize = 256;

/// Idle/stall IO timeout, in seconds, for both read and write on a
/// connection: a peer that opens a socket and does not speak (no hello,
/// a truncated frame) or that sends a query and stops READING its result
/// (starving a query permit) must not park a thread forever (PR #259
/// review majors 2/4 + M2). Overridable via `ACETONE_SERVE_IO_TIMEOUT_SECS`
/// so tests can exercise the release deterministically without waiting the
/// production default.
fn io_timeout() -> std::time::Duration {
    let secs = std::env::var("ACETONE_SERVE_IO_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(30);
    std::time::Duration::from_secs(secs)
}

/// Per-query result-row ceiling for the daemon — well below the library
/// default (1,000,000) because the daemon materialises the whole result
/// and its peak is summed across `--max-concurrent` (ADR-0074 §6).
const DAEMON_MAX_RESULT_ROWS: u64 = 100_000;

pub fn serve(
    repo_path: &Path,
    socket: &Path,
    max_concurrent: usize,
    timeout_secs: u64,
) -> Result<()> {
    if max_concurrent == 0 {
        bail!("--max-concurrent must be at least 1");
    }
    // Validate the repository before binding, so a bad --repo fails
    // fast. Each connection then opens its OWN Repository handle —
    // gitoxide's internals are not Send, and per-connection handles are
    // the honest model anyway: concurrent connections behave exactly
    // like concurrent CLI processes (unlimited MVCC readers, one writer
    // via the existing lock).
    drop(Repository::open(repo_path).context("opening repository")?);
    let repo_path = repo_path.to_path_buf();

    // Reclaim a stale socket: if the path exists but nothing accepts on
    // it (a crashed daemon's leftover — the ADR §8 crash-loop, arriving
    // via §7), unlink and rebind. A LIVE daemon is refused, so two never
    // serve one path (PR #259 review major 5). The host owns the socket
    // path (ADR-0074 §7), so a *non-socket* file there is the host's
    // mistake to make; reclaim still refuses to remove one that another
    // process is serving, and only unlinks a genuinely dead endpoint.
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => bail!(
                "socket path {} is already served by a live daemon",
                socket.display()
            ),
            Err(_) => std::fs::remove_file(socket)
                .with_context(|| format!("removing the stale socket {}", socket.display()))?,
        }
    }

    // Bind inside a private 0700 staging directory, then atomically
    // rename the socket into place — so the socket is NEVER
    // world-connectable, closing the bind→chmod TOCTOU in which the
    // kernel ACL (the daemon's ONLY authentication, ADR-0074 §1) does not
    // yet hold, INCLUDING on the staging path (PR #259 review major 1 and
    // its residual): the staging dir's 0700 makes the socket unreachable
    // from the instant of bind, whatever the umask, with no FFI.
    // `rename` within a filesystem is atomic, and the daemon has not
    // begun accepting, so no peer can connect to the temp path either.
    let staging_dir = socket.with_extension(format!("staging.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging_dir);
    std::fs::create_dir(&staging_dir)
        .with_context(|| format!("creating the staging dir {}", staging_dir.display()))?;
    std::fs::set_permissions(&staging_dir, std::fs::Permissions::from_mode(0o700))
        .context("restricting the staging dir to 0700")?;
    let staging = staging_dir.join("s");
    let listener =
        UnixListener::bind(&staging).with_context(|| format!("binding {}", staging.display()))?;
    // Belt-and-braces on the socket itself, before it leaves the 0700 dir.
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
        .context("restricting the socket to 0600")?;
    let published = std::fs::rename(&staging, socket);
    let _ = std::fs::remove_dir_all(&staging_dir);
    published.with_context(|| format!("publishing the socket at {}", socket.display()))?;
    let _guard = SocketGuard(socket.to_path_buf());

    // Readiness handshake: exactly one line on stdout (ADR-0074 §7).
    println!(
        "{}",
        json!({"ready": true, "pid": std::process::id(), "protocol": PROTOCOL})
    );
    std::io::stdout().flush().ok();

    // `--max-concurrent` bounds simultaneous query EXECUTION (summed
    // per-query budgets in one address space, ADR-0074 §6);
    // `MAX_CONNECTIONS` separately bounds open connections so an idle
    // peer cannot exhaust threads (PR #259 review majors 2/3).
    let query_permits = Arc::new(Semaphore::new(max_concurrent));
    let conn_permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            // EMFILE and friends: at the fd limit. Back off rather than
            // spin, and never die on a transient accept error.
            Err(e) => {
                eprintln!("accept error: {e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };
        // Refuse politely at the connection cap rather than spawning an
        // unbounded thread; the permit lives for the whole handler.
        let Some(conn_permit) = conn_permits.try_acquire() else {
            let _ = write_frame(
                &mut stream,
                &json!({"error": {
                    "kind": "busy",
                    "message": "the daemon is at its connection limit; retry",
                }}),
            );
            continue;
        };
        let query_permits = Arc::clone(&query_permits);
        let repo_path = repo_path.clone();
        // A spawn failure must not unwind the accept loop: drop the
        // permit (by not moving it in) and keep serving.
        let spawned = std::thread::Builder::new().spawn(move || {
            let _conn_permit = conn_permit; // released when the handler ends
            // A peer that never speaks — or that sends a query and then
            // stops READING its large result — must not park this thread,
            // and with it a query permit, forever (PR #259 review M2 +
            // re-review): symmetric read AND write timeouts. The write
            // timeout is the half that stops the slow-reader permit
            // starvation, since result frames go out under the query
            // permit.
            let timeout = io_timeout();
            let deadlines = stream
                .set_read_timeout(Some(timeout))
                .and(stream.set_write_timeout(Some(timeout)));
            if deadlines.is_err() {
                let _ = write_frame(
                    &mut stream,
                    &json!({"error": {"kind": "internal", "message": "socket setup failed"}}),
                );
                return;
            }
            let repo = match Repository::open(&repo_path) {
                Ok(r) => r,
                Err(e) => {
                    // Answer with a frame, never a bare EOF.
                    let _ = write_frame(
                        &mut stream,
                        &json!({"error": {
                            "kind": "internal",
                            "message": format!("could not open repository: {e}"),
                        }}),
                    );
                    return;
                }
            };
            if let Err(e) = connection(stream, &repo, &query_permits, timeout_secs) {
                // A broken/idle peer is routine, not a daemon error.
                eprintln!("connection ended: {e:#}");
            }
        });
        if spawned.is_err() {
            eprintln!("could not spawn a connection handler; backing off");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    Ok(())
}

/// Best-effort unlink on shutdown paths that drop the listener.
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn connection(
    mut stream: UnixStream,
    repo: &Repository,
    permits: &Semaphore,
    timeout_secs: u64,
) -> Result<()> {
    // Hello, both ways (ADR-0074 §3). The daemon speaks first so a
    // client can fail fast on a protocol mismatch without composing one.
    write_frame(
        &mut stream,
        &json!({"acetone": {"protocol": PROTOCOL, "version": env!("CARGO_PKG_VERSION")}}),
    )?;
    let hello = read_frame(&mut stream)?.context("peer closed before hello")?;
    let peer_protocol = hello
        .pointer("/acetone/protocol")
        .and_then(Json::as_u64)
        .unwrap_or(0);
    if peer_protocol != PROTOCOL {
        write_frame(
            &mut stream,
            &json!({"error": {
                "kind": "protocol-mismatch",
                "message": format!("this daemon speaks protocol {PROTOCOL}"),
            }}),
        )?;
        return Ok(());
    }

    // Request loop: one request at a time per connection (a client wanting
    // parallelism opens connections; admission is the host's business).
    while let Some(request) = read_frame(&mut stream)? {
        let id = request.get("id").cloned().unwrap_or(Json::Null);
        let verb = request.get("verb").and_then(Json::as_str).unwrap_or("");
        match verb {
            "query" => {
                let _permit = permits.acquire();
                run_query(&mut stream, repo, &request, &id, timeout_secs)?;
            }
            // A read-only snapshot of the workspace state (branch, head,
            // dirty, counts) — the socket equivalent of `acetone status`
            // (acetone-pz0k.4). Cheap; still takes a query permit so a burst
            // of `status` cannot bypass the concurrency bound.
            "status" => {
                let _permit = permits.acquire();
                run_status(&mut stream, repo, &id)?;
            }
            // The first payload verb (acetone-pz0k.3): the schema document
            // arrives as a stream of `chunk` text frames after the request —
            // NO PATHS OVER THE WIRE (ADR-0074 §4): a path param would let
            // any socket peer read anything the daemon's uid can. `import`
            // (bytes) follows the same protocol in its own unit.
            "schema-apply" => {
                let _permit = permits.acquire();
                run_schema_apply(&mut stream, repo, &request, &id)?;
            }
            // Streamed import (acetone-pz0k.4): the source bytes arrive as
            // chunk frames — NO path over the wire — and are staged to a
            // daemon-private temp file the peer never names, then run
            // through the same import path as the CLI.
            "import" => {
                let _permit = permits.acquire();
                run_import(&mut stream, repo, &request, &id)?;
            }
            // The ref-advancing verbs (acetone-pz0k.5). Greg's decision: a
            // connection is a view onto the ONE shared per-worktree workspace,
            // exactly like concurrent CLI processes — a `commit` commits
            // whatever is staged, a `checkout` moves the HEAD every connection
            // sees, a merge-in-progress is shared. (Per-connection isolated
            // sessions are anticipated future work; the verbs here are thin
            // wrappers over the same `Repository` methods, so that mode is a
            // matter of which workspace ref a connection resolves, not a
            // protocol change.) All take a query permit.
            "commit" => {
                let _permit = permits.acquire();
                run_commit(&mut stream, repo, &request, &id)?;
            }
            "branch" => {
                let _permit = permits.acquire();
                run_branch(&mut stream, repo, &request, &id)?;
            }
            "checkout" => {
                let _permit = permits.acquire();
                run_checkout(&mut stream, repo, &request, &id)?;
            }
            "merge" => {
                let _permit = permits.acquire();
                run_merge(&mut stream, repo, &request, &id)?;
            }
            "resolve" => {
                let _permit = permits.acquire();
                run_resolve(&mut stream, repo, &request, &id)?;
            }
            other => {
                write_frame(
                    &mut stream,
                    &json!({"id": id, "error": {
                        "kind": "unknown-verb",
                        "message": format!(
                            "verb {other:?} is not served (this build serves \"query\" \
                             (read and write), \"status\", \"schema-apply\", \"import\", \
                             \"commit\", \"branch\", \"checkout\", \"merge\" and \"resolve\")"
                        ),
                    }}),
                )?;
            }
        }
    }
    Ok(())
}

fn run_query(
    stream: &mut UnixStream,
    repo: &Repository,
    request: &Json,
    id: &Json,
    timeout_secs: u64,
) -> Result<()> {
    let Some(cypher) = request.pointer("/params/cypher").and_then(Json::as_str) else {
        return write_frame(
            stream,
            &json!({"id": id, "error": {
                "kind": "bad-request",
                "message": "query needs params.cypher (a string)",
            }}),
        );
    };
    // Writes are served (acetone-pz0k.2): `run_with` dispatches read vs
    // write. The single-writer lock is fail-fast (O_EXCL, never blocks),
    // so a concurrent write does NOT queue — one wins, the other returns
    // a typed `graph` (locked) error to retry, exactly as two concurrent
    // CLI processes would. The one still-deferred hazard is a SIGKILLed daemon
    // leaving a held lock; that is the pre-existing manual-recovery
    // situation the CLI already has, and its automatic recovery is
    // ADR-0074 §8's own later unit, not a prerequisite for serving
    // writes here.
    // `params.autodeclare` (default false) opts a write into relationship-type
    // coinage (ADR-0060), exactly as the CLI's `query --autodeclare` — so a
    // non-Rust client can coin over the socket. It only affects writes; a read
    // never coins.
    let autodeclare = request
        .pointer("/params/autodeclare")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let session = Session::new(repo).autodeclare(autodeclare);
    // A daemon materialises the whole result before streaming it, and a
    // long-lived process does not hand back the peak the way
    // process-per-command does; at `--max-concurrent` the worst case is
    // that peak SUMMED. So the daemon caps result rows well below the
    // library default (1,000,000) — the honest per-query memory ceiling
    // named in ADR-0074 §6 (PR #259 review, final note).
    let limits =
        crate::query::cli_limits(timeout_secs).with_max_result_rows(DAEMON_MAX_RESULT_ROWS);
    // Daemon-only stale-writer-lock recovery (ADR-0074 §8): a write that hit
    // a lock left by a SIGKILLed writer would otherwise crash-loop the daemon
    // — break a dead-pid lock and retry once (the same helper the ref verbs
    // use). The CLI never does this.
    let outcome = with_lock_recovery(
        repo,
        |e: &QueryError| matches!(e, QueryError::Graph(GraphError::Locked { .. })),
        || session.run_with(cypher, &BTreeMap::new(), &limits),
    );
    match outcome {
        // Both outcomes stream rows the same way (a write may RETURN);
        // the terminal `ok` frame carries the write-summary counts for a
        // write and just the row count for a read.
        Ok(outcome) => {
            let is_write = outcome.is_write();
            let (Outcome::Read(result) | Outcome::Write(result)) = outcome;
            for row in &result.rows {
                let values: Vec<Json> = row
                    .iter()
                    .map(|v| {
                        serde_json::from_str(&crate::query::json_value(v)).unwrap_or(Json::Null)
                    })
                    .collect();
                // An over-cap RESULT frame must not leave the client on a
                // bare EOF (PR #259 review major 7): convert the framing
                // refusal into a typed terminal error so the contract's
                // "exactly one ok/error" holds.
                let frame = json!({"id": id, "row": {
                    "columns": result.columns, "values": values,
                }});
                if let Err(e) = write_frame(stream, &frame) {
                    return write_frame(
                        stream,
                        &json!({"id": id, "error": {
                            "kind": "result-too-large",
                            "message": format!("a result row exceeds the wire frame cap: {e}"),
                        }}),
                    );
                }
            }
            for advisory in &result.advisories {
                write_frame(stream, &json!({"id": id, "advisory": advisory}))?;
            }
            let mut ok = json!({"rows": result.rows.len()});
            if is_write {
                let s = &result.stats;
                ok["write"] = json!({
                    "nodes_created": s.nodes_created,
                    "relationships_created": s.relationships_created,
                    "properties_set": s.properties_set,
                    "labels_added": s.labels_added,
                    "labels_removed": s.labels_removed,
                    "nodes_deleted": s.nodes_deleted,
                    "relationships_deleted": s.relationships_deleted,
                });
            }
            write_frame(stream, &json!({"id": id, "ok": ok}))
        }
        // Typed error kinds mapped from the QueryError variant, with the
        // CLI's span-aware rendering (PR #259 review major 6) — the daemon
        // is the interface that succeeds `--json`, so a client must be
        // able to distinguish a syntax error from a resource refusal.
        Err(e) => {
            let kind = match &e {
                QueryError::Parse(_) => "parse",
                QueryError::Bind(_) => "bind",
                QueryError::Exec(_) => "exec",
                QueryError::Persist(_) => "persist",
                // Surface the writer-lock conflict as `locked` — the same
                // kind the ref verbs use — so a client tells a retriable
                // lock conflict from a permanent graph error consistently
                // across verbs (PR #270 review M-2).
                QueryError::Graph(GraphError::Locked { .. }) => "locked",
                QueryError::Graph(_) => "graph",
                QueryError::WriteAtVersion => "write-at-version",
            };
            write_frame(
                stream,
                &json!({"id": id, "error": {
                    "kind": kind,
                    "message": e.render(cypher),
                }}),
            )
        }
    }
}

/// Serve the `status` verb: a read-only snapshot of the workspace state, as
/// one terminal `ok` frame (acetone-pz0k.4). Read-only — takes no write lock.
fn run_status(stream: &mut UnixStream, repo: &Repository, id: &Json) -> Result<()> {
    let status = (|| -> Result<Json, acetone_core::graph::GraphError> {
        let branch = repo.current_branch()?;
        let head = repo.head_commit()?.map(|h| h.to_hex());
        let dirty = repo.is_dirty()?;
        let snapshot = repo.workspace_snapshot()?;
        Ok(json!({
            "branch": branch,
            "head": head,
            "dirty": dirty,
            "nodes": snapshot.node_count()?,
            "edges": snapshot.edge_count()?,
        }))
    })();
    match status {
        Ok(ok) => write_frame(stream, &json!({"id": id, "ok": ok})),
        // A damaged/absent workspace is reported as a typed error, mirroring
        // how the `query` verb maps engine errors.
        Err(e) => write_frame(
            stream,
            &json!({"id": id, "error": {"kind": "graph", "message": e.to_string()}}),
        ),
    }
}

/// Total-size cap on a streamed schema-apply payload: schema documents are
/// small, so a generous 16 MiB is far above any real document while bounding
/// the memory a peer can make the daemon accumulate (ADR-0074 §6). `import`
/// streams to its own bound in a later unit. Overridable via
/// `ACETONE_SERVE_PAYLOAD_MAX_BYTES` so a test can exercise the cap without a
/// 16 MiB payload (the production default otherwise stands).
const SCHEMA_APPLY_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Total-size cap on a streamed `import` source. Imports can be large, so
/// this is generous (1 GiB) — the honest ceiling is the daemon's own disk,
/// and the import path streams record-by-record so daemon *memory* stays
/// bounded regardless (acetone-7qw.3). Bounds a peer's disk use.
const IMPORT_MAX_BYTES: usize = 1024 * 1024 * 1024;

/// The effective payload cap: `default` unless overridden by
/// `ACETONE_SERVE_PAYLOAD_MAX_BYTES` (so a test can trip the cap without a
/// large payload; the production default otherwise stands).
fn payload_cap(default: usize) -> usize {
    std::env::var("ACETONE_SERVE_PAYLOAD_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Read a streamed text payload into a `String` (for schema-apply, which
/// parses it in memory). Bounded by `cap`; see [`read_payload_to_writer`]
/// for the framing/violation contract.
fn read_payload(stream: &mut UnixStream, id: &Json, cap: usize) -> Result<String> {
    let mut buf: Vec<u8> = Vec::new();
    read_payload_to_writer(stream, id, cap, &mut buf)?;
    // The chunks are JSON string values, hence already valid UTF-8; the
    // conversion cannot realistically fail, but check rather than assume.
    String::from_utf8(buf).context("streamed payload was not valid UTF-8")
}

/// Read a streamed payload — `{"chunk": "<utf-8 text>"}` frames until a
/// `{"chunk_end": true}` frame (ADR-0074 §4 — the peer streams the bytes; NO
/// path ever crosses the wire) — writing each chunk to `sink` as it arrives,
/// so a large import stays O(1) in daemon memory. The cumulative size is
/// capped at `cap`. On a protocol violation (over the cap, a frame that is
/// neither `chunk` nor `chunk_end`, or EOF before `chunk_end`) it sends a
/// typed error frame and returns `Err`, which closes the connection: the
/// chunk stream is then desynced and must not be read as the next request.
fn read_payload_to_writer(
    stream: &mut UnixStream,
    id: &Json,
    cap: usize,
    sink: &mut impl Write,
) -> Result<()> {
    let mut total: usize = 0;
    loop {
        let Some(frame) = read_frame(stream)? else {
            let _ = write_frame(
                stream,
                &json!({"id": id, "error": {
                    "kind": "bad-request",
                    "message": "connection closed before chunk_end",
                }}),
            );
            bail!("payload stream closed before chunk_end");
        };
        if frame.get("chunk_end").and_then(Json::as_bool) == Some(true) {
            return Ok(());
        }
        match frame.pointer("/chunk").and_then(Json::as_str) {
            Some(chunk) => {
                total = total.saturating_add(chunk.len());
                if total > cap {
                    let _ = write_frame(
                        stream,
                        &json!({"id": id, "error": {
                            "kind": "payload-too-large",
                            "message": format!("streamed payload exceeds the {cap}-byte cap"),
                        }}),
                    );
                    bail!("streamed payload exceeded the cap");
                }
                sink.write_all(chunk.as_bytes())
                    .context("writing the streamed payload")?;
            }
            None => {
                let _ = write_frame(
                    stream,
                    &json!({"id": id, "error": {
                        "kind": "bad-request",
                        "message": "expected a chunk or chunk_end frame",
                    }}),
                );
                bail!("unexpected frame in payload stream");
            }
        }
    }
}

/// Serve the `schema-apply` verb: apply a schema document streamed as chunk
/// frames (acetone-pz0k.3). Plan lines stream as advisories; the terminal
/// `ok` reports what was applied. A malformed document is a typed
/// `schema-apply` error, leaving the connection open (the payload was fully
/// read); a payload-protocol violation closes the connection (`read_payload`).
fn run_schema_apply(
    stream: &mut UnixStream,
    repo: &Repository,
    request: &Json,
    id: &Json,
) -> Result<()> {
    let dry_run = request
        .pointer("/params/dry_run")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let text = read_payload(stream, id, payload_cap(SCHEMA_APPLY_MAX_BYTES))?;
    let mut plan = Vec::new();
    let outcome = crate::commands::schema_apply_core(repo, &text, dry_run, |line| plan.push(line));
    for line in plan {
        write_frame(stream, &json!({"id": id, "advisory": line}))?;
    }
    match outcome {
        Ok(o) => {
            let ok = match o {
                crate::commands::SchemaApplyOutcome::DryRun => {
                    json!({"dry_run": true, "applied": 0})
                }
                crate::commands::SchemaApplyOutcome::NothingToApply => json!({"applied": 0}),
                crate::commands::SchemaApplyOutcome::Applied(n) => json!({"applied": n}),
            };
            write_frame(stream, &json!({"id": id, "ok": ok}))
        }
        Err(e) => write_frame(
            stream,
            &json!({"id": id, "error": {"kind": "schema-apply", "message": e.to_string()}}),
        ),
    }
}

/// Removes its directory (recursively) on drop — so a streamed import's
/// staging directory is cleaned up however `run_import` returns (success,
/// error, or a `?` early-out).
struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fresh private (`0700`) directory for one streamed import's source file
/// (acetone-pz0k.4). Named by pid + a process-lifetime counter so concurrent
/// imports never collide; a crashed prior run's leftover at the same name is
/// cleared first, then the directory is created exclusively and locked to
/// `0700` — the same staging pattern `serve` uses for the socket, so the
/// file is unreachable to other users whatever the temp dir's own mode.
fn import_staging_dir() -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("acetone-import.{}.{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir)
        .with_context(|| format!("creating import staging dir {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .context("restricting the import staging dir to 0700")?;
    Ok(dir)
}

/// Serve the `import` verb: stream the source bytes into a daemon-private
/// staging file, then run the ordinary import path against it (acetone-pz0k.4).
/// The peer never names a path (ADR-0074 §4) — the staging file is the
/// daemon's own, removed when this returns. `params` mirror `acetone import`
/// minus the source: `format` (required), the node/edge mapping, `branch`,
/// `message`, `batch_size`.
fn run_import(stream: &mut UnixStream, repo: &Repository, request: &Json, id: &Json) -> Result<()> {
    let param = |k: &str| {
        request
            .pointer(&format!("/params/{k}"))
            .and_then(Json::as_str)
    };
    let Some(format) = param("format") else {
        return write_frame(
            stream,
            &json!({"id": id, "error": {
                "kind": "bad-request",
                "message": "import needs params.format (e.g. \"csv\" or \"ndjson\")",
            }}),
        );
    };
    let batch_size = request
        .pointer("/params/batch_size")
        .and_then(Json::as_u64)
        .map(|n| n as usize);

    // Stage the streamed source to a private temp file (removed on return).
    let dir = import_staging_dir()?;
    let _guard = DirGuard(dir.clone());
    let source = dir.join("source");
    {
        let mut file =
            std::fs::File::create(&source).context("creating the import staging file")?;
        // A protocol violation here closes the connection (`?`); the guard
        // still removes the staging dir on the way out.
        read_payload_to_writer(stream, id, payload_cap(IMPORT_MAX_BYTES), &mut file)?;
        file.flush().ok();
    }

    let outcome = crate::import::import_core(
        repo,
        format,
        &source,
        "(streamed over the acetone daemon socket)",
        param("label"),
        param("edge"),
        param("from"),
        param("to"),
        param("disc"),
        param("branch"),
        param("message"),
        batch_size,
    );
    match outcome {
        Ok(acetone_core::graph::import::ImportOutcome::NoChange) => {
            write_frame(stream, &json!({"id": id, "ok": {"imported": false}}))
        }
        Ok(acetone_core::graph::import::ImportOutcome::Committed {
            commit,
            nodes,
            edges,
        }) => write_frame(
            stream,
            &json!({"id": id, "ok": {
                "imported": true, "nodes": nodes, "edges": edges, "commit": commit.to_hex(),
            }}),
        ),
        // `{e:#}` renders anyhow's whole cause chain, not just the outer
        // `.context("importing")` — so a peer can tell a retriable lock
        // conflict ("...locked by another writer") from a permanent data
        // error, rather than the bare "importing" (PR #268 review M1).
        // Concurrent imports race the shared workspace exactly as two
        // concurrent `acetone import` processes do (a dirty-workspace
        // refusal, not corruption); a client retries.
        Err(e) => write_frame(
            stream,
            &json!({"id": id, "error": {"kind": "import", "message": format!("{e:#}")}}),
        ),
    }
}

// --- ref-advancing verbs (acetone-pz0k.5) -----------------------------------
//
// These operate on the ONE shared per-worktree workspace (Greg's decision):
// a connection is a view onto it, like concurrent CLI processes. They are
// thin wrappers over the same `Repository` methods the CLI uses.

/// A typed `bad-request` frame for a malformed verb request.
fn bad_request(stream: &mut UnixStream, id: &Json, message: &str) -> Result<()> {
    write_frame(
        stream,
        &json!({"id": id, "error": {"kind": "bad-request", "message": message}}),
    )
}

/// A typed error frame for a `GraphError`, mapping the common variants to
/// distinct kinds so a machine client can tell a retriable lock conflict from
/// a permanent one; the message carries the detail.
fn graph_error_frame(stream: &mut UnixStream, id: &Json, e: &GraphError) -> Result<()> {
    let kind = match e {
        GraphError::Locked { .. } => "locked",
        GraphError::DirtyWorkspace => "dirty-workspace",
        GraphError::NoSuchBranch { .. } => "no-such-branch",
        GraphError::BranchExists { .. } => "branch-exists",
        GraphError::NoCurrentBranch => "no-current-branch",
        GraphError::NothingToCommit => "nothing-to-commit",
        GraphError::MergeInProgress => "merge-in-progress",
        // "concurrent-modification" (a ref/workspace moved under us — reload
        // and retry), deliberately NOT "conflict": that would collide with
        // the merge verb's `outcome:"conflicts"` ok-frame, which is
        // resolve-these DATA, not a retriable error (PR #270 review M-4).
        GraphError::BranchConflict { .. } | GraphError::WorkspaceConflict { .. } => {
            "concurrent-modification"
        }
        _ => "graph",
    };
    write_frame(
        stream,
        &json!({"id": id, "error": {"kind": kind, "message": e.to_string()}}),
    )
}

/// Serve `commit`: commit the shared workspace. `params.message` (required),
/// `params.trailer` (optional `["k=v", ...]`), `params.allow_empty`.
fn run_commit(stream: &mut UnixStream, repo: &Repository, request: &Json, id: &Json) -> Result<()> {
    let Some(message) = request.pointer("/params/message").and_then(Json::as_str) else {
        return bad_request(stream, id, "commit needs params.message (a string)");
    };
    let allow_empty = request
        .pointer("/params/allow_empty")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let mut trailers: Vec<(String, String)> = Vec::new();
    match request.pointer("/params/trailer") {
        None => {}
        // A present-but-not-an-array trailer is a client mistake, not
        // silently dropped metadata (PR #270 review M-3).
        Some(Json::Array(arr)) => {
            for entry in arr {
                let Some(raw) = entry.as_str() else {
                    return bad_request(
                        stream,
                        id,
                        "trailer entries must be \"key=value\" strings",
                    );
                };
                match crate::value::parse_kv(raw, "trailer") {
                    Ok((k, v)) => trailers.push((k.to_owned(), v.to_owned())),
                    Err(e) => return bad_request(stream, id, &e.to_string()),
                }
            }
        }
        Some(_) => {
            return bad_request(
                stream,
                id,
                "params.trailer must be an array of \"key=value\" strings",
            );
        }
    }
    // Take the lock through the stale-lock-recovery wrapper (ADR-0074 §8), so
    // a dead writer's lock doesn't wedge `commit` (PR #270 review MAJOR-1).
    let committed = with_lock_recovery(repo, is_graph_locked, || {
        let txn = repo.begin_write()?;
        if allow_empty {
            txn.commit_allow_empty(message, &trailers, None)
        } else {
            txn.commit(message, &trailers, None)
        }
    });
    match committed {
        Ok(commit) => write_frame(
            stream,
            &json!({"id": id, "ok": {"commit": commit.to_hex()}}),
        ),
        Err(e) => graph_error_frame(stream, id, &e),
    }
}

/// Serve `branch`: list (no params), create (`params.name`, optional
/// `params.refspec`), or delete (`params.delete`).
fn run_branch(stream: &mut UnixStream, repo: &Repository, request: &Json, id: &Json) -> Result<()> {
    let p = |k: &str| {
        request
            .pointer(&format!("/params/{k}"))
            .and_then(Json::as_str)
    };
    if let Some(name) = p("delete") {
        return match repo.delete_branch(name) {
            Ok(was) => write_frame(
                stream,
                &json!({"id": id, "ok": {"deleted": name, "was": was.to_hex()}}),
            ),
            Err(e) => graph_error_frame(stream, id, &e),
        };
    }
    if let Some(name) = p("name") {
        return match repo.create_branch(name, p("refspec")) {
            Ok(at) => write_frame(
                stream,
                &json!({"id": id, "ok": {"created": name, "at": at.to_hex()}}),
            ),
            Err(e) => graph_error_frame(stream, id, &e),
        };
    }
    match repo.branches() {
        Ok(branches) => {
            let list: Vec<Json> = branches
                .into_iter()
                .map(|(name, head)| json!({"name": name, "head": head.to_hex()}))
                .collect();
            write_frame(stream, &json!({"id": id, "ok": {"branches": list}}))
        }
        Err(e) => graph_error_frame(stream, id, &e),
    }
}

/// Serve `checkout`: switch the shared current-branch pointer. `params.branch`.
fn run_checkout(
    stream: &mut UnixStream,
    repo: &Repository,
    request: &Json,
    id: &Json,
) -> Result<()> {
    let Some(branch) = request.pointer("/params/branch").and_then(Json::as_str) else {
        return bad_request(stream, id, "checkout needs params.branch (a string)");
    };
    match with_lock_recovery(repo, is_graph_locked, || repo.checkout_branch(branch)) {
        Ok(()) => write_frame(stream, &json!({"id": id, "ok": {"checked_out": branch}})),
        Err(e) => graph_error_frame(stream, id, &e),
    }
}

/// Serve `merge`: merge `params.refspec` into the current branch (optional
/// `params.message`), or abort a merge in progress (`params.abort`).
/// Conflicts come back as DATA in the terminal frame, mid-merge on the shared
/// workspace — a client resolves via `resolve`/write queries then `commit`.
fn run_merge(stream: &mut UnixStream, repo: &Repository, request: &Json, id: &Json) -> Result<()> {
    if request
        .pointer("/params/abort")
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        return match with_lock_recovery(repo, is_graph_locked, || repo.abort_merge()) {
            Ok(()) => write_frame(stream, &json!({"id": id, "ok": {"aborted": true}})),
            Err(e) => graph_error_frame(stream, id, &e),
        };
    }
    let Some(refspec) = request.pointer("/params/refspec").and_then(Json::as_str) else {
        return bad_request(stream, id, "merge needs params.refspec (or params.abort)");
    };
    let message = request
        .pointer("/params/message")
        .and_then(Json::as_str)
        .unwrap_or("merge");
    match with_lock_recovery(repo, is_graph_locked, || repo.merge(refspec, message)) {
        Ok(acetone_core::graph::merge::MergeOutcome::AlreadyUpToDate) => {
            write_frame(stream, &json!({"id": id, "ok": {"outcome": "up-to-date"}}))
        }
        Ok(acetone_core::graph::merge::MergeOutcome::FastForward(head)) => write_frame(
            stream,
            &json!({"id": id, "ok": {"outcome": "fast-forward", "head": head.to_hex()}}),
        ),
        Ok(acetone_core::graph::merge::MergeOutcome::Merged(commit)) => write_frame(
            stream,
            &json!({"id": id, "ok": {"outcome": "merged", "commit": commit.to_hex()}}),
        ),
        Ok(acetone_core::graph::merge::MergeOutcome::Conflicts(conflicts)) => {
            let rendered: Vec<String> = conflicts
                .iter()
                .map(crate::commands::render_conflict)
                .collect();
            write_frame(
                stream,
                &json!({"id": id, "ok": {
                    "outcome": "conflicts",
                    "count": rendered.len(),
                    "conflicts": rendered,
                }}),
            )
        }
        Err(e) => graph_error_frame(stream, id, &e),
    }
}

/// Serve `resolve`: resolve every remaining merge conflict to one side —
/// `params.all_ours` or `params.all_theirs`.
fn run_resolve(
    stream: &mut UnixStream,
    repo: &Repository,
    request: &Json,
    id: &Json,
) -> Result<()> {
    let ours = request
        .pointer("/params/all_ours")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let theirs = request
        .pointer("/params/all_theirs")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let side = match (ours, theirs) {
        (true, false) => acetone_core::graph::repo::ResolveSide::Ours,
        (false, true) => acetone_core::graph::repo::ResolveSide::Theirs,
        _ => {
            return bad_request(
                stream,
                id,
                "resolve needs exactly one of params.all_ours or params.all_theirs",
            );
        }
    };
    match with_lock_recovery(repo, is_graph_locked, || repo.resolve_all(side)) {
        Ok(n) => write_frame(stream, &json!({"id": id, "ok": {"resolved": n}})),
        Err(e) => graph_error_frame(stream, id, &e),
    }
}

// --- framing (ADR-0074 §2) --------------------------------------------------

fn write_frame(stream: &mut UnixStream, value: &Json) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME_BYTES);
    let Some(len) = len else {
        bail!("refusing to send a frame over the {MAX_FRAME_BYTES}-byte cap");
    };
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

/// `Ok(None)` is a clean EOF between frames.
fn read_frame(stream: &mut UnixStream) -> Result<Option<Json>> {
    let mut len_bytes = [0u8; 4];
    match stream.read_exact(&mut len_bytes) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_FRAME_BYTES {
        // Refused BEFORE allocating or parsing (ADR-0074 §2): a 4-byte
        // header cannot commit up to 16 MiB (review major 4).
        bail!("peer sent a frame of {len} bytes, over the {MAX_FRAME_BYTES}-byte cap");
    }
    let mut buf = vec![0u8; len as usize];
    // A read timeout fires as WouldBlock/TimedOut: a truncated or
    // slow-loris body ends the connection rather than parking the thread
    // (review majors 2/4).
    stream.read_exact(&mut buf)?;
    Ok(Some(
        serde_json::from_slice(&buf).context("parsing frame JSON")?,
    ))
}

// --- a minimal counting semaphore over std ----------------------------------
//
// Poison-robust: the guarded state is one integer, so a panic while it is
// held cannot leave it inconsistent; recover the guard rather than
// `.expect()`-ing, which under an unwinding `Drop` would double-panic to an
// abort (PR #259 review minor).

fn lock_count(m: &std::sync::Mutex<usize>) -> std::sync::MutexGuard<'_, usize> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Semaphore {
    count: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Semaphore {
            count: std::sync::Mutex::new(permits.max(1)),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Block for a permit borrowed for the caller's scope (the query
    /// semaphore, used within one connection).
    fn acquire(&self) -> Permit<'_> {
        let mut count = lock_count(&self.count);
        while *count == 0 {
            count = self.cv.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count -= 1;
        Permit(self)
    }

    /// Non-blocking, `'static`-owned permit (the connection semaphore,
    /// moved into a handler thread). `None` at the cap.
    fn try_acquire(self: &Arc<Self>) -> Option<OwnedPermit> {
        let mut count = lock_count(&self.count);
        if *count == 0 {
            return None;
        }
        *count -= 1;
        Some(OwnedPermit(Arc::clone(self)))
    }

    fn release(&self) {
        *lock_count(&self.count) += 1;
        self.cv.notify_one();
    }
}

struct Permit<'s>(&'s Semaphore);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct OwnedPermit(Arc<Semaphore>);
impl Drop for OwnedPermit {
    fn drop(&mut self) {
        self.0.release();
    }
}
