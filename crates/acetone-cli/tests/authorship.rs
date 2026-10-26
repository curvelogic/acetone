//! Caller-supplied authorship end to end (acetone-gid, ADR-0079): the CLI's
//! `--author` on `commit`, `import`, `merge` and `tag`, its
//! `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` fallback, and the daemon's
//! `params.author` on `commit`, `import` and `merge`. Every surface is read
//! back twice — through `acetone log --json` and through git itself — and
//! refuses a header-injection attempt with a typed error. The daemon never
//! reads an ambient identity: a commit without `params.author` is neutral
//! even when the daemon's own environment carries `GIT_AUTHOR_*`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use serde_json::{Value as Json, json};

const NEUTRAL_NAME: &str = "acetone";
const NEUTRAL_EMAIL: &str = "acetone@acetone.invalid";
const INJECTED: &str = "Eve <eve@example.com>\ncommitter Mallory <m@example.com>";

/// Run the CLI with no ambient `GIT_AUTHOR_*` unless `env` sets it.
fn acetone_env(repo: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acetone"));
    cmd.args(&full)
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run acetone")
}

fn acetone(repo: &Path, args: &[&str]) -> Output {
    acetone_env(repo, args, &[])
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "acetone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).expect("utf-8")
}

fn refused(out: &Output) -> String {
    assert!(!out.status.success(), "expected a refusal");
    String::from_utf8(out.stderr.clone()).expect("utf-8")
}

/// A repository with a committed `Doc` label and one committed node.
fn seeded(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = dir.path().join("repo");
    let out = Command::new(env!("CARGO_BIN_EXE_acetone"))
        .args(["init", repo.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(out.status.success());
    ok(&acetone(&repo, &["declare-label", "Doc", "--key", "id"]));
    ok(&acetone(&repo, &["put-node", "Doc", "d1"]));
    ok(&acetone(&repo, &["commit", "-m", "base"]));
    repo
}

/// The newest commit's `{author, committer}` per `acetone log --json`.
fn log_head(repo: &Path) -> Json {
    let log: Json = serde_json::from_str(&ok(&acetone(repo, &["log", "--json"]))).expect("json");
    log[0].clone()
}

/// `git log -1 --format=...` on `rev`: git's own reading of the object.
fn git_format(repo: &Path, rev: &str, format: &str) -> String {
    let out = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "log",
            "-1",
            &format!("--format={format}"),
            rev,
        ])
        .output()
        .expect("git log");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

fn head_hash(repo: &Path) -> String {
    log_head(repo)["hash"].as_str().expect("hash").to_owned()
}

/// Author and committer of `hash`, both via `log --json` (when it is the
/// head) and via git, asserting the two readings agree.
fn assert_identity(repo: &Path, name: &str, email: &str) {
    let head = log_head(repo);
    assert_eq!(
        head["author"],
        json!({"name": name, "email": email}),
        "{head}"
    );
    assert_eq!(
        head["committer"],
        json!({"name": NEUTRAL_NAME, "email": NEUTRAL_EMAIL}),
        "the committer stays acetone's own: {head}"
    );
    let hash = head["hash"].as_str().unwrap();
    assert_eq!(
        git_format(repo, hash, "%an <%ae>"),
        format!("{name} <{email}>")
    );
    assert_eq!(
        git_format(repo, hash, "%cn <%ce>"),
        format!("{NEUTRAL_NAME} <{NEUTRAL_EMAIL}>")
    );
}

// --- CLI ---------------------------------------------------------------------

#[test]
fn cli_commit_author_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    // The seed commit had no author and no environment: neutral.
    assert_identity(&repo, NEUTRAL_NAME, NEUTRAL_EMAIL);
    ok(&acetone(&repo, &["put-node", "Doc", "d2"]));
    ok(&acetone(
        &repo,
        &[
            "commit",
            "-m",
            "two",
            "--author",
            "Ada Lovelace <ada@example.com>",
        ],
    ));
    assert_identity(&repo, "Ada Lovelace", "ada@example.com");
}

