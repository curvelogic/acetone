//! End-to-end `declare-label --type` tests over the real binary
//! (`acetone-2ck.18`, ADR-0066).
//!
//! Property types were declarable only through the library: `declare_label`
//! passed an empty types map unconditionally and there was no flag. So a
//! graph built entirely through the shipped CLI had no declared types ever,
//! which meant every string equality seek declined to a scan — secondary
//! indexes on string properties were, for CLI users, decorative.
//!
//! These tests cover the flag, the enforcement that makes a declaration
//! something the seek guard may rely on, and the reachability corollary: the
//! whole path exercised through the binary, not the library.

use std::path::Path;
use std::process::{Command, Output};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn init(repo: &Path) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    Command::new(bin)
        .args(["init", repo.to_str().unwrap()])
        .output()
        .expect("init")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf8")
}
fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf8")
}

/// A repository with `Host` declared, keyed on `hostname`.
fn repo_with_host(dir: &tempfile::TempDir, types: &[&str]) -> std::path::PathBuf {
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let mut args = vec!["declare-label", "Host", "--key", "hostname"];
    for t in types {
        args.push("--type");
        args.push(t);
    }
    let out = acetone(&repo, &args);
    assert!(out.status.success(), "declare-label: {}", stderr(&out));
    repo
}

#[test]
fn declared_types_appear_in_schema_output() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &["os:string", "cores:int"]);

    let text = stdout(&acetone(&repo, &["schema"]));
    assert!(
        text.contains("\"os\": string") && text.contains("\"cores\": int"),
        "schema must render declared types, got:\n{text}"
    );

    // And in the machine format, since that is what tooling reads.
    let json = stdout(&acetone(&repo, &["schema", "--json"]));
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    let types = &parsed["labels"][0]["types"];
    assert_eq!(types["os"], "string");
    assert_eq!(types["cores"], "int");
}

#[test]
fn every_type_name_is_accepted() {
    // The CLI must accept exactly the names `PropertyType::parse` knows; a
    // name that parses but is unreachable from the flag is the bug this bead
    // exists to fix, one layer down.
    for name in acetone_core::model::schema::PropertyType::names() {
        let dir = tempfile::tempdir().expect("tmp");
        let repo = repo_with_host(&dir, &[&format!("p:{name}")]);
        let text = stdout(&acetone(&repo, &["schema"]));
        assert!(
            text.contains(&format!("\"p\": {name}")),
            "type {name} must be declarable and shown, got:\n{text}"
        );
    }
}

#[test]
fn malformed_type_flags_are_rejected() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let cases: &[(&str, &str)] = &[
        // No colon at all.
        ("os", "expects <property>:<type>"),
        // Unknown type name.
        ("os:strung", "unknown type"),
        // Empty property name.
        (":string", "empty property"),
    ];
    for (spec, expected) in cases {
        let out = acetone(
            &repo,
            &["declare-label", "Host", "--key", "hostname", "--type", spec],
        );
        assert!(
            !out.status.success(),
            "--type {spec} must be rejected, but succeeded"
        );
        let text = stderr(&out);
        assert!(
            text.contains(expected),
            "--type {spec}: expected {expected:?} in:\n{text}"
        );
    }
}

#[test]
fn declaring_one_property_twice_is_an_error_not_last_wins() {
    // Two contradicting declarations on one command line is a mistake.
    // Silently picking one would leave the user believing the other took
    // effect — and, since the seek guard reads the declaration, believing it
    // about the type an index probe will trust.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let out = acetone(
        &repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "hostname",
            "--type",
            "os:string",
            "--type",
            "os:int",
        ],
    );
    assert!(!out.status.success(), "duplicate --type must be rejected");
    assert!(
        stderr(&out).contains("more than once"),
        "expected a duplicate-property error, got:\n{}",
        stderr(&out)
    );
}

