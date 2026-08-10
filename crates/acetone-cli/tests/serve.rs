//! `acetone serve` integration: a raw-socket client (no acetone client
//! library — deliberately, per the phase criterion's non-Rust-client
//! spirit) drives the daemon over a live socket — hello, read and write
//! `query` (with autodeclare coinage), and the `status` verb — plus the
//! shipped Python example as a genuine non-Rust client.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

fn acetone(repo: &Path, args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn write_frame(s: &mut UnixStream, v: &serde_json::Value) {
    let b = serde_json::to_vec(v).expect("encode");
    s.write_all(&(b.len() as u32).to_be_bytes()).expect("len");
    s.write_all(&b).expect("body");
}

fn read_frame(s: &mut UnixStream) -> serde_json::Value {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).expect("len");
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    s.read_exact(&mut buf).expect("body");
    serde_json::from_slice(&buf).expect("json")
}

struct Daemon {
    child: Child,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon(repo: &Path, socket: &Path) -> Daemon {
    start_daemon_args(repo, socket, &[])
}

fn start_daemon_args(repo: &Path, socket: &Path, extra: &[&str]) -> Daemon {
    start_daemon_env(repo, socket, extra, &[])
}

fn start_daemon_env(repo: &Path, socket: &Path, extra: &[&str], env: &[(&str, &str)]) -> Daemon {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut args = vec![
        "--repo",
        repo.to_str().unwrap(),
        "serve",
        "--socket",
        socket.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let mut cmd = Command::new(bin);
    cmd.args(&args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    // Wait for the readiness line (ADR-0074 §7).
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let ready = lines.next().expect("readiness line").expect("read");
    let ready: serde_json::Value = serde_json::from_str(&ready).expect("readiness json");
    assert_eq!(ready["ready"], true, "{ready}");
    assert_eq!(ready["protocol"], 1);
    Daemon { child }
}

fn seeded_repo(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = dir.path().join("repo");
    let bin = env!("CARGO_BIN_EXE_acetone");
    assert!(
        Command::new(bin)
            .args(["init", repo.to_str().unwrap()])
            .output()
            .expect("init")
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["declare-label", "Doc", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "Doc", "d1", "--prop", "title=\"one\""])
            .status
            .success()
    );
    assert!(acetone(&repo, &["put-node", "Doc", "d2"]).status.success());
    repo
}

fn hello(socket: &Path) -> UnixStream {
    let mut s = UnixStream::connect(socket).expect("connect");
    let server_hello = read_frame(&mut s);
    assert_eq!(server_hello["acetone"]["protocol"], 1, "{server_hello}");
    write_frame(&mut s, &serde_json::json!({"acetone": {"protocol": 1}}));
    s
}

#[test]
fn hello_then_streamed_read_query() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    // The socket is 0600 (the kernel ACL IS the auth — ADR-0074 §1).
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&socket)
        .expect("socket meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket must be 0600");

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query", "params": {
            "cypher": "MATCH (d:Doc) RETURN d.id ORDER BY d.id"
        }}),
    );
    let mut rows = Vec::new();
    loop {
        let frame = read_frame(&mut s);
        assert_eq!(frame["id"], 1);
        if frame.get("row").is_some() {
            rows.push(frame["row"]["values"][0].clone());
        } else {
            assert_eq!(frame["ok"]["rows"], 2, "terminal frame: {frame}");
            break;
        }
    }
    assert_eq!(rows, vec![serde_json::json!("d1"), serde_json::json!("d2")]);
}