#[test]
fn cli_falls_back_to_git_author_env_and_the_flag_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let env = [
        ("GIT_AUTHOR_NAME", "Env Person"),
        ("GIT_AUTHOR_EMAIL", "env@example.com"),
    ];
    ok(&acetone(&repo, &["put-node", "Doc", "d2"]));
    ok(&acetone_env(&repo, &["commit", "-m", "env"], &env));
    assert_identity(&repo, "Env Person", "env@example.com");

    ok(&acetone(&repo, &["put-node", "Doc", "d3"]));
    ok(&acetone_env(
        &repo,
        &[
            "commit",
            "-m",
            "flag",
            "--author",
            "Flag <flag@example.com>",
        ],
        &env,
    ));
    assert_identity(&repo, "Flag", "flag@example.com");
}

#[test]
fn cli_refuses_half_an_env_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let before = head_hash(&repo);
    ok(&acetone(&repo, &["put-node", "Doc", "d2"]));
    let err = refused(&acetone_env(
        &repo,
        &["commit", "-m", "half"],
        &[("GIT_AUTHOR_NAME", "Only Name")],
    ));
    assert!(err.contains("GIT_AUTHOR_EMAIL"), "{err}");
    assert_eq!(head_hash(&repo), before);
}

#[test]
fn cli_refuses_injected_authors_on_every_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let before = head_hash(&repo);
    ok(&acetone(&repo, &["put-node", "Doc", "d2"]));

    let err = refused(&acetone(
        &repo,
        &["commit", "-m", "x", "--author", INJECTED],
    ));
    assert!(err.contains("invalid signature"), "commit: {err}");
    // The env path is checked by the same rule.
    let err = refused(&acetone_env(
        &repo,
        &["commit", "-m", "x"],
        &[
            ("GIT_AUTHOR_NAME", "Eve"),
            ("GIT_AUTHOR_EMAIL", "e@x>\ncommitter M <m@x"),
        ],
    ));
    assert!(err.contains("invalid signature"), "env: {err}");
    assert_eq!(head_hash(&repo), before, "nothing committed");

    let err = refused(&acetone(&repo, &["tag", "v1", "--author", INJECTED]));
    assert!(err.contains("invalid signature"), "tag: {err}");
    let tags = ok(&acetone(&repo, &["tag"]));
    assert!(!tags.contains("v1"), "no tag written: {tags}");

    let err = refused(&acetone(&repo, &["merge", "main", "--author", INJECTED]));
    assert!(err.contains("invalid signature"), "merge: {err}");

    let csv = dir.path().join("hosts.csv");
    std::fs::write(&csv, "id\nd9\n").expect("write csv");
    let err = refused(&acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Doc",
            "--author",
            INJECTED,
        ],
    ));
    assert!(err.contains("invalid signature"), "import: {err}");
    assert_eq!(head_hash(&repo), before, "nothing committed");
}

#[test]
fn cli_import_author_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let csv = dir.path().join("docs.csv");
    std::fs::write(&csv, "id\nd7\nd8\n").expect("write csv");
    ok(&acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Doc",
            "--author",
            "Importer <imp@example.com>",
        ],
    ));
    assert_identity(&repo, "Importer", "imp@example.com");
}

#[test]
fn cli_clean_merge_commit_author_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    ok(&acetone(&repo, &["branch", "feature"]));
    ok(&acetone(&repo, &["checkout", "feature"]));
    ok(&acetone(&repo, &["put-node", "Doc", "f1"]));
    ok(&acetone(&repo, &["commit", "-m", "feature"]));
    ok(&acetone(&repo, &["checkout", "main"]));
    ok(&acetone(&repo, &["put-node", "Doc", "m1"]));
    ok(&acetone(&repo, &["commit", "-m", "main"]));
    let out = ok(&acetone(
        &repo,
        &["merge", "feature", "--author", "Merger <merge@example.com>"],
    ));
    assert!(out.contains("merge commit"), "{out}");
    assert_identity(&repo, "Merger", "merge@example.com");
    assert_eq!(
        git_format(&repo, &head_hash(&repo), "%p")
            .split(' ')
            .count(),
        2
    );
}

