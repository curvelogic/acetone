//! Phase 0 benchmark harness (bead acetone-28x.4) over the throwaway
//! prolly-git spike.
//!
//! Measures the roadmap Phase 0 scenarios against a representative
//! asset-registry envelope: bulk load, point read, range scan, single-key
//! update (write amplification via `Store::chunks_written()`), diff between
//! adjacent versions, repo growth over 100 simulated import commits, and
//! loose-vs-packed read performance.
//!
//! The harness persists repos under `--dir` and commits state to
//! `refs/bench/v0` (the base version) and `refs/bench/head` (the current
//! version, parent-chained), so scenarios can be run individually and
//! results reproduced. Progress goes to stderr; results to stdout.
//!
//! IMPORTANT caveats carried from the spike review (annotate any reading of
//! the numbers with these):
//! - `apply_batch` loads ALL internal nodes per batch, so update latency at
//!   scale is NOT architecture-representative (a real implementation loads
//!   only the root-to-leaf path). Chunk-write counts are the meaningful
//!   write-amplification number; latency is spike-only.
//! - `chunks_written()` counts the manifest blob too: +1 per commit.
//!
//! Usage:
//!   bench --keys <100k|1m|5m|N> --dir <DIR> [--scenarios a,b,c]
//!         [--samples 1000] [--windows 100] [--window-size 1000]
//!         [--updates 100] [--growth-commits 100]
//!
//! Scenarios (default: all applicable, in this order):
//!   bulk-load, point-read, scan, update, diff, growth, repack-read
//! `growth` is skipped by default above 2M keys (the roadmap pins it to the
//! 1M-key graph); pass it explicitly to force.
//!
//! Additional opt-in scenarios for the pack-on-write experiment (bead
//! acetone-63m.10), not part of the default set — each uses its own fresh
//! repo under `--dir`:
//!   pack-growth  the growth scenario re-run writing one hand-rolled pack
//!                per commit with explicitly chosen REF_DELTA bases
//!   pack-probe   does git tolerate on-disk packs whose REF_DELTA bases
//!                live outside the pack (no --fix-thin completion)?

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use prolly_git_spike::pack::{self, PackEntry};
use prolly_git_spike::{BatchOp, Root, Store};

// ---------------------------------------------------------------------------
// Deterministic data generation
// ---------------------------------------------------------------------------

/// splitmix64: tiny deterministic RNG so runs are exactly reproducible
/// without a rand dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Base key for asset index `i`. Zero-padded so byte order == numeric order.
fn key_for(i: u64) -> Vec<u8> {
    format!("asset/{i:010}").into_bytes()
}

/// Key for an asset inserted after bulk load (diff scenario): the `x`
/// suffix sorts it between neighbouring base keys, spreading inserts across
/// the whole keyspace rather than appending at the end.
fn insert_key_for(i: u64) -> Vec<u8> {
    format!("asset/{i:010}x").into_bytes()
}

const VENDORS: [&str; 8] = [
    "acme",
    "globex",
    "initech",
    "umbrella",
    "tyrell",
    "wayne",
    "aperture",
    "cyberdyne",
];