#[test]
fn a_write_contradicting_a_declared_type_is_rejected() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &["os:string", "cores:int"]);

    // Conforming write lands.
    let ok = acetone(
        &repo,
        &[
            "query",
            "CREATE (h:Host {hostname: 'h1', os: 'debian', cores: 8})",
        ],
    );
    assert!(ok.status.success(), "conforming write: {}", stderr(&ok));

    // CREATE with the wrong type is refused.
    let bad_create = acetone(
        &repo,
        &[
            "query",
            "CREATE (h:Host {hostname: 'h2', os: 'debian', cores: 'eight'})",
        ],
    );
    assert!(!bad_create.status.success(), "mistyped CREATE must fail");
    // Assert the PERSIST path's own wording, not just "declared int": the
    // save-time chokepoint refuses this too, and its message contains that
    // substring as well — so a weaker assertion passed with the persist check
    // disabled entirely (PR #226 review, S2). `persist` is the earlier and
    // friendlier of the two, and the only one that sees runtime values.
    assert!(
        stderr(&bad_create).contains("the value written is of type string"),
        "expected the persist-path type error, got:\n{}",
        stderr(&bad_create)
    );

    // SET with the wrong type is refused too — the path that would otherwise
    // corrupt an already-conforming node.
    let bad_set = acetone(
        &repo,
        &["query", "MATCH (h:Host {hostname:'h1'}) SET h.os = 42"],
    );
    assert!(!bad_set.status.success(), "mistyped SET must fail");

    // Neither failure left anything behind: still one node, still conforming.
    let rows = stdout(&acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN h.hostname AS n",
            "--format",
            "csv",
        ],
    ));
    assert!(rows.contains("h1"), "the conforming node must survive");
    assert!(
        !rows.contains("h2"),
        "the rejected CREATE must not have landed:\n{rows}"
    );
}

#[test]
fn null_does_not_violate_a_declared_type() {
    // An absent or null property is existence's business (REQUIRE,
    // ADR-0061), not the type system's. Declaring `os: string` must not make
    // a node without `os` unwritable.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &["os:string"]);

    let out = acetone(&repo, &["query", "CREATE (h:Host {hostname: 'bare'})"]);
    assert!(
        out.status.success(),
        "a node omitting a typed property must be writable: {}",
        stderr(&out)
    );
    let explicit = acetone(
        &repo,
        &["query", "CREATE (h:Host {hostname: 'n', os: null})"],
    );
    assert!(
        explicit.status.success(),
        "an explicit null must satisfy a declared type: {}",
        stderr(&explicit)
    );
}

#[test]
fn declaring_a_type_over_contradicting_data_is_refused() {
    // Mirrors the existing --require/--unique backfill check (acetone-9gw):
    // accepting a declaration the data already contradicts would leave the
    // seek guard trusting something false.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &[]);

    assert!(
        acetone(
            &repo,
            &["query", "CREATE (h:Host {hostname:'h1', os:'debian'})"]
        )
        .status
        .success()
    );
    assert!(
        acetone(&repo, &["query", "CREATE (h:Host {hostname:'h2', os:99})"])
            .status
            .success()
    );

    let out = acetone(
        &repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "hostname",
            "--type",
            "os:string",
        ],
    );
    assert!(
        !out.status.success(),
        "declaring a type the data violates must be refused"
    );
    let text = stderr(&out);
    assert!(
        text.contains("h2") && text.contains("declared string"),
        "the refusal must name the violating node and the declaration:\n{text}"
    );

    // A declaration the data DOES satisfy is accepted.
    assert!(
        acetone(
            &repo,
            &[
                "declare-label",
                "Host",
                "--key",
                "hostname",
                "--type",
                "hostname:string",
            ],
        )
        .status
        .success()
    );
}

#[test]
fn a_cli_built_graph_serves_a_string_equality_seek() {
    // The corollary this bead exists for (CLAUDE.md): a capability not
    // reachable through the shipped interface is not delivered. With no way
    // to declare a type through the CLI, `probe_value` declined every string
    // pin on a CLI-built graph, so the seek fell back to a scan and the
    // declared index was decorative.
    //
    // Correctness is what is asserted here — the seek must return exactly the
    // scan's rows. That the seek is *taken* is pinned at the library level in
    // acetone-cypher's store_source tests; what is new is that the guard can
    // open at all for a graph built this way.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &["os:string"]);
    assert!(
        acetone(
            &repo,
            &[
                "declare-index",
                "host_os",
                "--label",
                "Host",
                "--property",
                "os"
            ],
        )
        .status
        .success()
    );

    for (name, os) in [("a", "debian"), ("b", "ubuntu"), ("c", "debian")] {
        assert!(
            acetone(
                &repo,
                &[
                    "query",
                    &format!("CREATE (h:Host {{hostname: '{name}', os: '{os}'}})"),
                ],
            )
            .status
            .success()
        );
    }

    let seek = stdout(&acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host {os: 'debian'}) RETURN h.hostname AS n ORDER BY n",
            "--format",
            "csv",
        ],
    ));
    let scan = stdout(&acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) WHERE h.os = 'debian' RETURN h.hostname AS n ORDER BY n",
            "--format",
            "csv",
        ],
    ));
    assert!(
        seek.contains('a') && seek.contains('c') && !seek.contains('b'),
        "the pinned seek must find exactly the debian hosts:\n{seek}"
    );
    assert_eq!(
        seek, scan,
        "the seek must return exactly the scan's rows — under-selection is \
         the failure an unenforced declaration would cause"
    );
}