#[test]
fn cli_tag_author_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    ok(&acetone(
        &repo,
        &["tag", "v1", "--author", "Tagger <tag@example.com>"],
    ));
    let out = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "cat-file",
            "tag",
            "refs/tags/v1",
        ])
        .output()
        .expect("git cat-file");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tag = String::from_utf8(out.stdout).expect("utf-8");
    assert!(
        tag.lines()
            .any(|l| l.starts_with("tagger Tagger <tag@example.com> ")),
        "{tag}"
    );
    // The env fallback reaches tags too.
    ok(&acetone_env(
        &repo,
        &["tag", "v2"],
        &[
            ("GIT_AUTHOR_NAME", "Env Tagger"),
            ("GIT_AUTHOR_EMAIL", "envtag@example.com"),
        ],
    ));
    let out = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "cat-file",
            "tag",
            "refs/tags/v2",
        ])
        .output()
        .expect("git cat-file");
    let tag = String::from_utf8(out.stdout).expect("utf-8");
    assert!(
        tag.lines()
            .any(|l| l.starts_with("tagger Env Tagger <envtag@example.com> ")),
        "{tag}"
    );
}

// --- daemon ------------------------------------------------------------------

struct Daemon {
    child: Child,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start `acetone serve` with `GIT_AUTHOR_*` set in ITS environment: the
/// daemon must never stamp its launcher's identity on a client's commit.
fn start_daemon(repo: &Path, socket: &Path) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_acetone"))
        .args([
            "--repo",
            repo.to_str().unwrap(),
            "serve",
            "--socket",
            socket.to_str().unwrap(),
        ])
        .env("GIT_AUTHOR_NAME", "Daemon Launcher")
        .env("GIT_AUTHOR_EMAIL", "launcher@example.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let stdout = child.stdout.take().expect("stdout");
    let ready = BufReader::new(stdout)
        .lines()
        .next()
        .expect("readiness line")
        .expect("read");
    let ready: Json = serde_json::from_str(&ready).expect("readiness json");
    assert_eq!(ready["ready"], true, "{ready}");
    Daemon { child }
}

fn write_frame(s: &mut impl Write, v: &Json) {
    let b = serde_json::to_vec(v).expect("encode");
    s.write_all(&(b.len() as u32).to_be_bytes()).expect("len");
    s.write_all(&b).expect("body");
}

fn read_frame(s: &mut impl Read) -> Json {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).expect("len");
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    s.read_exact(&mut buf).expect("body");
    serde_json::from_slice(&buf).expect("json")
}

fn hello(socket: &Path) -> UnixStream {
    let mut s = UnixStream::connect(socket).expect("connect");
    let server_hello = read_frame(&mut s);
    assert_eq!(server_hello["acetone"]["protocol"], 1, "{server_hello}");
    write_frame(&mut s, &json!({"acetone": {"protocol": 1}}));
    s
}

/// Send one request and return its terminal (`ok`/`error`) frame.
fn verb(s: &mut UnixStream, req: Json) -> Json {
    write_frame(s, &req);
    terminal(s)
}

fn terminal(s: &mut UnixStream) -> Json {
    let mut frame = read_frame(s);
    while frame.get("ok").is_none() && frame.get("error").is_none() {
        frame = read_frame(s);
    }
    frame
}

fn stage(s: &mut UnixStream, id: u64, key: &str) {
    let r = verb(
        s,
        json!({"id": id, "verb": "query",
               "params": {"cypher": format!("CREATE (:Doc {{id: '{key}'}})")}}),
    );
    assert!(r.get("ok").is_some(), "{r}");
}

#[test]
fn daemon_commit_without_author_is_neutral_despite_its_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);
    stage(&mut s, 1, "d2");
    let c = verb(
        &mut s,
        json!({"id": 2, "verb": "commit", "params": {"message": "anon"}}),
    );
    assert!(c["ok"]["commit"].is_string(), "{c}");
    assert_identity(&repo, NEUTRAL_NAME, NEUTRAL_EMAIL);
}

#[test]
fn daemon_commit_author_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);
    stage(&mut s, 1, "d2");
    let c = verb(
        &mut s,
        json!({"id": 2, "verb": "commit", "params": {
            "message": "authored",
            "author": {"name": "Ada Lovelace", "email": "ada@example.com"},
        }}),
    );
    assert!(c["ok"]["commit"].is_string(), "{c}");
    assert_identity(&repo, "Ada Lovelace", "ada@example.com");
}

