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

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
            other => panic!("unknown scenario {other}"),
        }
        progress(&format!("scenario {s} done in {}", fmt_dur(t.elapsed())));
    }
    progress(&format!("all scenarios done in {}", fmt_dur(t0.elapsed())));
}