#[test]
fn concurrent_connections_and_unknown_verb_and_over_cap_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    // A write is served (no longer refused) — a second connection then
    // sees it, proving the workspace advanced.
    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 7, "verb": "query", "params": {
            "cypher": "CREATE (:Doc {id: 'w7'})"
        }}),
    );
    let frame = read_frame(&mut s);
    assert_eq!(frame["ok"]["write"]["nodes_created"], 1, "{frame}");

    // A second connection works while the first stays open.
    let mut s2 = hello(&socket);
    write_frame(
        &mut s2,
        &serde_json::json!({"id": 8, "verb": "query", "params": {
            "cypher": "MATCH (d:Doc) RETURN count(d)"
        }}),
    );
    let row = read_frame(&mut s2);
    // 2 seeded + the w7 node just created above.
    assert_eq!(row["row"]["values"][0], 3, "{row}");
    let done = read_frame(&mut s2);
    assert_eq!(done["ok"]["rows"], 1);

    // Unknown verbs refuse typed; the connection survives.
    write_frame(&mut s, &serde_json::json!({"id": 9, "verb": "frobnicate"}));
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "unknown-verb", "{frame}");

    // An over-cap frame is refused at the framing layer: the daemon drops
    // the connection rather than allocating.
    let mut s3 = UnixStream::connect(&socket).expect("connect");
    let _ = read_frame(&mut s3);
    s3.write_all(&(20 * 1024 * 1024u32).to_be_bytes())
        .expect("len");
    let mut probe = [0u8; 1];
    // The daemon closes on us; reading eventually errors or EOFs.
    let closed = matches!(s3.read(&mut probe), Ok(0) | Err(_));
    assert!(closed, "over-cap frame must close the connection");

    // A second daemon on a LIVE socket refuses (connects, finds a live
    // daemon there — distinct from the stale-reclaim path).
    let out = acetone(&repo, &["serve", "--socket", socket.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already served by a live daemon"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn typed_errors_carry_kind_and_span() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);

    // A syntax error is kind "parse" with a rendered span, distinct from
    // other failures (PR #259 review major 6).
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query", "params": {"cypher": "MATCH ("}}),
    );
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "parse", "{frame}");
    let msg = frame["error"]["message"].as_str().expect("message");
    assert!(msg.contains("line 1"), "span rendered: {msg}");

    // An unknown label binds as an error of a different kind — the client
    // can tell them apart.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 2, "verb": "query",
            "params": {"cypher": "MATCH (n:Nope) RETURN n"}}),
    );
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "bind", "{frame}");
}

#[test]
fn an_over_cap_result_frame_terminates_typed_not_with_a_bare_eof() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);
    // A single row far over the 16 MiB frame cap.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query",
            "params": {"cypher": "RETURN range(0, 5000000) AS r"}}),
    );
    let frame = read_frame(&mut s);
    assert_eq!(
        frame["error"]["kind"], "result-too-large",
        "must terminate typed, not EOF: {frame}"
    );
}

#[test]
fn a_stale_socket_from_a_dead_daemon_is_reclaimed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    {
        let _daemon = start_daemon(&repo, &socket);
        // daemon killed on drop here, leaving the socket file behind
    }
    assert!(socket.exists(), "the killed daemon left its socket");
    // A fresh daemon reclaims it rather than refusing.
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query", "params": {"cypher": "RETURN 1 AS n"}}),
    );
    let row = read_frame(&mut s);
    assert_eq!(row["row"]["values"][0], 1, "{row}");
}

#[test]
fn a_truncated_frame_does_not_wedge_the_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    // One connection declares a body it never sends: the read timeout
    // ends it rather than parking a thread forever. We don't wait the
    // full timeout here — we assert a SECOND connection still works
    // immediately, i.e. the stall didn't take the daemon down.
    let mut stalled = UnixStream::connect(&socket).expect("connect");
    let _ = read_frame(&mut stalled); // server hello
    stalled.write_all(&100u32.to_be_bytes()).expect("len only");
    // never send the 100 bytes

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query", "params": {"cypher": "RETURN 1 AS n"}}),
    );
    let row = read_frame(&mut s);
    assert_eq!(row["row"]["values"][0], 1, "daemon stays responsive: {row}");
}