#[test]
fn daemon_refuses_bad_authors_with_typed_errors_and_survives() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let before = head_hash(&repo);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);
    stage(&mut s, 1, "d2");

    let injected = json!({"name": "Eve", "email": "eve@example.com>\ncommitter M <m@x"});
    let e = verb(
        &mut s,
        json!({"id": 2, "verb": "commit",
               "params": {"message": "x", "author": injected}}),
    );
    assert_eq!(e["error"]["kind"], "invalid-author", "{e}");
    let e = verb(
        &mut s,
        json!({"id": 3, "verb": "merge",
               "params": {"refspec": "main", "author": injected}}),
    );
    assert_eq!(e["error"]["kind"], "invalid-author", "{e}");
    // The wrong shape is a bad request, not a silently dropped author.
    for bad in [
        json!("Ada <ada@example.com>"),
        json!({"name": "Ada"}),
        // An unknown key is refused, never silently dropped.
        json!({"name": "Ada", "email": "ada@example.com", "emial": "typo"}),
    ] {
        let e = verb(
            &mut s,
            json!({"id": 4, "verb": "commit",
                   "params": {"message": "x", "author": bad}}),
        );
        assert_eq!(e["error"]["kind"], "bad-request", "{e}");
    }
    // Import drains its payload before refusing, so the connection stays
    // in sync.
    write_frame(
        &mut s,
        &json!({"id": 5, "verb": "import",
                "params": {"format": "ndjson", "label": "Doc", "author": injected}}),
    );
    write_frame(&mut s, &json!({"id": 5, "chunk": "{\"id\":\"d9\"}\n"}));
    write_frame(&mut s, &json!({"id": 5, "chunk_end": true}));
    let e = terminal(&mut s);
    assert_eq!(e["error"]["kind"], "invalid-author", "{e}");
    assert_eq!(head_hash(&repo), before, "nothing committed");

    // The connection survives: a good commit still lands.
    let c = verb(
        &mut s,
        json!({"id": 6, "verb": "commit", "params": {"message": "fine"}}),
    );
    assert!(c["ok"]["commit"].is_string(), "{c}");
}

#[test]
fn daemon_merge_and_import_author_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded(&dir);
    let socket = dir.path().join("acetone.sock");
    let _daemon = start_daemon(&repo, &socket);
    let mut s = hello(&socket);

    // Import with an author.
    write_frame(
        &mut s,
        &json!({"id": 1, "verb": "import", "params": {
            "format": "ndjson", "label": "Doc",
            "author": {"name": "Importer", "email": "imp@example.com"},
        }}),
    );
    write_frame(&mut s, &json!({"id": 1, "chunk": "{\"id\":\"d5\"}\n"}));
    write_frame(&mut s, &json!({"id": 1, "chunk_end": true}));
    let r = terminal(&mut s);
    assert_eq!(r["ok"]["imported"], true, "{r}");
    assert_identity(&repo, "Importer", "imp@example.com");

    // Diverge main and feature, then a clean merge with an author.
    let b = verb(
        &mut s,
        json!({"id": 2, "verb": "branch", "params": {"name": "feature"}}),
    );
    assert!(b.get("ok").is_some(), "{b}");
    verb(
        &mut s,
        json!({"id": 3, "verb": "checkout", "params": {"branch": "feature"}}),
    );
    stage(&mut s, 4, "f1");
    verb(
        &mut s,
        json!({"id": 5, "verb": "commit", "params": {"message": "feature"}}),
    );
    verb(
        &mut s,
        json!({"id": 6, "verb": "checkout", "params": {"branch": "main"}}),
    );
    stage(&mut s, 7, "m1");
    verb(
        &mut s,
        json!({"id": 8, "verb": "commit", "params": {"message": "main"}}),
    );
    let m = verb(
        &mut s,
        json!({"id": 9, "verb": "merge", "params": {
            "refspec": "feature",
            "author": {"name": "Merger", "email": "merge@example.com"},
        }}),
    );
    assert_eq!(m["ok"]["outcome"], "merged", "{m}");
    assert_identity(&repo, "Merger", "merge@example.com");
}