#[test]
fn the_shell_declare_label_form_takes_types_too() {
    // The `:declare-label` meta-command is a separate parse path; a flag
    // added to one and not the other is half an interface.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let bin = env!("CARGO_BIN_EXE_acetone");
    use std::io::Write;
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn shell");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b":declare-label Host --key hostname --type os:string\n:schema\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    let text = stdout(&out);
    assert!(
        text.contains("\"os\": string"),
        "the shell form must declare types too, got:\n{text}"
    );
}

#[test]
fn put_node_cannot_bypass_a_declared_type() {
    // The Cypher write path is not the only writer. `put-node` stages a node
    // record straight onto a `Transaction`, never touching `persist.rs`, so
    // enforcing types only there left this path able to store a value
    // contradicting its declaration — and `probe_value` decides a raw string
    // index probe is safe from the declaration ALONE, so one unchecked writer
    // is enough to make a seek under-select.
    //
    // Enforcement therefore lives at `save`, the chokepoint every writer
    // passes through (ADR-0066).
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &["cores:int"]);

    let bad = acetone(
        &repo,
        &["put-node", "Host", "h1", "--prop", "cores=\"eight\""],
    );
    assert!(
        !bad.status.success(),
        "put-node must not bypass the declared type"
    );
    assert!(
        stderr(&bad).contains("declared int"),
        "expected a type error naming the declaration, got:\n{}",
        stderr(&bad)
    );

    // The conforming form still works, and nothing from the refusal landed.
    assert!(
        acetone(&repo, &["put-node", "Host", "h2", "--prop", "cores=8"])
            .status
            .success()
    );
    let rows = stdout(&acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN h.hostname AS n ORDER BY n",
            "--format",
            "csv",
        ],
    ));
    assert!(rows.contains("h2"), "the conforming node must be stored");
    assert!(
        !rows.contains("h1"),
        "the refused put-node must not have landed:\n{rows}"
    );
}

#[test]
fn declaring_a_type_and_writing_conforming_values_in_one_transaction_succeeds() {
    // The save-time check reads the schema AFTER this transaction's own schema
    // ops are applied. If it read the pre-transaction schema instead, this
    // would still pass (no type declared yet); if it read the post-transaction
    // schema but applied it in the wrong order, a declare-then-write shell
    // session would fail spuriously. Pinned through the shell, where both
    // land in one workspace before any commit.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let bin = env!("CARGO_BIN_EXE_acetone");
    use std::io::Write;
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn shell");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b":declare-label Host --key hostname --type cores:int\n\
              CREATE (h:Host {hostname: 'h1', cores: 4})\n\
              MATCH (h:Host) RETURN h.cores AS c\n",
        )
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    let text = stdout(&out);
    assert!(
        text.contains('4'),
        "declaring a type and writing a conforming value in one session must \
         succeed, got:\n{text}\nstderr:\n{}",
        stderr(&out)
    );
}

#[test]
fn a_key_property_type_is_enforced_and_backfill_checked() {
    // Key properties are the subtle half (PR #226 review, B2). A node's key
    // values live in its NodeKey, not its NodeRecord, so the declare-time,
    // save-time and merge-time checks — all of which read
    // `record.properties()` — silently exempted exactly the properties whose
    // type matters most. `acetone-2ck.17` extends the seek guard's trust to
    // key properties, where a missed row is a missed identity.
    //
    // An earlier version of this suite asserted that declaring
    // `hostname:string` over string data is ACCEPTED. That assertion was
    // vacuous: it passed for any declared type, including a contradicting
    // one, because nothing looked at key values at all.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &[]);
    assert!(
        acetone(&repo, &["query", "CREATE (h:Host {hostname:'h1'})"])
            .status
            .success()
    );

    // Declaring a key type the stored keys contradict must be refused.
    let refused = acetone(
        &repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "hostname",
            "--type",
            "hostname:int",
        ],
    );
    assert!(
        !refused.status.success(),
        "a key-property type the data contradicts must be refused"
    );
    assert!(
        stderr(&refused).contains("hostname") && stderr(&refused).contains("declared int"),
        "the refusal must name the key property and declaration, got:\n{}",
        stderr(&refused)
    );

    // And the truthful declaration is still accepted — the positive case,
    // now meaningful because the negative one above can fail.
    assert!(
        acetone(
            &repo,
            &[
                "declare-label",
                "Host",
                "--key",
                "hostname",
                "--type",
                "hostname:string",
            ],
        )
        .status
        .success(),
        "a key-property type the data satisfies must be accepted"
    );

    // put-node must not be able to write a contradicting key value either.
    let dir2 = tempfile::tempdir().expect("tmp");
    let repo2 = repo_with_host(&dir2, &["hostname:int"]);
    let bad = acetone(&repo2, &["put-node", "Host", "h3", "--prop", "os=x"]);
    assert!(
        !bad.status.success(),
        "put-node must not write a key value contradicting its declared type"
    );
}