/// A slow-READER peer (sends a valid query, never reads the large result)
/// must not starve query permits: the write timeout ends its stalled
/// send so a second client still executes (PR #259 re-review M2).
#[test]
fn a_slow_reader_does_not_starve_the_query_permits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    // One query permit makes the starvation deterministic: if the stalled
    // reader held it indefinitely, the second client could never run.
    // A 2s IO timeout (not the 30s production default) so the release is
    // provable fast; one query permit makes the starvation deterministic.
    let _daemon = start_daemon_env(
        &repo,
        &socket,
        &["--max-concurrent", "1"],
        &[("ACETONE_SERVE_IO_TIMEOUT_SECS", "2")],
    );

    // A big result the peer never reads — its send blocks in write_all
    // under the query permit, until the write timeout fires.
    let mut stalled = hello(&socket);
    write_frame(
        &mut stalled,
        &serde_json::json!({"id": 1, "verb": "query",
            "params": {"cypher": "UNWIND range(0, 90000) AS i RETURN i, 'xxxxxxxxxxxxxxxxxxxx' AS p"}}),
    );
    // Do NOT read from `stalled`.

    // A second client must complete well within the 30s write timeout
    // window — proving the permit is not held forever.
    let start = std::time::Instant::now();
    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 2, "verb": "query", "params": {"cypher": "RETURN 1 AS n"}}),
    );
    let row = read_frame(&mut s);
    assert_eq!(row["row"]["values"][0], 1, "second client served: {row}");
    // The stalled reader's write blocks under the one permit until the 2s
    // timeout releases it; the second client then runs. Well under 10s
    // proves the permit is not held forever (pre-M2-fix it was).
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "the permit must release on the write timeout, not be held forever: {:?}",
        start.elapsed()
    );
}

#[test]
fn a_write_query_advances_the_workspace_and_reports_its_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    // A write over the socket: create a node, RETURN nothing.
    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query",
            "params": {"cypher": "CREATE (:Doc {id: 'new1'})"}}),
    );
    let ok = read_frame(&mut s);
    assert_eq!(ok["ok"]["write"]["nodes_created"], 1, "write summary: {ok}");

    // A fresh connection sees the committed workspace advance (MVCC +
    // the write path advanced the workspace ref).
    let mut s2 = hello(&socket);
    write_frame(
        &mut s2,
        &serde_json::json!({"id": 2, "verb": "query",
            "params": {"cypher": "MATCH (d:Doc {id: 'new1'}) RETURN d.id"}}),
    );
    let row = read_frame(&mut s2);
    assert_eq!(
        row["row"]["values"][0], "new1",
        "the write is visible: {row}"
    );
    let done = read_frame(&mut s2);
    assert_eq!(done["ok"]["rows"], 1);
}

#[test]
fn a_write_that_returns_rows_streams_them_then_the_write_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query",
            "params": {"cypher": "CREATE (d:Doc {id: 'r1'}) RETURN d.id"}}),
    );
    let row = read_frame(&mut s);
    assert_eq!(row["row"]["values"][0], "r1", "the RETURN row: {row}");
    let ok = read_frame(&mut s);
    assert_eq!(
        ok["ok"]["write"]["nodes_created"], 1,
        "then the summary: {ok}"
    );
}

/// Concurrent writes do not deadlock and do not both win (PR #261 review):
/// the query permit is always acquired before the fail-fast writer lock,
/// so no hold-and-wait cycle forms — one write commits, the other returns
/// a typed `graph` (locked) error, both terminate.
#[test]
fn concurrent_writes_do_not_deadlock() {
    use std::sync::mpsc;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    // Two permits so both handlers run at once and genuinely contend the
    // lock (with one permit they'd serialise at the permit instead).
    let _daemon = start_daemon_args(&repo, &socket, &["--max-concurrent", "2"]);

    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for i in 0..2 {
        let socket = socket.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let mut s = hello(&socket);
            write_frame(
                &mut s,
                &serde_json::json!({"id": i, "verb": "query",
                    "params": {"cypher": format!("CREATE (:Doc {{id: 'c{i}'}})")}}),
            );
            tx.send(read_frame(&mut s)).expect("send");
        }));
    }
    drop(tx);
    let frames: Vec<serde_json::Value> = rx.iter().collect();
    for h in handles {
        h.join().expect("join");
    }
    // Both terminated (no hang). Exactly the losers, if any, are typed
    // `graph` (locked) errors; at least one succeeded.
    assert_eq!(frames.len(), 2, "both writes must terminate");
    let wins = frames.iter().filter(|f| f.get("ok").is_some()).count();
    let locked = frames
        .iter()
        .filter(|f| f["error"]["kind"] == "graph")
        .count();
    assert!(wins >= 1, "at least one write commits: {frames:?}");
    assert_eq!(
        wins + locked,
        2,
        "losers are typed graph errors: {frames:?}"
    );
}

