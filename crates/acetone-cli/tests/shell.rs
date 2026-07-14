//! End-to-end tests for the `acetone shell` REPL, driven through its
//! non-interactive (piped-stdin) path — the same statement-accumulation and
//! meta-command logic as the interactive rustyline path, minus the terminal
//! editing that needs a PTY.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn init(repo: &Path) {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let ok = Command::new(bin)
        .args(["init", repo.to_str().unwrap()])
        .output()
        .expect("init")
        .status
        .success();
    assert!(ok, "init");
}

/// Pipe `input` into `acetone shell` and return its output.
fn shell(repo: &Path, input: &str) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shell");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait shell")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_query_runs_in_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), "RETURN 1 + 1 AS two;\n:quit\n");
    let s = out(&o);
    assert!(s.contains("two"), "should show the column: {s}");
    assert!(s.contains('2'), "should show the value: {s}");
}

#[test]
fn declare_then_query_in_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(
        dir.path(),
        ":declare-label Host --key hostname --require os\n\
         CREATE (:Host {hostname: 'web1', os: 'linux'});\n\
         MATCH (h:Host) RETURN h.hostname;\n\
         :quit\n",
    );
    let s = out(&o);
    // The freshly declared label is picked up by the later query in the same
    // session (no stale catalogue), and the create + match both work.
    assert!(
        s.contains("web1"),
        "the created node should be queryable: {s}"
    );
    assert!(
        s.contains("node created") || s.contains("1 row"),
        "the write and read should both run: {s}"
    );
}

#[test]
fn commit_and_status_in_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(
        dir.path(),
        ":declare-label Host --key hostname\n\
         CREATE (:Host {hostname: 'web1'});\n\
         :commit initial import\n\
         :status\n\
         :quit\n",
    );
    let s = out(&o);
    assert!(s.contains("committed "), "commit should report a hash: {s}");
    assert!(s.contains("On branch main"), ":status should run: {s}");
    assert!(s.contains("workspace: clean"), "clean after commit: {s}");
}

#[test]
fn schema_meta_command_shows_declarations() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(
        dir.path(),
        ":declare-label Host --key hostname\n\
         :declare-rel-type RUNS\n\
         :declare-index by_host --label Host --property hostname\n\
         :schema\n\
         :quit\n",
    );
    let s = out(&o);
    assert!(s.contains("Labels"), "{s}");
    assert!(s.contains("Host"), "{s}");
    assert!(s.contains("RUNS"), "{s}");
    assert!(s.contains("by_host"), "{s}");
}

#[test]
fn unknown_meta_command_does_not_end_the_session() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), ":nope\nRETURN 42 AS answer;\n:quit\n");
    let s = out(&o);
    assert!(s.contains("unknown command ':nope'"), "{s}");
    // The session survived the bad command and ran the following query.
    assert!(
        s.contains("42"),
        "query after a bad meta-command should run: {s}"
    );
}

#[test]
fn unterminated_final_statement_runs_at_eof() {
    // A piped query with no trailing ';' and no blank line must still execute
    // (EOF flushes the pending statement) — the common scripting case.
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), "RETURN 7 AS lucky");
    let s = out(&o);
    assert!(s.contains("lucky"), "column: {s}");
    assert!(s.contains('7'), "value: {s}");
}

#[test]
fn shell_query_errors_go_to_stderr_not_stdout() {
    // A query that fails to parse must put its `error:` line on STDERR, so it
    // does not interleave with result rows on STDOUT.
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), "RETURN;\n:quit\n");
    let stdout = out(&o);
    let stderr = err(&o);
    assert!(
        stderr.contains("error:"),
        "the error should be on stderr: stderr={stderr:?} stdout={stdout:?}"
    );
    assert!(
        !stdout.contains("error:"),
        "the error must not be on stdout: {stdout:?}"
    );
}

#[test]
fn shell_meta_command_errors_go_to_stderr() {
    // A meta-command usage error (`:commit` with no message) is a real error →
    // stderr; the informational `unknown command` message stays on stdout.
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), ":commit\n:nope\n:quit\n");
    let stdout = out(&o);
    let stderr = err(&o);
    assert!(
        stderr.contains("error:") && stderr.contains("usage: :commit"),
        "the :commit usage error should be on stderr: {stderr:?}"
    );
    assert!(
        !stdout.contains("error:"),
        "no error line on stdout: {stdout:?}"
    );
    // The unknown-command notice is informational and stays on stdout.
    assert!(
        stdout.contains("unknown command ':nope'"),
        "informational meta output stays on stdout: {stdout:?}"
    );
}

#[test]
fn shell_null_and_string_null_render_distinctly() {
    // A row with a genuine NULL beside the string "null": the NULL renders as
    // the distinct `NULL` marker while the string renders as itself.
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), "RETURN null AS a, 'null' AS b;\n:quit\n");
    let s = out(&o);
    assert!(s.contains("NULL"), "genuine NULL should show as NULL: {s}");
    assert!(
        s.contains("null"),
        "the string 'null' should show as null: {s}"
    );
}

#[test]
fn shell_caps_large_result_and_reports_true_total() {
    // The shell caps table output at SHELL_ROW_CAP (1000) rows and prints a
    // notice for the remainder, while the `N rows` line reports the true total.
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), "UNWIND range(1, 1500) AS n RETURN n;\n:quit\n");
    let s = out(&o);
    assert!(
        s.contains("more rows (showing first 1000"),
        "the cap notice should appear: {s}"
    );
    assert!(s.contains("500 more rows"), "true remainder reported: {s}");
    assert!(s.contains("1500 rows"), "true total reported: {s}");
    // The first row is shown; a row past the cap is not.
    assert!(s.contains("│ 1 "), "first row shown: {s}");
    assert!(!s.contains("│ 1200 "), "a row past the cap is hidden: {s}");
}

#[test]
fn shell_wide_char_table_borders_align() {
    // A CJK value is two terminal cells wide per character; the box-drawing
    // borders must line up with the content when measured by display width.
    use unicode_width::UnicodeWidthStr;
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), "RETURN '你好世界' AS name;\n:quit\n");
    let s = out(&o);
    let border = s
        .lines()
        .find(|l| l.starts_with('┌'))
        .expect("top border present");
    let content = s
        .lines()
        .find(|l| l.contains("你好世界"))
        .expect("content row present");
    assert_eq!(
        UnicodeWidthStr::width(border),
        UnicodeWidthStr::width(content),
        "border and content display widths must match:\n{s}"
    );
}

#[test]
fn help_lists_the_meta_commands() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path());
    let o = shell(dir.path(), ":help\n:quit\n");
    let s = out(&o);
    for expected in [":commit", ":status", ":schema", ":declare-label", ":cancel"] {
        assert!(s.contains(expected), "help should list {expected}: {s}");
    }
}
