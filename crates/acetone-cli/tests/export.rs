//! End-to-end `acetone export` round-trip test (spec §9, acetone-6g5.2): a
//! graph exported and re-imported into a fresh repo reproduces identical map
//! roots (Invariant #1: identical content → identical roots).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use acetone_graph::repo::Repository;

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

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf8")
}

fn ok(o: Output, what: &str) {
    assert!(o.status.success(), "{what}: {}", stderr(&o));
}

/// Declare the Host/Software/RUNS schema and commit it.
fn declare_schema(repo: &Path) {
    ok(
        acetone(repo, &["declare-label", "Host", "--key", "name"]),
        "declare Host",
    );
    ok(
        acetone(repo, &["declare-label", "Software", "--key", "name"]),
        "declare Software",
    );
    ok(acetone(repo, &["declare-rel-type", "RUNS"]), "declare RUNS");
    // A declared index, so the round-trip also proves the `indexes` map is
    // rebuilt to an identical root on reimport (export omits it — Invariant #5).
    ok(
        acetone(
            repo,
            &[
                "declare-index",
                "host_os",
                "--label",
                "Host",
                "--property",
                "os",
            ],
        ),
        "declare index",
    );
    ok(acetone(repo, &["commit", "-m", "schema"]), "commit schema");
}

/// Populate a repo (nodes with mixed-typed properties via NDJSON, plus edges).
fn populate(repo: &Path, dir: &Path) {
    let hosts = dir.join("hosts.ndjson");
    fs::write(
        &hosts,
        "{\"name\":\"web1\",\"os\":\"linux\",\"cores\":8,\"up\":true}\n\
         {\"name\":\"db1\",\"os\":\"linux\",\"cores\":16,\"up\":false}\n",
    )
    .expect("write");
    ok(
        acetone(
            repo,
            &[
                "import",
                "--format",
                "ndjson",
                hosts.to_str().unwrap(),
                "--label",
                "Host",
            ],
        ),
        "import hosts",
    );

    let sw = dir.join("sw.ndjson");
    fs::write(&sw, "{\"name\":\"nginx\",\"version\":\"1.25\"}\n").expect("write");
    ok(
        acetone(
            repo,
            &[
                "import",
                "--format",
                "ndjson",
                sw.to_str().unwrap(),
                "--label",
                "Software",
            ],
        ),
        "import sw",
    );

    let edges = dir.join("edges.ndjson");
    fs::write(&edges, "{\"src\":\"web1\",\"dst\":\"nginx\"}\n").expect("write");
    ok(
        acetone(
            repo,
            &[
                "import",
                "--format",
                "ndjson",
                edges.to_str().unwrap(),
                "--edge",
                "RUNS",
                "--from",
                "Host=src",
                "--to",
                "Software=dst",
            ],
        ),
        "import edges",
    );
}

/// Import the exported tables from `exp` into a repo with the schema declared.
fn import_exported(repo: &Path, exp: &Path, ext: &str) {
    let node = |label: &str| exp.join(format!("{label}.{ext}"));
    let fmt = ext;
    ok(
        acetone(
            repo,
            &[
                "import",
                "--format",
                fmt,
                node("Host").to_str().unwrap(),
                "--label",
                "Host",
            ],
        ),
        "reimport Host",
    );
    ok(
        acetone(
            repo,
            &[
                "import",
                "--format",
                fmt,
                node("Software").to_str().unwrap(),
                "--label",
                "Software",
            ],
        ),
        "reimport Software",
    );
    ok(
        acetone(
            repo,
            &[
                "import",
                "--format",
                fmt,
                exp.join(format!("rel-RUNS.{ext}")).to_str().unwrap(),
                "--edge",
                "RUNS",
                "--from",
                "Host=src",
                "--to",
                "Software=dst",
            ],
        ),
        "reimport edges",
    );
}