/// A stale writer lock (a SIGKILLed writer's leftover) is a typed `graph`
/// error with a manual-recovery hint, not a hang — and the daemon keeps
/// serving reads (PR #261 review: this became load-bearing when writes
/// shipped without the ADR-0074 §8 recovery).
#[test]
fn a_stale_writer_lock_is_a_typed_error_not_a_hang() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    // Plant a stale lock in the repo's git dir, as a SIGKILLed writer
    // would leave. The repo dir is the git dir for a bare init.
    let lock = repo.join("acetone-writer.lock");
    std::fs::write(&lock, "pid=999999 unix-time=1\n").expect("plant lock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query",
            "params": {"cypher": "CREATE (:Doc {id: 'x'})"}}),
    );
    let frame = read_frame(&mut s);
    assert_eq!(
        frame["error"]["kind"], "graph",
        "typed locked error: {frame}"
    );

    // The daemon survives — a read on the same connection still works.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 2, "verb": "query", "params": {"cypher": "RETURN 1 AS n"}}),
    );
    let row = read_frame(&mut s);
    assert_eq!(
        row["row"]["values"][0], 1,
        "daemon still serves reads: {row}"
    );
}

/// acetone-pz0k / Phase 11 gate criterion 1: a NON-Rust client drives a full
/// session (hello → write → read) against a live `acetone serve`, using only
/// the documented JSON frame protocol and no acetone library. The worked
/// example under `examples/acetone_daemon_client.py` IS that client; this
/// test runs it. Skipped where python3 is unavailable.
#[test]
fn a_non_rust_python_client_drives_a_full_session() {
    let has_python = Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_python {
        eprintln!("skipping a_non_rust_python_client_drives_a_full_session: python3 not found");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let bin = env!("CARGO_BIN_EXE_acetone");
    assert!(
        Command::new(bin)
            .args(["init", repo.to_str().unwrap()])
            .output()
            .expect("init")
            .status
            .success()
    );
    // The example imports/writes `Demo {id}` nodes; import needs a clean
    // workspace, so declare the label and commit before serving.
    assert!(
        acetone(&repo, &["declare-label", "Demo", "--key", "id"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "setup"]).status.success());
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/acetone_daemon_client.py"
    );
    let out = Command::new("python3")
        .arg(script)
        .arg(&socket)
        .output()
        .expect("run python client");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the non-Rust client failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    // It wrote a node, read it back, coined a relationship type, and read
    // the workspace status — a query/coin/status session from outside Rust.
    assert!(
        stdout.contains("nodes_created") && stdout.contains("from-python"),
        "the client must write then read on the live workspace: {stdout}"
    );
    assert!(
        stdout.contains("coined+wrote") && stdout.contains("relationships_created"),
        "the client must coin a relationship type: {stdout}"
    );
    assert!(
        stdout.contains("imported") && stdout.contains("'nodes': 2"),
        "the client must stream an import: {stdout}"
    );
    assert!(
        stdout.contains("status:"),
        "the client must read status: {stdout}"
    );
}

#[test]
fn the_status_verb_reports_workspace_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir); // 2 Doc nodes committed
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    write_frame(&mut s, &serde_json::json!({"id": 1, "verb": "status"}));
    let ok = read_frame(&mut s);
    assert_eq!(ok["ok"]["nodes"], 2, "status reports node count: {ok}");
    assert_eq!(ok["ok"]["edges"], 0, "{ok}");
    assert!(ok["ok"]["branch"].is_string(), "branch present: {ok}");
    // seeded_repo stages the nodes without committing, so the workspace is
    // dirty and there is no head yet — status reports that faithfully.
    assert_eq!(
        ok["ok"]["dirty"], true,
        "uncommitted workspace is dirty: {ok}"
    );
    assert!(ok["ok"]["head"].is_null(), "no commit yet: {ok}");

    // After a write over the socket, a fresh status reflects the new count.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 2, "verb": "query", "params": {
            "cypher": "CREATE (:Doc {id: 'd3'})"
        }}),
    );
    let _ = read_frame(&mut s); // the write's terminal ok
    write_frame(&mut s, &serde_json::json!({"id": 3, "verb": "status"}));
    let ok = read_frame(&mut s);
    assert_eq!(ok["ok"]["nodes"], 3, "status sees the socket write: {ok}");
}