/// A JSON-ish asset record, ~150-300 bytes, deterministic in `(i, ver)`.
/// Structured text with repeated field names compresses like real registry
/// records (unlike random bytes), which matters for repo-size numbers.
/// Distinct `ver` values guarantee a distinct record for the same key, so a
/// put in a churn batch always really changes the value.
fn value_for(i: u64, ver: u64) -> Vec<u8> {
    let mut rng = Rng::new(i.wrapping_mul(0x0100_0000_01b3).wrapping_add(ver));
    let vendor = VENDORS[(rng.below(8)) as usize];
    let serial = rng.next();
    let fw = (rng.below(9), rng.below(20), rng.below(100));
    let site = rng.below(400);
    let ntags = 1 + rng.below(4);
    let tags: Vec<String> = (0..ntags)
        .map(|_| format!("\"t{:03}\"", rng.below(500)))
        .collect();
    let notes_len = (rng.below(120)) as usize;
    let mut notes = String::with_capacity(notes_len);
    while notes.len() < notes_len {
        notes.push((b'a' + (rng.below(26)) as u8) as char);
    }
    format!(
        "{{\"id\":\"asset/{i:010}\",\"vendor\":\"{vendor}\",\"serial\":\"{serial:016x}\",\
         \"fw\":\"{}.{}.{}\",\"site\":\"site-{site:03}\",\"rev\":{ver},\
         \"tags\":[{}],\"notes\":\"{notes}\"}}",
        fw.0,
        fw.1,
        fw.2,
        tags.join(",")
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Small measurement helpers
// ---------------------------------------------------------------------------

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn dur_stats(mut samples: Vec<Duration>) -> (Duration, Duration, Duration, Duration) {
    samples.sort();
    let total: Duration = samples.iter().sum();
    let mean = total / samples.len().max(1) as u32;
    (
        mean,
        percentile(&samples, 50.0),
        percentile(&samples, 99.0),
        *samples.last().unwrap_or(&Duration::ZERO),
    )
}

fn fmt_dur(d: Duration) -> String {
    if d >= Duration::from_secs(10) {
        format!("{:.1}s", d.as_secs_f64())
    } else if d >= Duration::from_millis(10) {
        format!("{:.1}ms", d.as_secs_f64() * 1e3)
    } else {
        format!("{:.1}us", d.as_secs_f64() * 1e6)
    }
}

/// `du -sk` of a directory, in KiB (apparent on-disk usage incl. fs blocks).
fn du_kb(path: &Path) -> u64 {
    let out = Command::new("du")
        .arg("-sk")
        .arg(path)
        .output()
        .expect("du");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn fmt_kb(kb: u64) -> String {
    if kb >= 10 * 1024 * 1024 {
        format!("{:.1} GiB", kb as f64 / (1024.0 * 1024.0))
    } else if kb >= 10 * 1024 {
        format!("{:.1} MiB", kb as f64 / 1024.0)
    } else {
        format!("{kb} KiB")
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    if !out.status.success() {
        eprintln!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn count_objects(repo: &Path) -> String {
    git(repo, &["count-objects", "-v"])
        .trim()
        .replace('\n', ", ")
}

fn progress(msg: &str) {
    eprintln!("[bench] {msg}");
    std::io::stderr().flush().ok();
}

// ---------------------------------------------------------------------------
// Structural diff by OID comparison (scenario 5)
// ---------------------------------------------------------------------------
//
// The spike has no diff; this is the minimal OID-comparison tree walk the
// bead asks for, implemented harness-side. The bench binary is a separate
// crate from the spike lib, whose node decoding is private, so the
// documented node encoding (tree.rs header) is re-implemented here and
// chunks are read straight from the git ODB via gix.
//
// Walk: start from both roots; wherever the two sides' child OIDs are
// equal, the whole subtree is skipped (content addressing guarantees equal
// contents); only mismatched, key-aligned regions are descended, and only
// leaf entries inside mismatched leaves are compared. Cost is O(changed
// chunks + boundary chunks per level), independent of map size.

mod tdiff {
    use gix::ObjectId;
    use std::cell::Cell;
    use std::path::Path;

    pub struct RawRef {
        pub last_key: Vec<u8>,
        pub oid: ObjectId,
    }

    type LeafEntries = Vec<(Vec<u8>, Vec<u8>)>;

    pub enum RawNode {
        Leaf(LeafEntries),
        Inner(Vec<RawRef>),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum ChangeKind {
        Added,
        Removed,
        Modified,
    }

    /// Reads and decodes tree nodes directly from the git ODB, counting
    /// chunk reads so the walk cost is observable.
    pub struct Reader {
        repo: gix::Repository,
        pub reads: Cell<u64>,
    }

    fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
        if buf.len() < n {
            return Err("truncated node".into());
        }
        let (head, tail) = buf.split_at(n);
        *buf = tail;
        Ok(head)
    }

    fn take_u32(buf: &mut &[u8]) -> Result<u32, String> {
        let b = take(buf, 4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Decode per the spike's documented node encoding:
    /// `level:u8 count:u32be entries...`; leaf entries `klen key vlen value`,
    /// inner entries `klen last_key olen oid`.
    fn decode(data: &[u8]) -> Result<(u8, RawNode), String> {
        let mut buf = data;
        let level = take(&mut buf, 1)?[0];
        let count = take_u32(&mut buf)? as usize;
        if level == 0 {
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let klen = take_u32(&mut buf)? as usize;
                let key = take(&mut buf, klen)?.to_vec();
                let vlen = take_u32(&mut buf)? as usize;
                let value = take(&mut buf, vlen)?.to_vec();
                entries.push((key, value));
            }
            Ok((level, RawNode::Leaf(entries)))
        } else {
            let mut refs = Vec::with_capacity(count);
            for _ in 0..count {
                let klen = take_u32(&mut buf)? as usize;
                let last_key = take(&mut buf, klen)?.to_vec();
                let olen = take(&mut buf, 1)?[0] as usize;
                let oid = ObjectId::try_from(take(&mut buf, olen)?).map_err(|e| e.to_string())?;
                refs.push(RawRef { last_key, oid });
            }
            Ok((level, RawNode::Inner(refs)))
        }
    }

    impl Reader {
        pub fn open(path: &Path) -> Result<Self, String> {
            Ok(Reader {
                repo: gix::open(path).map_err(|e| e.to_string())?,
                reads: Cell::new(0),
            })
        }

        fn read_node(&self, oid: &ObjectId) -> Result<RawNode, String> {
            self.reads.set(self.reads.get() + 1);
            let obj = self.repo.find_object(*oid).map_err(|e| e.to_string())?;
            Ok(decode(&obj.data)?.1)
        }

        fn expand(&self, refs: &[&RawRef]) -> Result<Vec<RawRef>, String> {
            let mut out = Vec::new();
            for r in refs {
                match self.read_node(&r.oid)? {
                    RawNode::Inner(children) => out.extend(children),
                    RawNode::Leaf(_) => return Err("expected inner node".into()),
                }
            }
            Ok(out)
        }

        fn leaf_entries(&self, refs: &[&RawRef]) -> Result<LeafEntries, String> {
            let mut out = Vec::new();
            for r in refs {
                match self.read_node(&r.oid)? {
                    RawNode::Leaf(entries) => out.extend(entries),
                    RawNode::Inner(_) => return Err("expected leaf node".into()),
                }
            }
            Ok(out)
        }
    }

    /// Key-merge two sorted leaf-entry runs covering the same key span.
    fn merge_diff(
        a: Vec<(Vec<u8>, Vec<u8>)>,
        b: Vec<(Vec<u8>, Vec<u8>)>,
        out: &mut Vec<(Vec<u8>, ChangeKind)>,
    ) {
        let mut ai = a.into_iter().peekable();
        let mut bi = b.into_iter().peekable();
        loop {
            match (ai.peek(), bi.peek()) {
                (Some((ka, _)), Some((kb, _))) => match ka.cmp(kb) {
                    std::cmp::Ordering::Less => {
                        out.push((ai.next().expect("peeked").0, ChangeKind::Removed));
                    }
                    std::cmp::Ordering::Greater => {
                        out.push((bi.next().expect("peeked").0, ChangeKind::Added));
                    }
                    std::cmp::Ordering::Equal => {
                        let (ka, va) = ai.next().expect("peeked");
                        let (_, vb) = bi.next().expect("peeked");
                        if va != vb {
                            out.push((ka, ChangeKind::Modified));
                        }
                    }
                },
                (Some(_), None) => out.push((ai.next().expect("peeked").0, ChangeKind::Removed)),
                (None, Some(_)) => out.push((bi.next().expect("peeked").0, ChangeKind::Added)),
                (None, None) => break,
            }
        }
    }

    /// Diff two same-level ref runs covering the same overall key span.
    /// `level` is the level of the nodes the refs point at.
    fn diff_refs(
        rd: &Reader,
        level: u8,
        a: &[RawRef],
        b: &[RawRef],
        out: &mut Vec<(Vec<u8>, ChangeKind)>,
    ) -> Result<(), String> {
        let mut i = 0usize;
        let mut j = 0usize;
        while i < a.len() || j < b.len() {
            // Skip identical subtrees: content addressing makes OID equality
            // a proof of subtree equality, and adjacent versions share
            // unchanged chunks (history independence + splice reuse).
            if i < a.len() && j < b.len() && a[i].oid == b[j].oid {
                i += 1;
                j += 1;
                continue;
            }
            // Accumulate a mismatched region on each side until the two
            // sides' key spans re-align (equal trailing last_key) or both
            // are exhausted.
            let mut ra: Vec<&RawRef> = Vec::new();
            let mut rb: Vec<&RawRef> = Vec::new();
            loop {
                let a_span = ra.last().map(|r| r.last_key.as_slice());
                let b_span = rb.last().map(|r| r.last_key.as_slice());
                if let (Some(x), Some(y)) = (a_span, b_span)
                    && x == y
                {
                    break;
                }
                let extend_a = match (a_span, b_span) {
                    (None, _) => true,
                    (_, None) => false,
                    (Some(x), Some(y)) => x < y,
                };
                if extend_a && i < a.len() {
                    ra.push(&a[i]);
                    i += 1;
                } else if !extend_a && j < b.len() {
                    rb.push(&b[j]);
                    j += 1;
                } else if i < a.len() {
                    ra.push(&a[i]);
                    i += 1;
                } else if j < b.len() {
                    rb.push(&b[j]);
                    j += 1;
                } else {
                    break;
                }
            }
            if level == 0 {
                let ea = rd.leaf_entries(&ra)?;
                let eb = rd.leaf_entries(&rb)?;
                merge_diff(ea, eb, out);
            } else {
                let ca = rd.expand(&ra)?;
                let cb = rd.expand(&rb)?;
                diff_refs(rd, level - 1, &ca, &cb, out)?;
            }
        }
        Ok(())
    }

    /// Diff two versions by root: yields `(key, kind)` in key order.
    pub fn diff_roots(
        rd: &Reader,
        a: &super::Root,
        b: &super::Root,
    ) -> Result<Vec<(Vec<u8>, ChangeKind)>, String> {
        if a.oid == b.oid {
            return Ok(Vec::new());
        }
        let pseudo = |oid: ObjectId| RawRef {
            last_key: Vec::new(),
            oid,
        };
        // Levels of the nodes the current ref runs point at.
        let mut la = a.height - 1;
        let mut lb = b.height - 1;
        let mut ra = vec![pseudo(a.oid)];
        let mut rb = vec![pseudo(b.oid)];
        // If heights differ, expand the taller side until levels match.
        while la > lb {
            ra = rd.expand(&ra.iter().collect::<Vec<_>>())?;
            la -= 1;
        }
        while lb > la {
            rb = rd.expand(&rb.iter().collect::<Vec<_>>())?;
            lb -= 1;
        }
        let mut out = Vec::new();
        diff_refs(rd, la as u8, &ra, &rb, &mut out)?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Scenario context
// ---------------------------------------------------------------------------

const REF_V0: &str = "refs/bench/v0";
const REF_HEAD: &str = "refs/bench/head";

struct Ctx {
    n: u64,
    repo: PathBuf,
    samples: usize,
    windows: usize,
    window_size: usize,
    updates: usize,
    growth_commits: usize,
}

impl Ctx {
    fn open_store(&self) -> Store {
        Store::open(&self.repo).expect("open store (run bulk-load first)")
    }

    fn head(&self, store: &Store) -> Root {
        store
            .read_manifest(REF_HEAD)
            .expect("read refs/bench/head (run bulk-load first)")
    }
}

fn section(name: &str, ctx: &Ctx) {
    println!("\n## {name} (keys={})", ctx.n);
}

// ---------------------------------------------------------------------------
// Scenario 1: bulk load
// ---------------------------------------------------------------------------

fn bulk_load(ctx: &Ctx) {
    section("bulk-load", ctx);
    assert!(
        !ctx.repo.exists(),
        "repo {} already exists; use a fresh --dir or delete it",
        ctx.repo.display()
    );
    let store = Store::create(&ctx.repo).expect("create store");
    progress(&format!("bulk-loading {} keys...", ctx.n));
    let t0 = Instant::now();
    let root = store
        .bulk_load((0..ctx.n).map(|i| (key_for(i), value_for(i, 0))))
        .expect("bulk_load");
    let load = t0.elapsed();
    let chunks = store.chunks_written();

    let t1 = Instant::now();
    store
        .commit_root(&root, REF_V0, "bench: base version v0")
        .expect("commit v0");
    store
        .commit_root(&root, REF_HEAD, "bench: base version v0")
        .expect("commit head");
    let commit = t1.elapsed();

    println!("load_time: {}", fmt_dur(load));
    println!(
        "load_throughput: {:.0} keys/s",
        ctx.n as f64 / load.as_secs_f64()
    );
    println!("tree_height: {}", root.height);
    println!("chunks_written_load: {chunks}");
    println!(
        "chunks_written_commit_x2: {} (incl. 1 manifest blob per commit)",
        store.chunks_written() - chunks
    );
    println!("commit_time_x2: {}", fmt_dur(commit));
    println!("repo_size_loose: {}", fmt_kb(du_kb(&ctx.repo)));
    println!("count_objects: {}", count_objects(&ctx.repo));
}

// ---------------------------------------------------------------------------
// Scenario 2 (and 7's read half): point reads
// ---------------------------------------------------------------------------

fn point_read_on(ctx: &Ctx, store: &Store, root: &Root, label: &str) {
    let mut rng = Rng::new(0xbead + ctx.n);
    let keys: Vec<Vec<u8>> = (0..ctx.samples)
        .map(|_| key_for(rng.below(ctx.n)))
        .collect();
    // Warm-up: a handful of untimed reads so first-touch costs (mmap of
    // odb indexes etc.) do not distort the sample.
    for k in keys.iter().take(50) {
        store.get(root, k).expect("get");
    }
    let mut times = Vec::with_capacity(keys.len());
    let mut found = 0usize;
    for k in &keys {
        let t = Instant::now();
        let v = store.get(root, k).expect("get");
        times.push(t.elapsed());
        if v.is_some() {
            found += 1;
        }
    }
    let (mean, p50, p99, max) = dur_stats(times);
    println!(
        "{label}: samples={} found={found} mean={} p50={} p99={} max={}",
        keys.len(),
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
        fmt_dur(max)
    );
}

fn point_read(ctx: &Ctx) {
    section("point-read (loose)", ctx);
    let store = ctx.open_store();
    let root = store.read_manifest(REF_V0).expect("read v0");
    point_read_on(ctx, &store, &root, "point_read_loose");
}

// ---------------------------------------------------------------------------
// Scenario 3: range scans
// ---------------------------------------------------------------------------

fn scan(ctx: &Ctx) {
    section("scan", ctx);
    let store = ctx.open_store();
    let root = store.read_manifest(REF_V0).expect("read v0");

    progress("full scan...");
    let t0 = Instant::now();
    let mut count = 0u64;
    let mut bytes = 0u64;
    for item in store.range_scan(&root, ..).expect("scan") {
        let (k, v) = item.expect("scan item");
        count += 1;
        bytes += (k.len() + v.len()) as u64;
    }
    let full = t0.elapsed();
    assert_eq!(count, ctx.n, "full scan must see every key");
    println!(
        "full_scan: keys={count} time={} throughput={:.0} keys/s ({:.1} MiB/s payload)",
        fmt_dur(full),
        count as f64 / full.as_secs_f64(),
        bytes as f64 / full.as_secs_f64() / (1024.0 * 1024.0)
    );

    progress(&format!(
        "{} windows of {} keys...",
        ctx.windows, ctx.window_size
    ));
    let mut rng = Rng::new(0x5ca9 + ctx.n);
    let mut times = Vec::with_capacity(ctx.windows);
    for _ in 0..ctx.windows {
        let start = rng.below(ctx.n.saturating_sub(ctx.window_size as u64).max(1));
        let t = Instant::now();
        let got = store
            .range_scan(&root, key_for(start)..)
            .expect("scan")
            .take(ctx.window_size)
            .count();
        times.push(t.elapsed());
        assert_eq!(got, ctx.window_size);
    }
    let (mean, p50, p99, max) = dur_stats(times);
    println!(
        "window_scan: windows={} size={} mean={} p50={} p99={} max={}",
        ctx.windows,
        ctx.window_size,
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
        fmt_dur(max)
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: single-key updates
// ---------------------------------------------------------------------------

fn update(ctx: &Ctx) {
    section("update (single-key)", ctx);
    println!(
        "CAVEAT: apply_batch loads ALL internal nodes per batch; latency is \
         spike-only, chunk counts are the architecture-representative number."
    );
    let store = ctx.open_store();
    let mut root = ctx.head(&store);
    let mut rng = Rng::new(0x0bda7e + ctx.n);
    let mut times = Vec::with_capacity(ctx.updates);
    let mut chunk_counts = Vec::with_capacity(ctx.updates);
    for u in 0..ctx.updates {
        let i = rng.below(ctx.n);
        let op = BatchOp::Put(key_for(i), value_for(i, 1_000_000 + u as u64));
        let before = store.chunks_written();
        let t = Instant::now();
        root = store.apply_batch(&root, [op]).expect("apply_batch");
        times.push(t.elapsed());
        chunk_counts.push(store.chunks_written() - before);
    }
    let (mean, p50, p99, max) = dur_stats(times);
    println!(
        "update_latency (apply only, spike-only number): n={} mean={} p50={} p99={} max={}",
        ctx.updates,
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
        fmt_dur(max)
    );
    chunk_counts.sort();
    let sum: u64 = chunk_counts.iter().sum();
    println!(
        "update_chunks_written (write amplification): mean={:.1} p50={} max={} (no commit included)",
        sum as f64 / chunk_counts.len() as f64,
        chunk_counts[chunk_counts.len() / 2],
        chunk_counts.last().unwrap()
    );

    let before = store.chunks_written();
    let t = Instant::now();
    store
        .commit_root(&root, REF_HEAD, "bench: after single-key updates")
        .expect("commit");
    println!(
        "commit_after_updates: time={} chunks_written={} (incl. manifest blob; commit walks all internal nodes)",
        fmt_dur(t.elapsed()),
        store.chunks_written() - before
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: diff between adjacent versions
// ---------------------------------------------------------------------------

/// Build a 1%-churn batch with structural change: 90% value updates,
/// 5% inserts (new `...x` keys), 5% deletes (existing base keys).
fn churn_ops_structural(n: u64, ver: u64, rng: &mut Rng) -> Vec<BatchOp> {
    let total = (n / 100).max(1) as usize;
    let inserts = total / 20;
    let deletes = total / 20;
    let updates = total - inserts - deletes;
    let mut ops = Vec::with_capacity(total);
    for _ in 0..updates {
        let i = rng.below(n);
        ops.push(BatchOp::Put(key_for(i), value_for(i, ver)));
    }
    for _ in 0..inserts {
        let i = rng.below(n);
        ops.push(BatchOp::Put(insert_key_for(i), value_for(i, ver)));
    }
    for _ in 0..deletes {
        ops.push(BatchOp::Delete(key_for(rng.below(n))));
    }
    ops
}

/// Dedup ops the way `apply_batch` does (sorted, last wins), for verifying
/// diff output against intent.
fn dedup_ops(mut ops: Vec<BatchOp>) -> Vec<BatchOp> {
    ops.sort_by(|a, b| a.key().cmp(b.key()));
    let mut out: Vec<BatchOp> = Vec::with_capacity(ops.len());
    for op in ops {
        match out.last_mut() {
            Some(last) if last.key() == op.key() => *last = op,
            _ => out.push(op),
        }
    }
    out
}

fn diff(ctx: &Ctx) {
    section("diff (adjacent versions, 1% churn)", ctx);
    let store = ctx.open_store();
    let old_root = ctx.head(&store);

    let mut rng = Rng::new(0xd1ff + ctx.n);
    let ops = churn_ops_structural(ctx.n, 2_000_000, &mut rng);
    let deduped = dedup_ops(ops.clone());
    progress(&format!("applying {}-op churn batch...", ops.len()));
    let before = store.chunks_written();
    let t = Instant::now();
    let new_root = store.apply_batch(&old_root, ops).expect("apply_batch");
    let apply = t.elapsed();
    let batch_chunks = store.chunks_written() - before;
    store
        .commit_root(&new_root, REF_HEAD, "bench: 1% churn for diff")
        .expect("commit");
    println!(
        "churn_batch: ops={} apply_time={} chunks_written={batch_chunks}",
        deduped.len(),
        fmt_dur(apply)
    );

    // Expected changes, classified from the deduped ops. All delete/update
    // targets are base keys (present); `...x` insert keys are new. A put on
    // an `x` key duplicated in-batch stays one Added.
    let mut expected: Vec<(Vec<u8>, tdiff::ChangeKind)> = deduped
        .iter()
        .map(|op| match op {
            BatchOp::Delete(k) => (k.clone(), tdiff::ChangeKind::Removed),
            BatchOp::Put(k, _) if k.ends_with(b"x") => (k.clone(), tdiff::ChangeKind::Added),
            BatchOp::Put(k, _) => (k.clone(), tdiff::ChangeKind::Modified),
        })
        .collect();
    expected.sort();

    progress("diff walk...");
    let rd = tdiff::Reader::open(&ctx.repo).expect("open reader");
    let t = Instant::now();
    let changes = tdiff::diff_roots(&rd, &old_root, &new_root).expect("diff");
    let walk = t.elapsed();
    let reads = rd.reads.get();
    let (adds, dels, mods) = changes.iter().fold((0, 0, 0), |(a, d, m), (_, k)| match k {
        tdiff::ChangeKind::Added => (a + 1, d, m),
        tdiff::ChangeKind::Removed => (a, d + 1, m),
        tdiff::ChangeKind::Modified => (a, d, m + 1),
    });
    println!(
        "diff_walk: time={} changes={} (added={adds} removed={dels} modified={mods}) chunks_read={reads}",
        fmt_dur(walk),
        changes.len()
    );

    // Verify against intent (cheap: proportional to churn).
    let mut sorted_changes = changes.clone();
    sorted_changes.sort();
    assert_eq!(
        sorted_changes, expected,
        "diff walk must report exactly the churned keys"
    );
    println!("diff_verified_against_ops: ok");

    // Belt-and-braces at small scale: recompute the diff from two full
    // scans and compare.
    if ctx.n <= 200_000 {
        progress("verifying diff against full scans...");
        let scan_map = |root: &Root| -> Vec<(Vec<u8>, Vec<u8>)> {
            store
                .range_scan(root, ..)
                .expect("scan")
                .collect::<Result<_, _>>()
                .expect("scan items")
        };
        let a = scan_map(&old_root);
        let b = scan_map(&new_root);
        let mut naive: Vec<(Vec<u8>, tdiff::ChangeKind)> = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && (j >= b.len() || a[i].0 < b[j].0) {
                naive.push((a[i].0.clone(), tdiff::ChangeKind::Removed));
                i += 1;
            } else if j < b.len() && (i >= a.len() || b[j].0 < a[i].0) {
                naive.push((b[j].0.clone(), tdiff::ChangeKind::Added));
                j += 1;
            } else {
                if a[i].1 != b[j].1 {
                    naive.push((a[i].0.clone(), tdiff::ChangeKind::Modified));
                }
                i += 1;
                j += 1;
            }
        }
        assert_eq!(
            sorted_changes, naive,
            "diff walk must match scan-based diff"
        );
        println!("diff_verified_against_scan: ok");
    }
}

// ---------------------------------------------------------------------------
// Scenario 6: repo growth over simulated import commits
// ---------------------------------------------------------------------------

fn growth(ctx: &Ctx) {
    section("growth (import commits at 1% churn)", ctx);
    let store = ctx.open_store();
    let mut root = ctx.head(&store);
    let mut rng = Rng::new(0x94011 + ctx.n);
    let churn = (ctx.n / 100).max(1) as usize;
    println!(
        "commits={} churn_per_commit={churn} (all value updates on existing keys)",
        ctx.growth_commits
    );
    let mut apply_times = Vec::new();
    let mut commit_times = Vec::new();
    let mut chunk_counts = Vec::new();
    println!("size_trajectory (after N commits, raw loose):");
    for c in 0..ctx.growth_commits {
        let ver = 3_000_000 + c as u64;
        let ops: Vec<BatchOp> = (0..churn)
            .map(|_| {
                let i = rng.below(ctx.n);
                BatchOp::Put(key_for(i), value_for(i, ver))
            })
            .collect();
        let before = store.chunks_written();
        let t = Instant::now();
        root = store.apply_batch(&root, ops).expect("apply_batch");
        apply_times.push(t.elapsed());
        let t = Instant::now();
        store
            .commit_root(&root, REF_HEAD, &format!("bench: import commit {}", c + 1))
            .expect("commit");
        commit_times.push(t.elapsed());
        chunk_counts.push(store.chunks_written() - before);
        if (c + 1) % 10 == 0 {
            let kb = du_kb(&ctx.repo);
            println!("  after {:3} commits: {}", c + 1, fmt_kb(kb));
            progress(&format!(
                "growth: {}/{} commits, repo {}",
                c + 1,
                ctx.growth_commits,
                fmt_kb(kb)
            ));
        }
    }
    let (amean, ap50, ap99, _) = dur_stats(apply_times);
    let (cmean, cp50, cp99, _) = dur_stats(commit_times);
    let csum: u64 = chunk_counts.iter().sum();
    println!(
        "apply_per_commit (spike-only latency): mean={} p50={} p99={}",
        fmt_dur(amean),
        fmt_dur(ap50),
        fmt_dur(ap99)
    );
    println!(
        "commit_per_commit (walks all internal nodes): mean={} p50={} p99={}",
        fmt_dur(cmean),
        fmt_dur(cp50),
        fmt_dur(cp99)
    );
    println!(
        "chunks_written_per_commit: mean={:.0} (incl. manifest blob)",
        csum as f64 / chunk_counts.len() as f64
    );

    let raw = du_kb(&ctx.repo);
    println!("repo_size_raw_loose: {}", fmt_kb(raw));
    println!("count_objects_raw: {}", count_objects(&ctx.repo));
    progress("git gc --prune=now ...");
    let t = Instant::now();
    git(&ctx.repo, &["gc", "--prune=now", "--quiet"]);
    let gc = t.elapsed();
    let after_gc = du_kb(&ctx.repo);
    println!(
        "repo_size_after_gc: {} (gc took {})",
        fmt_kb(after_gc),
        fmt_dur(gc)
    );
    progress("git gc --aggressive --prune=now ...");
    let t = Instant::now();
    git(&ctx.repo, &["gc", "--aggressive", "--prune=now", "--quiet"]);
    let agg = t.elapsed();
    let after_agg = du_kb(&ctx.repo);
    println!(
        "repo_size_after_gc_aggressive: {} (took {})",
        fmt_kb(after_agg),
        fmt_dur(agg)
    );
    println!("count_objects_packed: {}", count_objects(&ctx.repo));
}

// ---------------------------------------------------------------------------
// Scenario 7: loose vs packed point reads
// ---------------------------------------------------------------------------

fn repack_read(ctx: &Ctx) {
    section("repack-read (packed)", ctx);
    // Idempotent if growth already packed the repo. All benchmark versions
    // are reachable from refs/bench/*, so pruning cannot drop them.
    progress("git gc --prune=now (idempotent) ...");
    git(&ctx.repo, &["gc", "--prune=now", "--quiet"]);
    println!("count_objects: {}", count_objects(&ctx.repo));
    println!("repo_size_packed: {}", fmt_kb(du_kb(&ctx.repo)));
    // Fresh store so reads go through the repacked ODB, same sample as the
    // loose point-read scenario (same seed, same v0 root).
    let store = ctx.open_store();
    let root = store.read_manifest(REF_V0).expect("read v0");
    point_read_on(ctx, &store, &root, "point_read_packed");
}

// ---------------------------------------------------------------------------
// Pack-on-write experiment (bead acetone-63m.10)
// ---------------------------------------------------------------------------

/// Run git returning success and combined output (the plain `git` helper
/// only eprints failures; correctness checks need the status).
fn git_check(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Feed a (possibly thin) pack to `git index-pack --stdin --fix-thin`,
/// which stores it in `objects/pack`, appending any external delta bases.
/// Returns the stored pack's name hash.
fn index_pack_fix_thin(repo: &Path, pack_bytes: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git index-pack");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(pack_bytes)
        .expect("pipe pack to index-pack");
    let out = child.wait_with_output().expect("wait for index-pack");
    assert!(
        out.status.success(),
        "index-pack --fix-thin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // stdout is `pack\t<hex>` (or `keep\t<hex>`).
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .expect("index-pack reports the pack hash")
        .to_string()
}

/// Newest pack index in `objects/pack`, for verify-pack.
fn newest_idx(repo: &Path) -> Option<PathBuf> {
    let dir = repo.join("objects/pack");
    let mut idxs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "idx"))
        .map(|e| {
            (
                e.metadata().and_then(|m| m.modified()).expect("mtime"),
                e.path(),
            )
        })
        .collect();
    idxs.sort();
    idxs.pop().map(|(_, p)| p)
}

mod pow {
    //! Building the per-commit pack entry list: everything `apply_batch` +
    //! `commit_root` wrote, each with its chosen delta base — chunks against
    //! their recorded predecessor, trees/manifest against the parent
    //! commit's counterpart.

    use std::collections::{HashMap, HashSet};

    use gix::ObjectId;
    use prolly_git_spike::WriteRecord;
    use prolly_git_spike::pack::PackEntry;

    pub fn obj_data(repo: &gix::Repository, oid: ObjectId) -> (gix::object::Kind, Vec<u8>) {
        let o = repo.find_object(oid).expect("object readable").detach();
        (o.kind, o.data)
    }

    /// Decode a tree object into (filename -> entry OID).
    fn tree_map(repo: &gix::Repository, oid: ObjectId) -> HashMap<Vec<u8>, ObjectId> {
        let (kind, data) = obj_data(repo, oid);
        assert_eq!(kind, gix::object::Kind::Tree, "expected tree {oid}");
        gix::objs::TreeRef::from_bytes(&data, gix::hash::Kind::Sha1)
            .expect("valid tree")
            .entries
            .iter()
            .map(|e| (e.filename.to_vec(), e.oid.to_owned()))
            .collect()
    }

    struct Builder<'r> {
        repo: &'r gix::Repository,
        seen: HashSet<ObjectId>,
        entries: Vec<PackEntry>,
    }

    impl Builder<'_> {
        fn push(&mut self, oid: ObjectId, base: Option<ObjectId>) {
            if !self.seen.insert(oid) {
                return;
            }
            let (kind, data) = obj_data(self.repo, oid);
            let base = base
                .filter(|b| *b != oid)
                .map(|b| (b, obj_data(self.repo, b).1));
            self.entries.push(PackEntry {
                oid,
                kind,
                data,
                base,
            });
        }
    }

    /// All objects created by one growth commit, with chosen delta bases.
    pub fn build_entries(
        repo: &gix::Repository,
        commit_oid: ObjectId,
        rec: &WriteRecord,
    ) -> Vec<PackEntry> {
        let mut b = Builder {
            repo,
            seen: HashSet::new(),
            entries: Vec::with_capacity(rec.written.len() + 300),
        };

        let commit = repo
            .find_object(commit_oid)
            .expect("commit")
            .try_into_commit()
            .expect("commit object");
        let tree_id = commit.tree_id().expect("tree id").detach();
        let parent_tree = commit.parent_ids().next().map(|p| {
            p.object()
                .expect("parent")
                .try_into_commit()
                .expect("parent commit")
                .tree_id()
                .expect("parent tree id")
                .detach()
        });

        b.push(commit_oid, None);
        if parent_tree != Some(tree_id) {
            b.push(tree_id, parent_tree);
        }
        let new_top = tree_map(repo, tree_id);
        let old_top = parent_tree.map(|t| tree_map(repo, t)).unwrap_or_default();

        // Manifest blob: delta against the parent's manifest.
        let manifest = new_top.get(b"manifest".as_slice()).copied();
        let old_manifest = old_top.get(b"manifest".as_slice()).copied();
        if let Some(m) = manifest
            && old_manifest != Some(m)
        {
            b.push(m, old_manifest);
        }

        // Reachability trees: `chunks/` and its shard subtrees, each delta
        // against the parent commit's same-named tree.
        let chunks = new_top.get(b"chunks".as_slice()).copied();
        let old_chunks = old_top.get(b"chunks".as_slice()).copied();
        if let Some(ct) = chunks
            && old_chunks != Some(ct)
        {
            b.push(ct, old_chunks);
            let new_shards = tree_map(repo, ct);
            let old_shards = old_chunks.map(|t| tree_map(repo, t)).unwrap_or_default();
            for (name, oid) in &new_shards {
                let old = old_shards.get(name).copied();
                if old != Some(*oid) {
                    b.push(*oid, old);
                }
            }
        }

        // Chunk blobs, each against its recorded predecessor.
        for oid in &rec.written {
            b.push(*oid, rec.bases.get(oid).copied());
        }
        b.entries
    }
}

/// Scenario pack-growth: the growth scenario (same op stream, same seeds)
/// re-run against a fresh repo, writing one hand-rolled pack per commit in
/// which every new chunk is a REF_DELTA against its predecessor. Measures
/// retained bytes per commit, what `git repack -a -d` then does to the
/// hand-chosen deltas, and verifies correctness (fsck, verify-pack, clone,
/// full read-back).
fn pack_growth(ctx: &Ctx) {
    section(
        "pack-growth (pack-on-write import commits at 1% churn)",
        ctx,
    );
    let repo = ctx.repo.with_file_name(format!("repo-{}-pack", ctx.n));
    assert!(
        !repo.exists(),
        "repo {} already exists; use a fresh --dir or delete it",
        repo.display()
    );
    let store = Store::create(&repo).expect("create store");
    progress(&format!("bulk-loading {} keys...", ctx.n));
    let root = store
        .bulk_load((0..ctx.n).map(|i| (key_for(i), value_for(i, 0))))
        .expect("bulk_load");
    store
        .commit_root(&root, REF_V0, "bench: base version v0")
        .expect("commit v0");
    store
        .commit_root(&root, REF_HEAD, "bench: base version v0")
        .expect("commit head");
    progress("packing the base version (git repack -a -d)...");
    git(&repo, &["repack", "-a", "-d", "--quiet"]);
    let base_kb = du_kb(&repo);
    println!("base_size_packed: {}", fmt_kb(base_kb));
    // Pack creation order, for the mtime-inversion repack variant below.
    let mut pack_order: Vec<String> = vec![
        newest_idx(&repo)
            .expect("base pack")
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("pack-"))
            .expect("pack-<hex>.idx name")
            .to_string(),
    ];

    let gxr = gix::open(&repo).expect("open gix repo");
    let churn = (ctx.n / 100).max(1) as usize;
    let mut vers: Vec<u64> = vec![0; ctx.n as usize];
    let mut rng = Rng::new(0x94011 + ctx.n);
    let mut root = root;
    println!(
        "commits={} churn_per_commit={churn} (op stream identical to the growth scenario)",
        ctx.growth_commits
    );
    let mut apply_times = Vec::new();
    let mut pack_times = Vec::new();
    let mut thin_bytes = Vec::new();
    let mut n_deltas = 0u64;
    let mut n_whole = 0u64;
    let mut delta_payload = 0u64;
    // (oid, chosen base) of every growth object in creation order, for the
    // native consolidation pack below.
    let mut growth_meta: Vec<(gix::ObjectId, Option<gix::ObjectId>)> = Vec::new();
    println!("size_trajectory (after N commits, packs incl. --fix-thin bases):");
    for c in 0..ctx.growth_commits {
        let ver = 3_000_000 + c as u64;
        let ops: Vec<BatchOp> = (0..churn)
            .map(|_| {
                let i = rng.below(ctx.n);
                vers[i as usize] = ver;
                BatchOp::Put(key_for(i), value_for(i, ver))
            })
            .collect();
        store.start_recording();
        let t = Instant::now();
        root = store.apply_batch(&root, ops).expect("apply_batch");
        let commit_oid = store
            .commit_root(&root, REF_HEAD, &format!("bench: import commit {}", c + 1))
            .expect("commit");
        apply_times.push(t.elapsed());
        let rec = store.take_recording().expect("recording enabled");

        let t = Instant::now();
        let entries = pow::build_entries(&gxr, commit_oid, &rec);
        growth_meta.extend(
            entries
                .iter()
                .map(|e| (e.oid, e.base.as_ref().map(|(b, _)| *b))),
        );
        let pf = pack::write_pack(entries.len(), entries).expect("write pack");
        pack_order.push(index_pack_fix_thin(&repo, &pf.bytes));
        git(&repo, &["prune-packed", "--quiet"]);
        pack_times.push(t.elapsed());
        thin_bytes.push(pf.bytes.len() as u64);
        n_deltas += pf.deltas as u64;
        n_whole += pf.whole as u64;
        delta_payload += pf.delta_bytes;

        if (c + 1) % 10 == 0 {
            let kb = du_kb(&repo);
            println!("  after {:3} commits: {}", c + 1, fmt_kb(kb));
            progress(&format!(
                "pack-growth: {}/{} commits, repo {}",
                c + 1,
                ctx.growth_commits,
                fmt_kb(kb)
            ));
        }
    }
    let commits = ctx.growth_commits as u64;
    let (amean, ap50, ap99, _) = dur_stats(apply_times);
    let (pmean, pp50, pp99, _) = dur_stats(pack_times);
    let thin_total: u64 = thin_bytes.iter().sum();
    println!(
        "apply_and_commit_per_commit (spike-only latency): mean={} p50={} p99={}",
        fmt_dur(amean),
        fmt_dur(ap50),
        fmt_dur(ap99)
    );
    println!(
        "pack_write_index_prune_per_commit: mean={} p50={} p99={}",
        fmt_dur(pmean),
        fmt_dur(pp50),
        fmt_dur(pp99)
    );
    println!(
        "thin_pack_bytes_per_commit (before --fix-thin base completion): mean={} total={}",
        fmt_kb(thin_total / commits / 1024),
        fmt_kb(thin_total / 1024)
    );
    println!(
        "pack_entries_per_commit: deltas={:.0} whole={:.0} raw_delta_payload={} mean/delta={:.0} bytes",
        n_deltas as f64 / commits as f64,
        n_whole as f64 / commits as f64,
        fmt_kb(delta_payload / 1024),
        delta_payload as f64 / n_deltas.max(1) as f64
    );

    let raw = du_kb(&repo);
    println!(
        "repo_size_pack_on_write: {} ({} retained/commit)",
        fmt_kb(raw),
        fmt_kb(raw.saturating_sub(base_kb) / commits)
    );
    println!("count_objects: {}", count_objects(&repo));

    // Correctness of the hand-written (thin-completed) packs.
    let idx = newest_idx(&repo).expect("a pack exists");
    let (ok, text) = git_check(&repo, &["verify-pack", "-s", idx.to_str().expect("utf8")]);
    assert!(ok, "verify-pack on hand-written pack failed: {text}");
    println!("verify_pack_last_commit_pack: ok\n{}", text.trim_end());
    progress("git fsck --strict ...");
    let (ok, text) = git_check(&repo, &["fsck", "--strict", "--no-progress"]);
    assert!(ok, "fsck failed: {text}");
    println!("fsck_before_repack: clean");

    // What does a stock repack do to the hand-chosen deltas? Run it on a
    // copy: for objects present in several packs (every --fix-thin base
    // duplicate), pack-objects keeps whichever representation it happens to
    // find first, so a fraction of the hand-chosen deltas is replaced by the
    // whole duplicates.
    let stock = ctx
        .repo
        .with_file_name(format!("repo-{}-pack-stock", ctx.n));
    let out = Command::new("cp")
        .arg("-Rp")
        .arg(&repo)
        .arg(&stock)
        .output()
        .expect("cp");
    assert!(out.status.success(), "cp -Rp failed");
    progress("git repack -a -d (stock, on a copy) ...");
    let t = Instant::now();
    git(&stock, &["repack", "-a", "-d", "--quiet"]);
    let repack = t.elapsed();
    let packed = du_kb(&stock);
    println!(
        "repo_size_after_stock_repack: {} ({} retained/commit, repack took {})",
        fmt_kb(packed),
        fmt_kb(packed.saturating_sub(base_kb) / commits),
        fmt_dur(repack)
    );
    let idx = newest_idx(&stock).expect("repacked pack exists");
    let (ok, text) = git_check(&stock, &["verify-pack", "-s", idx.to_str().expect("utf8")]);
    assert!(ok, "verify-pack after stock repack failed: {text}");
    println!("verify_pack_after_stock_repack:\n{}", text.trim_end());
    let (ok, text) = git_check(&stock, &["fsck", "--strict", "--no-progress"]);
    assert!(ok, "fsck after stock repack failed: {text}");
    println!("fsck_after_stock_repack: clean");

    // Native consolidation on the original repo: one cumulative pack holding
    // every growth object exactly once with its chosen delta representation
    // (what a production acetone-store repack would write), replacing the
    // 100 per-commit packs. The base pack is kept; --fix-thin re-appends
    // only the first-generation bases it references.
    progress("native consolidation pack ...");
    let t = Instant::now();
    let mut base_of: std::collections::HashMap<gix::ObjectId, Option<gix::ObjectId>> =
        std::collections::HashMap::new();
    let mut order: Vec<gix::ObjectId> = Vec::new();
    for (oid, base) in &growth_meta {
        if base_of.contains_key(oid) {
            continue; // first occurrence wins
        }
        // Guard against REF_DELTA cycles inside one pack (possible only if
        // content re-appears across commits); walk the chain so far.
        let mut chosen = *base;
        let mut cur = chosen;
        while let Some(b) = cur {
            if b == *oid {
                chosen = None;
                break;
            }
            cur = base_of.get(&b).copied().flatten();
        }
        base_of.insert(*oid, chosen);
        order.push(*oid);
    }
    let pf = pack::write_pack(
        order.len(),
        order.iter().map(|oid| {
            let (kind, data) = pow::obj_data(&gxr, *oid);
            let base = base_of[oid].map(|b| (b, pow::obj_data(&gxr, b).1));
            PackEntry {
                oid: *oid,
                kind,
                data,
                base,
            }
        }),
    )
    .expect("write consolidated pack");
    let consolidated = index_pack_fix_thin(&repo, &pf.bytes);
    for stale in &pack_order[1..] {
        if *stale == consolidated {
            continue;
        }
        for ext in ["pack", "idx"] {
            let path = repo.join(format!("objects/pack/pack-{stale}.{ext}"));
            std::fs::remove_file(&path)
                .unwrap_or_else(|e| panic!("remove {}: {e}", path.display()));
        }
        // Reverse indexes are written by default since git 2.41; absence ok.
        let _ = std::fs::remove_file(repo.join(format!("objects/pack/pack-{stale}.rev")));
    }
    println!(
        "native_consolidation: objects={} deltas={} whole={} thin_pack={} took {}",
        order.len(),
        pf.deltas,
        pf.whole,
        fmt_kb(pf.bytes.len() as u64 / 1024),
        fmt_dur(t.elapsed())
    );
    let packed = du_kb(&repo);
    println!(
        "repo_size_after_native_consolidation: {} ({} retained/commit)",
        fmt_kb(packed),
        fmt_kb(packed.saturating_sub(base_kb) / commits),
    );
    println!(
        "count_objects_after_native_consolidation: {}",
        count_objects(&repo)
    );
    let idx = newest_idx(&repo).expect("consolidated pack exists");
    let (ok, text) = git_check(&repo, &["verify-pack", "-s", idx.to_str().expect("utf8")]);
    assert!(ok, "verify-pack after consolidation failed: {text}");
    println!(
        "verify_pack_after_native_consolidation:\n{}",
        text.trim_end()
    );
    let (ok, text) = git_check(&repo, &["fsck", "--strict", "--no-progress"]);
    assert!(ok, "fsck after consolidation failed: {text}");
    println!("fsck_after_native_consolidation: clean");

    // Clone over the real object walk (no hardlink shortcut), then read
    // every key back and compare against the expected final state.
    let clone = ctx
        .repo
        .with_file_name(format!("repo-{}-pack-clone", ctx.n));
    progress("git clone --mirror --no-local ...");
    let out = Command::new("git")
        .arg("clone")
        .arg("--mirror")
        .arg("--no-local")
        .arg("--quiet")
        .arg(&repo)
        .arg(&clone)
        .output()
        .expect("git clone");
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!("clone_size: {}", fmt_kb(du_kb(&clone)));
    progress("verifying full read-back from the clone...");
    let cstore = Store::open(&clone).expect("open clone");
    let croot = cstore.read_manifest(REF_HEAD).expect("clone head manifest");
    let mut count = 0u64;
    for item in cstore.range_scan(&croot, ..).expect("scan clone") {
        let (k, v) = item.expect("scan item");
        assert_eq!(k, key_for(count), "key {count} in clone scan");
        assert_eq!(
            v,
            value_for(count, vers[count as usize]),
            "value of key {count} in clone scan"
        );
        count += 1;
    }
    assert_eq!(count, ctx.n, "clone must contain every key");
    println!("clone_read_back: verified {count} keys against expected state");
}

/// Scenario pack-probe: git's tolerance of an ON-DISK pack whose REF_DELTA
/// base lives outside the pack (a "thin" pack indexed as-is with a
/// hand-written .idx, no --fix-thin completion). If tolerated, production
/// pack-on-write could skip base duplication entirely.
fn pack_probe(ctx: &Ctx) {
    section("pack-probe (external REF_DELTA base, no --fix-thin)", ctx);
    let repo = ctx.repo.with_file_name(format!("repo-{}-probe", ctx.n));
    assert!(
        !repo.exists(),
        "repo {} already exists; use a fresh --dir or delete it",
        repo.display()
    );
    let gxr = gix::init_bare(&repo).expect("init probe repo");

    // ASCII contents so lossy stdout capture is faithful.
    let base: Vec<u8> = (0..12).flat_map(|i| value_for(i, 0)).collect();
    let mut target = base.clone();
    let edit = value_for(99, 7);
    target[400..400 + edit.len()].copy_from_slice(&edit);
    let base_oid = gxr.write_blob(&base).expect("write base").detach();
    let target_oid =
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::object::Kind::Blob, &target)
            .expect("hash");

    let pf = pack::write_pack(
        1,
        [PackEntry {
            oid: target_oid,
            kind: gix::object::Kind::Blob,
            data: target.clone(),
            base: Some((base_oid, base.clone())),
        }],
    )
    .expect("write pack");
    assert_eq!(pf.deltas, 1, "the probe entry must be stored as a delta");
    let idx = pack::write_idx(&pf).expect("write idx");
    let pack_dir = repo.join("objects/pack");
    let stem = format!("pack-{}", pf.trailer);
    std::fs::write(pack_dir.join(format!("{stem}.pack")), &pf.bytes).expect("store pack");
    std::fs::write(pack_dir.join(format!("{stem}.idx")), &idx).expect("store idx");

    let (ok, text) = git_check(&repo, &["cat-file", "blob", &target_oid.to_string()]);
    println!(
        "git_cat_file_external_base: status={} content_matches={} stderr_or_content_head={:?}",
        if ok { "ok" } else { "FAILED" },
        text.as_bytes() == target.as_slice(),
        text.chars().take(80).collect::<String>()
    );
    let (ok, text) = git_check(&repo, &["fsck", "--strict", "--no-progress"]);
    println!(
        "git_fsck: status={} output_head={:?}",
        if ok { "ok" } else { "FAILED" },
        text.lines().take(3).collect::<Vec<_>>()
    );
    let (ok, text) = git_check(
        &repo,
        &[
            "verify-pack",
            "-v",
            pack_dir.join(format!("{stem}.idx")).to_str().expect("utf8"),
        ],
    );
    println!(
        "git_verify_pack: status={} output_tail={:?}",
        if ok { "ok" } else { "FAILED" },
        text.lines().rev().take(3).collect::<Vec<_>>()
    );
    match gix::open(&repo)
        .expect("reopen probe repo")
        .find_object(target_oid)
    {
        Ok(o) => println!(
            "gix_find_object: ok content_matches={}",
            o.data == target.as_slice()
        ),
        Err(e) => println!("gix_find_object: FAILED ({e})"),
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn parse_keys(s: &str) -> u64 {
    match s.to_ascii_lowercase().as_str() {
        "100k" => 100_000,
        "1m" => 1_000_000,
        "5m" => 5_000_000,
        other => other
            .parse()
            .expect("--keys must be 100k|1m|5m or a number"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut keys: Option<u64> = None;
    let mut dir: Option<PathBuf> = None;
    let mut scenarios: Option<Vec<String>> = None;
    let mut samples = 1000usize;
    let mut windows = 100usize;
    let mut window_size = 1000usize;
    let mut updates = 100usize;
    let mut growth_commits = 100usize;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut val = || it.next().expect("missing value for flag").clone();
        match arg.as_str() {
            "--keys" => keys = Some(parse_keys(&val())),
            "--dir" => dir = Some(PathBuf::from(val())),
            "--scenarios" => {
                scenarios = Some(val().split(',').map(|s| s.trim().to_string()).collect())
            }
            "--samples" => samples = val().parse().expect("--samples"),
            "--windows" => windows = val().parse().expect("--windows"),
            "--window-size" => window_size = val().parse().expect("--window-size"),
            "--updates" => updates = val().parse().expect("--updates"),
            "--growth-commits" => growth_commits = val().parse().expect("--growth-commits"),
            other => panic!("unknown argument {other}"),
        }
    }
    let n = keys.expect("--keys is required");
    let dir = dir.expect("--dir is required");
    std::fs::create_dir_all(&dir).expect("create --dir");
    let ctx = Ctx {
        n,
        repo: dir.join(format!("repo-{n}")),
        samples,
        windows,
        window_size,
        updates,
        growth_commits,
    };

    let default: Vec<String> = ["bulk-load", "point-read", "scan", "update", "diff"]
        .iter()
        .map(|s| s.to_string())
        // The roadmap pins the growth scenario to the 1M-key graph; skip it
        // by default at 5M (pass --scenarios growth to force).
        .chain((n <= 2_000_000).then(|| "growth".to_string()))
        .chain(std::iter::once("repack-read".to_string()))
        .collect();
    let scenarios = scenarios.unwrap_or(default);

    println!(
        "# prolly-git-spike bench: keys={n} repo={}",
        ctx.repo.display()
    );
    println!("scenarios: {}", scenarios.join(", "));
    let t0 = Instant::now();
    for s in &scenarios {
        let t = Instant::now();
        match s.as_str() {
            "bulk-load" => bulk_load(&ctx),
            "point-read" => point_read(&ctx),
            "scan" => scan(&ctx),
            "update" => update(&ctx),
            "diff" => diff(&ctx),
            "growth" => growth(&ctx),
            "repack-read" => repack_read(&ctx),
            "pack-growth" => pack_growth(&ctx),
            "pack-probe" => pack_probe(&ctx),
            other => panic!("unknown scenario {other}"),
        }
        progress(&format!("scenario {s} done in {}", fmt_dur(t.elapsed())));
    }
    progress(&format!("all scenarios done in {}", fmt_dur(t0.elapsed())));
}