/// Assert two repos have identical node/edge/schema map roots.
fn assert_same_roots(a: &Path, b: &Path) {
    let ra = Repository::open(&a.join("repo")).expect("open a");
    let rb = Repository::open(&b.join("repo")).expect("open b");
    let ma = ra.workspace_manifest().expect("manifest a");
    let mb = rb.workspace_manifest().expect("manifest b");
    assert_eq!(ma.nodes, mb.nodes, "nodes map roots differ");
    assert_eq!(ma.edges_fwd, mb.edges_fwd, "edges_fwd roots differ");
    assert_eq!(ma.edges_rev, mb.edges_rev, "edges_rev roots differ");
    assert_eq!(ma.schema, mb.schema, "schema roots differ");
    // The derived index map, rebuilt on import, must also match (Invariant #5).
    assert_eq!(ma.indexes, mb.indexes, "index map roots differ");
}

fn round_trip(ext: &str) {
    let src = tempfile::tempdir().expect("tmp");
    let dst = tempfile::tempdir().expect("tmp");
    let src_repo = src.path().join("repo");
    let dst_repo = dst.path().join("repo");

    // Source graph.
    assert!(init(&src_repo).status.success());
    declare_schema(&src_repo);
    populate(&src_repo, src.path());

    // Export it.
    let exp = src.path().join("export");
    fs::create_dir_all(&exp).expect("mkdir");
    ok(
        acetone(
            &src_repo,
            &["export", "--format", ext, "--out", exp.to_str().unwrap()],
        ),
        "export",
    );

    // Fresh repo, same schema, import the export.
    assert!(init(&dst_repo).status.success());
    declare_schema(&dst_repo);
    import_exported(&dst_repo, &exp, ext);

    assert_same_roots(src.path(), dst.path());
}

#[test]
fn ndjson_round_trip_reproduces_identical_map_roots() {
    round_trip("ndjson");
}

#[test]
fn json_round_trip_reproduces_identical_map_roots() {
    round_trip("json");
}

#[test]
fn csv_round_trip_reproduces_identical_map_roots_for_string_properties() {
    // CSV cells are untyped; without a typed schema (the CLI cannot declare
    // property types yet), only string-valued properties round-trip exactly.
    let src = tempfile::tempdir().expect("tmp");
    let dst = tempfile::tempdir().expect("tmp");
    let src_repo = src.path().join("repo");
    let dst_repo = dst.path().join("repo");

    assert!(init(&src_repo).status.success());
    declare_schema(&src_repo);
    // String-only properties.
    let hosts = src.path().join("hosts.csv");
    fs::write(&hosts, "name,os,region\nweb1,linux,eu\ndb1,linux,us\n").expect("write");
    ok(
        acetone(
            &src_repo,
            &[
                "import",
                "--format",
                "csv",
                hosts.to_str().unwrap(),
                "--label",
                "Host",
            ],
        ),
        "import hosts",
    );
    let sw = src.path().join("sw.csv");
    fs::write(&sw, "name,version\nnginx,stable\n").expect("write");
    ok(
        acetone(
            &src_repo,
            &[
                "import",
                "--format",
                "csv",
                sw.to_str().unwrap(),
                "--label",
                "Software",
            ],
        ),
        "import sw",
    );
    let edges = src.path().join("edges.csv");
    fs::write(&edges, "src,dst\nweb1,nginx\n").expect("write");
    ok(
        acetone(
            &src_repo,
            &[
                "import",
                "--format",
                "csv",
                edges.to_str().unwrap(),
                "--edge",
                "RUNS",
                "--from",
                "Host=src",
                "--to",
                "Software=dst",
            ],
        ),
        "import edges",
    );

    let exp = src.path().join("export");
    fs::create_dir_all(&exp).expect("mkdir");
    ok(
        acetone(
            &src_repo,
            &["export", "--format", "csv", "--out", exp.to_str().unwrap()],
        ),
        "export",
    );

    assert!(init(&dst_repo).status.success());
    declare_schema(&dst_repo);
    import_exported(&dst_repo, &exp, "csv");

    assert_same_roots(src.path(), dst.path());
}