#[test]
fn a_write_coins_a_rel_type_only_with_autodeclare() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    // Without autodeclare, a CREATE naming an undeclared rel-type is refused.
    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "query", "params": {
            "cypher": "MATCH (a:Doc {id:'d1'}),(b:Doc {id:'d2'}) CREATE (a)-[:`relates to`]->(b)"
        }}),
    );
    let refused = read_frame(&mut s);
    assert!(
        refused.get("error").is_some(),
        "undeclared rel-type must refuse: {refused}"
    );

    // With autodeclare, the same write coins the type and succeeds.
    let mut s2 = hello(&socket);
    write_frame(
        &mut s2,
        &serde_json::json!({"id": 2, "verb": "query", "params": {
            "cypher": "MATCH (a:Doc {id:'d1'}),(b:Doc {id:'d2'}) CREATE (a)-[:`relates to`]->(b)",
            "autodeclare": true
        }}),
    );
    // Drain rows/advisories to the terminal frame.
    let mut terminal = read_frame(&mut s2);
    while terminal.get("ok").is_none() && terminal.get("error").is_none() {
        terminal = read_frame(&mut s2);
    }
    assert_eq!(
        terminal["ok"]["write"]["relationships_created"], 1,
        "autodeclare coins and creates the edge: {terminal}"
    );
}

#[test]
fn the_schema_apply_verb_applies_a_streamed_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    // Request, then stream the document as two chunk frames + chunk_end —
    // no path crosses the wire.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "schema-apply", "params": {}}),
    );
    let doc = r#"{"labels": [{"name": "Widget", "key": ["id"]}]}"#;
    let (a, b) = doc.split_at(20);
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": a}));
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": b}));
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk_end": true}));

    // Drain plan advisories to the terminal ok.
    let mut frame = read_frame(&mut s);
    while frame.get("ok").is_none() && frame.get("error").is_none() {
        frame = read_frame(&mut s);
    }
    assert_eq!(frame["ok"]["applied"], 1, "schema applied: {frame}");

    // The label + key are now declared: a Widget create succeeds on the
    // same live workspace.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 2, "verb": "query", "params": {
            "cypher": "CREATE (:Widget {id: 'x'})"
        }}),
    );
    let ok = read_frame(&mut s);
    assert_eq!(ok["ok"]["write"]["nodes_created"], 1, "declared: {ok}");
}

#[test]
fn schema_apply_dry_run_streams_the_plan_without_applying() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "schema-apply", "params": {"dry_run": true}}),
    );
    let doc = r#"{"labels": [{"name": "Widget", "key": ["id"]}]}"#;
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": doc}));
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk_end": true}));

    let mut saw_plan = false;
    let mut frame = read_frame(&mut s);
    while frame.get("ok").is_none() && frame.get("error").is_none() {
        if frame.get("advisory").is_some() {
            saw_plan = true;
        }
        frame = read_frame(&mut s);
    }
    assert!(saw_plan, "dry run must stream the plan as advisories");
    assert_eq!(frame["ok"]["dry_run"], true, "{frame}");
    assert_eq!(
        frame["ok"]["applied"], 0,
        "dry run applies nothing: {frame}"
    );
}

#[test]
fn an_over_cap_schema_apply_payload_is_refused_and_closes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    // A tiny payload cap so a small doc trips it.
    let _daemon = start_daemon_env(
        &repo,
        &socket,
        &[],
        &[("ACETONE_SERVE_PAYLOAD_MAX_BYTES", "8")],
    );

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "schema-apply", "params": {}}),
    );
    // A chunk larger than 8 bytes.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "chunk": "0123456789abcdef"}),
    );
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "payload-too-large", "{frame}");
    // The connection is closed after the typed refusal (stream desynced).
    use std::io::Read;
    let mut probe = [0u8; 1];
    let closed = matches!(s.read(&mut probe), Ok(0) | Err(_));
    assert!(closed, "over-cap payload must close the connection");
}

#[test]
fn schema_apply_rejects_a_malformed_chunk_frame_and_closes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "schema-apply", "params": {}}),
    );
    // A chunk frame whose `chunk` is a number, not a string — a protocol
    // violation, not a panic.
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": 123}));
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "bad-request", "{frame}");
    assert_eq!(
        frame["id"], 1,
        "the error carries the request id, not a smuggled one"
    );
    use std::io::Read;
    let mut probe = [0u8; 1];
    assert!(
        matches!(s.read(&mut probe), Ok(0) | Err(_)),
        "a malformed chunk stream closes the connection"
    );
}