#[test]
fn fsck_reports_a_declared_type_violation() {
    // fsck is the only tool that surfaces a violation the write path did not
    // catch — and ADR-0066 accepts that such violations exist, since the
    // format is unchanged and merge deliberately leaves untouched
    // pre-existing breaches alone. Its fast path skipped the whole check
    // unless a label declared REQUIRE or UNIQUE, so a types-only
    // declaration — exactly what `declare-label --type` produces — made fsck
    // report "clean" over a graph whose index could under-select (review B1).
    //
    // Reached here through a retyping merge, which is a legal way to arrive
    // at a violating workspace.
    // The branch forks BEFORE the violating node exists, so each side is
    // individually legal — the retyping branch has nothing to contradict it,
    // and the node's branch has no type declared. Only the merge breaches.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &[]);
    assert!(acetone(&repo, &["commit", "-m", "base"]).status.success());
    assert!(acetone(&repo, &["branch", "retype"]).status.success());

    // The retyping side: declare os:int over an empty graph.
    assert!(acetone(&repo, &["checkout", "retype"]).status.success());
    assert!(
        acetone(
            &repo,
            &[
                "declare-label",
                "Host",
                "--key",
                "hostname",
                "--type",
                "os:int"
            ],
        )
        .status
        .success(),
        "declaring os:int on the branch (no nodes here contradict it yet)"
    );
    assert!(acetone(&repo, &["commit", "-m", "retype"]).status.success());

    // The data side: a string `os`, legal under the untyped base schema.
    assert!(acetone(&repo, &["checkout", "main"]).status.success());
    assert!(
        acetone(
            &repo,
            &["query", "CREATE (h:Host {hostname:'h1', os:'debian'})"]
        )
        .status
        .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "data"]).status.success());

    // The merge reports the breach as a conflict...
    let merged = acetone(&repo, &["merge", "retype", "-m", "merge"]);
    let merge_text = format!("{}{}", stdout(&merged), stderr(&merged));
    assert!(
        merge_text.contains("declared int"),
        "the merge must report the type breach, got:\n{merge_text}"
    );

    // ...and fsck must not call the resulting workspace clean.
    let fsck = acetone(&repo, &["fsck"]);
    let text = format!("{}{}", stdout(&fsck), stderr(&fsck));
    assert!(
        !text.contains("fsck: clean"),
        "fsck must not report clean over a declared-type violation, got:\n{text}"
    );
    assert!(
        text.contains("declared int") || text.contains("os"),
        "fsck must name the violation, got:\n{text}"
    );
}

/// `acetone-2ck.17`, end to end: a primary-key point lookup by string key
/// answers identically to the scan spelling of the same predicate.
///
/// The key seek declined every string pin until the declared type could rule
/// the rendering hazard out, and a graph built through the CLI could not
/// declare one — so this whole path was scan-only for CLI users. What is
/// asserted here is agreement (a seek must never return fewer rows than the
/// scan it replaces); that the seek is *taken* is pinned at the source level
/// in acetone-cypher's `store_source` tests and measured by the lab.
#[test]
fn a_cli_built_graph_answers_a_string_key_lookup_like_the_scan() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = repo_with_host(&dir, &["hostname:string"]);

    for name in ["alpha", "beta", "gamma"] {
        assert!(
            acetone(
                &repo,
                &["query", &format!("CREATE (h:Host {{hostname: '{name}'}})")],
            )
            .status
            .success()
        );
    }

    let seek = stdout(&acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host {hostname: 'beta'}) RETURN h.hostname AS n",
            "--format",
            "csv",
        ],
    ));
    let scan = stdout(&acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) WHERE h.hostname = 'beta' RETURN h.hostname AS n",
            "--format",
            "csv",
        ],
    ));
    assert_eq!(seek, scan, "the two spellings must agree");
    assert!(
        seek.contains("beta"),
        "and both find the node, got:\n{seek}"
    );
    assert_eq!(
        seek.lines().filter(|l| l.contains("beta")).count(),
        1,
        "exactly one row, got:\n{seek}"
    );
}