#[test]
fn a_bad_schema_document_is_typed_and_the_connection_survives() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    // A well-formed chunk stream (so the payload is synced) carrying a
    // document with an unknown field — a LOGIC error, not a protocol one.
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "schema-apply", "params": {}}),
    );
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "chunk": r#"{"labels": [{"name": "X", "key": ["id"], "bogus": 1}]}"#}),
    );
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk_end": true}));
    let frame = read_frame(&mut s);
    assert_eq!(
        frame["error"]["kind"], "schema-apply",
        "typed doc error: {frame}"
    );

    // The connection survives (payload was fully read, stream synced): a
    // follow-up status works.
    write_frame(&mut s, &serde_json::json!({"id": 2, "verb": "status"}));
    let ok = read_frame(&mut s);
    assert!(
        ok["ok"]["nodes"].is_number(),
        "connection survives a doc error: {ok}"
    );
}

#[test]
fn the_import_verb_streams_a_source_and_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let bin = env!("CARGO_BIN_EXE_acetone");
    assert!(
        Command::new(bin)
            .args(["init", repo.to_str().unwrap()])
            .output()
            .expect("init")
            .status
            .success()
    );
    // A declared + committed label, so the workspace is clean before import.
    assert!(
        acetone(&repo, &["declare-label", "Host", "--key", "name"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "schema"]).status.success());
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "import", "params": {"format": "ndjson", "label": "Host"}}),
    );
    // Stream the source as chunk frames — no path crosses the wire.
    let src = "{\"name\":\"web1\"}\n{\"name\":\"db1\"}\n";
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": src}));
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk_end": true}));

    let mut frame = read_frame(&mut s);
    while frame.get("ok").is_none() && frame.get("error").is_none() {
        frame = read_frame(&mut s);
    }
    assert_eq!(frame["ok"]["imported"], true, "imported: {frame}");
    assert_eq!(frame["ok"]["nodes"], 2, "two nodes: {frame}");

    // A fresh connection sees the committed import (the workspace advanced).
    let mut s2 = hello(&socket);
    write_frame(
        &mut s2,
        &serde_json::json!({"id": 2, "verb": "query", "params": {
            "cypher": "MATCH (h:Host) RETURN count(h)"
        }}),
    );
    let row = read_frame(&mut s2);
    assert_eq!(
        row["row"]["values"][0], 2,
        "import is committed and visible: {row}"
    );
}

#[test]
fn an_over_cap_import_payload_is_refused_and_closes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon_env(
        &repo,
        &socket,
        &[],
        &[("ACETONE_SERVE_PAYLOAD_MAX_BYTES", "8")],
    );

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "import", "params": {"format": "ndjson", "label": "Doc"}}),
    );
    // A source larger than the 8-byte cap, streamed across two chunks to
    // prove the cap is cumulative, not per-frame.
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": "12345"}));
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk": "67890"}));
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "payload-too-large", "{frame}");
    use std::io::Read;
    let mut probe = [0u8; 1];
    assert!(
        matches!(s.read(&mut probe), Ok(0) | Err(_)),
        "over-cap import payload must close the connection"
    );
}

#[test]
fn an_import_error_surfaces_its_cause_not_a_bare_importing() {
    let dir = tempfile::tempdir().expect("tempdir");
    // seeded_repo leaves the workspace dirty (uncommitted nodes), so an
    // import refuses — and the frame must name WHY (PR #268 review M1),
    // not the bare outer "importing" context.
    let repo = seeded_repo(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);

    let mut s = hello(&socket);
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "verb": "import", "params": {"format": "ndjson", "label": "Doc"}}),
    );
    write_frame(
        &mut s,
        &serde_json::json!({"id": 1, "chunk": "{\"id\":\"x\"}\n"}),
    );
    write_frame(&mut s, &serde_json::json!({"id": 1, "chunk_end": true}));
    let frame = read_frame(&mut s);
    assert_eq!(frame["error"]["kind"], "import", "{frame}");
    let msg = frame["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("uncommitted"),
        "the error must name its cause, not a bare 'importing': {msg:?}"
    );
    // The connection survives a logic error (payload was fully read).
    write_frame(&mut s, &serde_json::json!({"id": 2, "verb": "status"}));
    assert!(
        read_frame(&mut s)["ok"]["nodes"].is_number(),
        "connection survives"
    );
}
