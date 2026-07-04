//! Phase 0 regression benchmark suite, ported to the production crates
//! (bead acetone-63m.9).
//!
//! The roadmap keeps the Phase 0 spike suite alive "as regression
//! benchmarks" (docs/acetone-03-roadmap.md). The spike itself
//! (`spikes/prolly-git-spike/src/bin/bench.rs`, bead acetone-28x.4) is
//! workspace-excluded throwaway code; this crate re-runs the same scenarios
//! against the real [`acetone_store::GitStore`] and [`acetone_prolly`], so
//! the numbers stay comparable to `docs/notes/phase0-benchmarks.md` while
//! guarding the production code against regressions.
//!
//! # What it measures
//!
//! The representative asset-registry envelope from the roadmap: bulk load,
//! point read, range scan, single-key update (write amplification), diff
//! between adjacent versions, repo growth over simulated import commits, and
//! loose-vs-packed read performance.
//!
//! # What is asserted vs printed
//!
//! Wall-clock throughput and latency are machine-dependent, so they are
//! **printed** for manual runs but never asserted (that would make CI flaky).
//! The **asserted** regressions are structural and machine-independent, and
//! double as invariant guards:
//!
//! - a full scan visits exactly every key, and window scans return their
//!   exact size;
//! - `diff` reports exactly the churned key set — and, at small scales, the
//!   same set a naive two-scan diff computes;
//! - **history independence** (Load-Bearing Invariant 1): a tree reached by
//!   [`apply_batch`](acetone_prolly::apply_batch) has the same root hash as a
//!   fresh [`bulk_load`](acetone_prolly::bulk_load) of the resulting
//!   contents;
//! - single-key update write amplification stays within the root→leaf spine
//!   (`<= height + 2` chunks), the property that makes Option A viable.
//!
//! # Fidelity note
//!
//! Two caveats the spike carried are **gone** in the production crates and so
//! do not appear here: the real `apply_batch` loads only the root→leaf path
//! (single-key update latency is now architecture-representative, not
//! spike-only), and a real streaming [`diff`](acetone_prolly::diff) exists,
//! so the diff scenario exercises production code rather than a harness-side
//! walk. The chunk-write counter here counts prolly `put` calls only — pure
//! tree write amplification — excluding the manifest/reachability-tree
//! objects that commits write straight through git, which the spike's
//! `chunks_written()` folded in (+1/commit).
//!
//! # Out of scope: pack-on-write
//!
//! The spike's `pack-growth`/`pack-probe` scenarios are **not** ported. They
//! depend on a hand-rolled delta encoder, pack and index writers, and
//! predecessor tracking threaded out of the prolly splice — none of which
//! exists in the production crates (`apply_batch` does not expose the new
//! chunk → predecessor mapping, and `acetone-store` has no delta/pack
//! plumbing). The hypothesis is already validated in the spike and
//! `docs/notes/pack-on-write-validation.md`; the production port is tracked
//! by bead acetone-63m.13, and the pack-on-write regression benchmark must
//! follow it.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use acetone_prolly::{
    BatchOp, ChunkParams, Root, apply_batch, bulk_load, diff, get, reachable_chunks, scan,
};
use acetone_store::{
    Bytes, ChunkStore, CommitStore, GitStore, Hash, NewCommit, RefStore, StoreError,
};

/// Fallible result type for the harness. `acetone-bench` is a benchmark
/// binary, not a library crate, so it uses a boxed error rather than pulling
/// in `anyhow` (which CLAUDE.md reserves for `acetone-cli`).
pub type BenchResult<T> = Result<T, Box<dyn std::error::Error>>;

const REF_V0: &str = "refs/bench/v0";
const REF_HEAD: &str = "refs/bench/head";

// ---------------------------------------------------------------------------
// Deterministic data generation (verbatim from the spike, so the same seeds
// and op streams produce the same trees and numbers as
// docs/notes/phase0-benchmarks.md).
// ---------------------------------------------------------------------------

/// splitmix64: tiny deterministic RNG so runs are exactly reproducible
/// without a `rand` dependency.
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
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

/// Key for an asset inserted after bulk load (diff scenario): the `x` suffix
/// sorts it between neighbouring base keys, spreading inserts across the whole
/// keyspace rather than appending at the end.
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
/// Distinct `ver` values guarantee a distinct record for the same key.
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

/// Build a 1%-churn batch with structural change: 90% value updates, 5%
/// inserts (new `...x` keys), 5% deletes (existing base keys).
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

// ---------------------------------------------------------------------------
// Counting chunk store: wraps the real GitStore and counts prolly `put`
// calls, giving the write-amplification number (the spike's
// `chunks_written()`), while every other operation delegates unchanged.
// ---------------------------------------------------------------------------

/// A [`GitStore`] that counts [`ChunkStore::put`] calls, so tree write
/// amplification is observable. All reads, refs and commits delegate to the
/// inner store; only writes made through the prolly layer are counted.
pub struct CountingStore {
    inner: GitStore,
    puts: Cell<u64>,
}

impl CountingStore {
    /// Create a fresh bare repository and open it, counting from zero.
    pub fn create(path: &Path) -> Result<Self, StoreError> {
        Ok(CountingStore {
            inner: GitStore::create(path)?,
            puts: Cell::new(0),
        })
    }

    /// Open an existing repository.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(CountingStore {
            inner: GitStore::open(path)?,
            puts: Cell::new(0),
        })
    }

    /// The underlying store, for ref and commit operations.
    pub fn inner(&self) -> &GitStore {
        &self.inner
    }

    /// Chunks written (prolly `put` calls) since the last [`Self::reset`].
    pub fn puts(&self) -> u64 {
        self.puts.get()
    }

    /// Reset the write counter to zero.
    pub fn reset(&self) {
        self.puts.set(0);
    }
}

impl ChunkStore for CountingStore {
    fn put(&self, data: &[u8]) -> Result<Hash, StoreError> {
        self.puts.set(self.puts.get() + 1);
        self.inner.put(data)
    }

    fn put_batch(&self, chunks: &[&[u8]]) -> Result<Vec<Hash>, StoreError> {
        self.puts.set(self.puts.get() + chunks.len() as u64);
        self.inner.put_batch(chunks)
    }

    fn get(&self, hash: &Hash) -> Result<Option<Bytes>, StoreError> {
        self.inner.get(hash)
    }

    fn max_chunk_size(&self) -> u64 {
        self.inner.max_chunk_size()
    }
}

// ---------------------------------------------------------------------------
// A minimal manifest: enough to reconstruct a Root from a committed ref. The
// real manifest format (bead acetone-63m.4) is not needed here — the store
// treats manifest bytes as opaque — so the bench uses its own trivial text
// encoding to exercise the commit/read-back path honestly.
// ---------------------------------------------------------------------------

fn encode_manifest(root: &Root) -> Vec<u8> {
    let p = root.params();
    format!(
        "acetone-bench-manifest\nroot={}\nheight={}\nmin={}\nmask={}\nmax={}\n",
        root.hash().to_hex(),
        root.height(),
        p.min_bytes(),
        p.mask_bits(),
        p.max_bytes()
    )
    .into_bytes()
}

fn decode_manifest(bytes: &[u8]) -> BenchResult<Root> {
    let text = std::str::from_utf8(bytes)?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            fields.insert(k, v);
        }
    }
    let field = |k: &str| -> BenchResult<&str> {
        fields
            .get(k)
            .copied()
            .ok_or_else(|| format!("manifest missing field {k}").into())
    };
    let hash = Hash::from_hex(field("root")?)?;
    let height: u32 = field("height")?.parse()?;
    let params = ChunkParams::new(
        field("min")?.parse()?,
        field("mask")?.parse()?,
        field("max")?.parse()?,
    )?;
    Ok(Root::new(hash, height, params)?)
}

// ---------------------------------------------------------------------------
// Measurement helpers.
// ---------------------------------------------------------------------------

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// (mean, p50, p99, max) of a duration sample.
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

/// `du -sk` of a directory, in KiB. Best-effort: 0 if `du` is unavailable
/// (the sizing scenarios are informational, never asserted).
fn du_kb(path: &Path) -> u64 {
    Command::new("du")
        .arg("-sk")
        .arg(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
        })
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

/// Run a git subcommand in `repo`, returning `(success, combined output)`.
/// Used only for `gc`/`count-objects` sizing, never on the data path.
fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    match Command::new("git").arg("-C").arg(repo).args(args).output() {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Configuration, report and the scenario set.
// ---------------------------------------------------------------------------

/// The list of scenarios, in canonical run order. `bulk-load` always runs
/// first (it creates the repo the others read).
pub const ALL_SCENARIOS: &[&str] = &[
    "bulk-load",
    "point-read",
    "scan",
    "update",
    "diff",
    "growth",
    "repack-read",
];

/// Benchmark configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Number of base keys in the map.
    pub n: u64,
    /// Directory for the benchmark repository (created fresh).
    pub repo: PathBuf,
    /// Point-read sample count.
    pub samples: usize,
    /// Number of random range-scan windows.
    pub windows: usize,
    /// Keys per range-scan window.
    pub window_size: usize,
    /// Single-key updates to time.
    pub updates: usize,
    /// Simulated import commits in the growth scenario.
    pub growth_commits: usize,
    /// Whether to run the O(n) cross-checks (naive scan diff, history
    /// independence). Cheap only at small `n`; on by default under
    /// [`Config::smoke`].
    pub verify: bool,
    /// Suppress progress lines on stderr.
    pub quiet: bool,
}

impl Config {
    /// A tiny, fast configuration for CI: exercises every scenario and every
    /// assertion in ~1-2 s against a temp repo.
    pub fn smoke(repo: PathBuf) -> Self {
        Config {
            n: 2_000,
            repo,
            samples: 200,
            windows: 20,
            window_size: 100,
            updates: 50,
            growth_commits: 5,
            verify: true,
            quiet: true,
        }
    }
}

/// Structural, machine-independent results the smoke test asserts on. Timing
/// figures are printed, not returned.
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// Tree height after bulk load.
    pub height: u32,
    /// Chunks written by the bulk load.
    pub load_chunks: u64,
    /// Keys seen by the full forward scan (must equal `n`).
    pub scan_count: u64,
    /// Maximum single-key-update write amplification observed.
    pub update_amp_max: u64,
    /// Number of changed keys `diff` reported.
    pub diff_changes: usize,
    /// Whether `diff` matched the intended churn set.
    pub diff_matches_expected: Option<bool>,
    /// Whether `diff` matched a naive two-scan diff (small `n` only).
    pub diff_matches_scan: Option<bool>,
    /// Whether `apply_batch`'s root hash equalled `bulk_load` of the same
    /// contents (Load-Bearing Invariant 1; small `n` only).
    pub history_independent: Option<bool>,
    /// Commits applied by the growth scenario.
    pub growth_commits: usize,
    /// Keys read back from the head after growth (must equal current key
    /// count).
    pub growth_readback: Option<u64>,
    /// Point-read samples that resolved to a value after a repack.
    pub packed_reads_found: Option<usize>,
}

/// Commit `root` under `refname` (compare-and-swap), anchoring its full chunk
/// set, and return the new commit id.
fn commit_root(
    store: &CountingStore,
    root: &Root,
    refname: &str,
    message: &str,
    parent: Option<Hash>,
) -> BenchResult<Hash> {
    let manifest = encode_manifest(root);
    let anchors = reachable_chunks(store, root)?;
    let parents: Vec<Hash> = parent.into_iter().collect();
    // NewCommit is #[non_exhaustive]: build with `new` (which defaults the
    // author and empty trailers) and assign the rest.
    let mut commit = NewCommit::new(&manifest, "acetone benchmark version", message);
    commit.parents = &parents;
    commit.anchors = &anchors;
    let id = store.inner().create_commit(&commit)?;
    let expected = store.inner().read_ref(refname)?;
    store.inner().write_ref(refname, expected.as_ref(), &id)?;
    Ok(id)
}

/// The live benchmark: a store plus the versions committed so far.
struct Bench {
    cfg: Config,
    store: CountingStore,
    params: ChunkParams,
    v0: Root,
    head: Root,
    head_commit: Hash,
}

fn diff_kind(before_present: bool, after_present: bool) -> Option<&'static str> {
    match (before_present, after_present) {
        (true, true) => Some("modified"),
        (false, true) => Some("added"),
        (true, false) => Some("removed"),
        (false, false) => None,
    }
}

impl Bench {
    fn progress(&self, msg: &str) {
        if !self.cfg.quiet {
            eprintln!("[bench] {msg}");
        }
    }

    // -- Scenario 1: bulk load ------------------------------------------------

    fn create(cfg: Config) -> BenchResult<(Self, Report)> {
        let mut report = Report::default();
        if cfg.repo.exists() {
            return Err(format!(
                "repo {} already exists; use a fresh dir",
                cfg.repo.display()
            )
            .into());
        }
        let store = CountingStore::create(&cfg.repo)?;
        let params = ChunkParams::default();
        if !cfg.quiet {
            eprintln!("[bench] bulk-loading {} keys...", cfg.n);
        }
        println!("\n## bulk-load (keys={})", cfg.n);
        store.reset();
        let t0 = Instant::now();
        let root = bulk_load(
            &store,
            params,
            (0..cfg.n).map(|i| (key_for(i), value_for(i, 0))),
        )?;
        let load = t0.elapsed();
        let load_chunks = store.puts();

        report.height = root.height();
        report.load_chunks = load_chunks;
        println!("load_time: {}", fmt_dur(load));
        if load.as_secs_f64() > 0.0 {
            println!(
                "load_throughput: {:.0} keys/s",
                cfg.n as f64 / load.as_secs_f64()
            );
        }
        println!("tree_height: {}", root.height());
        println!("chunks_written_load: {load_chunks}");

        let t1 = Instant::now();
        let c0 = commit_root(&store, &root, REF_V0, "bench: base version v0", None)?;
        commit_root(&store, &root, REF_HEAD, "bench: base version v0", None)?;
        println!("commit_time_x2: {}", fmt_dur(t1.elapsed()));
        println!("repo_size_loose: {}", fmt_kb(du_kb(&cfg.repo)));

        let bench = Bench {
            cfg,
            store,
            params,
            v0: root.clone(),
            head: root,
            head_commit: c0,
        };
        Ok((bench, report))
    }

    // -- Scenario 2 (+ 7's read half): point reads ---------------------------

    fn point_read_on(&self, root: &Root, label: &str) -> BenchResult<usize> {
        let mut rng = Rng::new(0xbead + self.cfg.n);
        let keys: Vec<Vec<u8>> = (0..self.cfg.samples)
            .map(|_| key_for(rng.below(self.cfg.n)))
            .collect();
        // Warm-up: untimed reads so first-touch costs (mmap of odb indexes)
        // do not distort the sample.
        for k in keys.iter().take(50) {
            get(&self.store, root, k)?;
        }
        let mut times = Vec::with_capacity(keys.len());
        let mut found = 0usize;
        for k in &keys {
            let t = Instant::now();
            let v = get(&self.store, root, k)?;
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
        Ok(found)
    }

    fn point_read(&self) -> BenchResult<()> {
        println!("\n## point-read (loose, keys={})", self.cfg.n);
        let found = self.point_read_on(&self.v0, "point_read_loose")?;
        assert_eq!(
            found, self.cfg.samples,
            "every sampled base key must resolve"
        );
        Ok(())
    }

    // -- Scenario 3: range scans ---------------------------------------------

    fn scan(&self, report: &mut Report) -> BenchResult<()> {
        println!("\n## scan (keys={})", self.cfg.n);
        self.progress("full scan...");
        let t0 = Instant::now();
        let mut count = 0u64;
        let mut bytes = 0u64;
        for item in scan(&self.store, &self.v0, ..)? {
            let (k, v) = item?;
            count += 1;
            bytes += (k.len() + v.len()) as u64;
        }
        let full = t0.elapsed();
        assert_eq!(count, self.cfg.n, "full scan must see every key");
        report.scan_count = count;
        println!(
            "full_scan: keys={count} time={} throughput={:.0} keys/s ({:.1} MiB/s payload)",
            fmt_dur(full),
            count as f64 / full.as_secs_f64().max(f64::MIN_POSITIVE),
            bytes as f64 / full.as_secs_f64().max(f64::MIN_POSITIVE) / (1024.0 * 1024.0)
        );

        self.progress(&format!(
            "{} windows of {} keys...",
            self.cfg.windows, self.cfg.window_size
        ));
        let mut rng = Rng::new(0x5ca9 + self.cfg.n);
        let mut times = Vec::with_capacity(self.cfg.windows);
        for _ in 0..self.cfg.windows {
            let start = rng.below(
                self.cfg
                    .n
                    .saturating_sub(self.cfg.window_size as u64)
                    .max(1),
            );
            let start_key = key_for(start);
            let range = (Bound::Included(start_key.as_slice()), Bound::Unbounded);
            let t = Instant::now();
            let got = scan(&self.store, &self.v0, range)?
                .take(self.cfg.window_size)
                .count();
            times.push(t.elapsed());
            assert_eq!(
                got, self.cfg.window_size,
                "window scan must return its full size"
            );
        }
        let (mean, p50, p99, max) = dur_stats(times);
        println!(
            "window_scan: windows={} size={} mean={} p50={} p99={} max={}",
            self.cfg.windows,
            self.cfg.window_size,
            fmt_dur(mean),
            fmt_dur(p50),
            fmt_dur(p99),
            fmt_dur(max)
        );
        Ok(())
    }

    // -- Scenario 4: single-key updates --------------------------------------

    fn update(&mut self, report: &mut Report) -> BenchResult<()> {
        println!("\n## update (single-key, keys={})", self.cfg.n);
        let height = self.head.height();
        let mut rng = Rng::new(0x0bda7e + self.cfg.n);
        let mut times = Vec::with_capacity(self.cfg.updates);
        let mut chunk_counts = Vec::with_capacity(self.cfg.updates);
        let mut root = self.head.clone();
        for u in 0..self.cfg.updates {
            let i = rng.below(self.cfg.n);
            let op = BatchOp::Put(key_for(i), value_for(i, 1_000_000 + u as u64));
            self.store.reset();
            let t = Instant::now();
            root = apply_batch(&self.store, &root, [op])?;
            times.push(t.elapsed());
            chunk_counts.push(self.store.puts());
        }
        self.head = root;
        let (mean, p50, p99, max) = dur_stats(times);
        println!(
            "update_latency (apply only): n={} mean={} p50={} p99={} max={}",
            self.cfg.updates,
            fmt_dur(mean),
            fmt_dur(p50),
            fmt_dur(p99),
            fmt_dur(max)
        );
        chunk_counts.sort_unstable();
        let sum: u64 = chunk_counts.iter().sum();
        let amp_max = *chunk_counts.last().unwrap_or(&0);
        report.update_amp_max = amp_max;
        println!(
            "update_chunks_written (write amplification): mean={:.1} p50={} max={amp_max} (no commit)",
            sum as f64 / chunk_counts.len().max(1) as f64,
            chunk_counts[chunk_counts.len() / 2],
        );
        // A single put rewrites only the root→leaf spine; a boundary shift can
        // add at most a split/merge either side. This is the property that
        // makes the git-ODB chunk store viable (Option A) — assert it.
        assert!(
            amp_max <= height as u64 + 2,
            "single-key update amplification {amp_max} exceeds spine bound (height {height} + 2)"
        );
        Ok(())
    }

    // -- Scenario 5: diff between adjacent versions --------------------------

    fn diff(&mut self, report: &mut Report) -> BenchResult<()> {
        println!(
            "\n## diff (adjacent versions, 1% churn, keys={})",
            self.cfg.n
        );
        let old_root = self.head.clone();

        let mut rng = Rng::new(0xd1ff + self.cfg.n);
        let ops = churn_ops_structural(self.cfg.n, 2_000_000, &mut rng);
        let deduped = dedup_ops(ops.clone());
        self.progress(&format!("applying {}-op churn batch...", ops.len()));
        self.store.reset();
        let t = Instant::now();
        let new_root = apply_batch(&self.store, &old_root, ops)?;
        let apply = t.elapsed();
        let batch_chunks = self.store.puts();
        println!(
            "churn_batch: ops={} apply_time={} chunks_written={batch_chunks}",
            deduped.len(),
            fmt_dur(apply)
        );

        // Expected changes classified from the deduped ops: deletes of base
        // keys are removals, puts of `...x` keys additions, other puts
        // modifications.
        let mut expected: Vec<(Vec<u8>, &'static str)> = deduped
            .iter()
            .map(|op| match op {
                BatchOp::Delete(k) => (k.clone(), "removed"),
                BatchOp::Put(k, _) if k.ends_with(b"x") => (k.clone(), "added"),
                BatchOp::Put(k, _) => (k.clone(), "modified"),
            })
            .collect();
        expected.sort();

        self.progress("diff walk...");
        let t = Instant::now();
        let mut changes: Vec<(Vec<u8>, &'static str)> = Vec::new();
        for entry in diff(&self.store, &old_root, &new_root)? {
            let entry = entry?;
            let kind = diff_kind(entry.before.is_some(), entry.after.is_some())
                .ok_or("diff yielded an entry with neither before nor after")?;
            changes.push((entry.key.to_vec(), kind));
        }
        let walk = t.elapsed();
        let (adds, dels, mods) = changes
            .iter()
            .fold((0, 0, 0), |(a, d, m), (_, k)| match *k {
                "added" => (a + 1, d, m),
                "removed" => (a, d + 1, m),
                _ => (a, d, m + 1),
            });
        report.diff_changes = changes.len();
        println!(
            "diff_walk: time={} changes={} (added={adds} removed={dels} modified={mods})",
            fmt_dur(walk),
            changes.len()
        );

        let mut sorted_changes = changes.clone();
        sorted_changes.sort();
        report.diff_matches_expected = Some(sorted_changes == expected);
        assert_eq!(
            sorted_changes, expected,
            "diff must report exactly the churned keys"
        );
        println!("diff_verified_against_ops: ok");

        // Belt-and-braces at small scale: recompute the diff from two full
        // scans, and check history independence (invariant 1) by rebuilding
        // the churned contents from scratch in a throwaway store.
        if self.cfg.verify {
            self.progress("verifying diff against full scans...");
            let scan_map = |root: &Root| -> BenchResult<Vec<(Vec<u8>, Vec<u8>)>> {
                let mut out = Vec::new();
                for item in scan(&self.store, root, ..)? {
                    let (k, v) = item?;
                    out.push((k.to_vec(), v.to_vec()));
                }
                Ok(out)
            };
            let a = scan_map(&old_root)?;
            let b = scan_map(&new_root)?;
            let naive = naive_scan_diff(&a, &b);
            report.diff_matches_scan = Some(sorted_changes == naive);
            assert_eq!(sorted_changes, naive, "diff must match a scan-based diff");
            println!("diff_verified_against_scan: ok");

            // History independence: apply_batch's new_root must equal a fresh
            // bulk_load of the resulting contents.
            let final_contents: Vec<(Vec<u8>, Vec<u8>)> = b;
            let tmp = tempdir_repo()?;
            let scratch = CountingStore::create(&tmp)?;
            let rebuilt = bulk_load(&scratch, self.params, final_contents)?;
            let independent = rebuilt.hash() == new_root.hash();
            report.history_independent = Some(independent);
            assert!(
                independent,
                "history independence violated: apply_batch root != bulk_load root"
            );
            std::fs::remove_dir_all(&tmp).ok();
            println!("history_independence: ok (apply_batch root == bulk_load root)");
        }

        self.head = new_root.clone();
        self.head_commit = commit_root(
            &self.store,
            &new_root,
            REF_HEAD,
            "bench: 1% churn for diff",
            Some(self.head_commit),
        )?;
        Ok(())
    }

    // -- Scenario 6: repo growth over simulated import commits ---------------

    fn growth(&mut self, report: &mut Report) -> BenchResult<()> {
        println!(
            "\n## growth (import commits at 1% churn, keys={})",
            self.cfg.n
        );
        let churn = (self.cfg.n / 100).max(1) as usize;
        println!(
            "commits={} churn_per_commit={churn} (value updates on existing keys)",
            self.cfg.growth_commits
        );
        let mut rng = Rng::new(0x94011 + self.cfg.n);
        let mut apply_times = Vec::new();
        let mut commit_times = Vec::new();
        let mut chunk_counts = Vec::new();
        let mut root = self.head.clone();
        let mut parent = self.head_commit;
        for c in 0..self.cfg.growth_commits {
            let ver = 3_000_000 + c as u64;
            let ops: Vec<BatchOp> = (0..churn)
                .map(|_| {
                    let i = rng.below(self.cfg.n);
                    BatchOp::Put(key_for(i), value_for(i, ver))
                })
                .collect();
            self.store.reset();
            let t = Instant::now();
            root = apply_batch(&self.store, &root, ops)?;
            apply_times.push(t.elapsed());
            chunk_counts.push(self.store.puts());
            let t = Instant::now();
            parent = commit_root(
                &self.store,
                &root,
                REF_HEAD,
                &format!("bench: import commit {}", c + 1),
                Some(parent),
            )?;
            commit_times.push(t.elapsed());
            if !self.cfg.quiet && (c + 1) % 10 == 0 {
                self.progress(&format!(
                    "growth: {}/{} commits, repo {}",
                    c + 1,
                    self.cfg.growth_commits,
                    fmt_kb(du_kb(&self.cfg.repo))
                ));
            }
        }
        self.head = root;
        self.head_commit = parent;
        report.growth_commits = self.cfg.growth_commits;

        let (amean, ap50, ap99, _) = dur_stats(apply_times);
        let (cmean, cp50, cp99, _) = dur_stats(commit_times);
        let csum: u64 = chunk_counts.iter().sum();
        println!(
            "apply_per_commit: mean={} p50={} p99={}",
            fmt_dur(amean),
            fmt_dur(ap50),
            fmt_dur(ap99)
        );
        println!(
            "commit_per_commit (anchors the full chunk set): mean={} p50={} p99={}",
            fmt_dur(cmean),
            fmt_dur(cp50),
            fmt_dur(cp99)
        );
        println!(
            "chunks_written_per_commit: mean={:.0}",
            csum as f64 / chunk_counts.len().max(1) as f64
        );

        let raw = du_kb(&self.cfg.repo);
        println!("repo_size_raw_loose: {}", fmt_kb(raw));
        self.progress("git gc --prune=now ...");
        let t = Instant::now();
        let (ok, _) = git(&self.cfg.repo, &["gc", "--prune=now", "--quiet"]);
        let gc = t.elapsed();
        if ok {
            println!(
                "repo_size_after_gc: {} (gc took {})",
                fmt_kb(du_kb(&self.cfg.repo)),
                fmt_dur(gc)
            );
        } else {
            println!("repo_size_after_gc: (git gc unavailable)");
        }

        // The committed head manifest must reconstruct exactly the root we
        // hold in memory (commit/manifest round-trip), and reading it back
        // must yield a consistent, non-empty scan.
        self.progress("reading head back...");
        let head_root = self.read_ref_root(REF_HEAD)?;
        assert_eq!(
            head_root.hash(),
            self.head.hash(),
            "committed head manifest must reconstruct the in-memory root"
        );
        let mut count = 0u64;
        for item in scan(&self.store, &head_root, ..)? {
            item?;
            count += 1;
        }
        report.growth_readback = Some(count);
        assert!(count > 0, "head must not be empty after growth");
        println!("head_readback: {count} keys");
        Ok(())
    }

    // -- Scenario 7: loose vs packed point reads -----------------------------

    fn repack_read(&self, report: &mut Report) -> BenchResult<()> {
        println!("\n## repack-read (packed, keys={})", self.cfg.n);
        self.progress("git gc --prune=now (idempotent) ...");
        let (ok, _) = git(&self.cfg.repo, &["gc", "--prune=now", "--quiet"]);
        if !ok {
            println!("repack-read: skipped (git gc unavailable)");
            return Ok(());
        }
        println!("repo_size_packed: {}", fmt_kb(du_kb(&self.cfg.repo)));
        // Reopen the store so reads go through the repacked ODB, and resolve
        // v0 from its committed ref to exercise the real commit-read path.
        let store = CountingStore::open(&self.cfg.repo)?;
        let commit = store
            .inner()
            .read_ref(REF_V0)?
            .ok_or("refs/bench/v0 missing")?;
        let manifest = store
            .inner()
            .read_commit(&commit)?
            .ok_or("v0 commit unreadable")?
            .manifest;
        let root = decode_manifest(&manifest)?;
        let packed = Bench {
            cfg: self.cfg.clone(),
            store,
            params: self.params,
            v0: root.clone(),
            head: root.clone(),
            head_commit: commit,
        };
        let found = packed.point_read_on(&root, "point_read_packed")?;
        report.packed_reads_found = Some(found);
        assert_eq!(
            found, self.cfg.samples,
            "every sampled key must resolve after repack"
        );
        Ok(())
    }

    fn read_ref_root(&self, refname: &str) -> BenchResult<Root> {
        let commit = self
            .store
            .inner()
            .read_ref(refname)?
            .ok_or_else(|| format!("{refname} missing"))?;
        let manifest = self
            .store
            .inner()
            .read_commit(&commit)?
            .ok_or_else(|| format!("{refname} commit unreadable"))?
            .manifest;
        decode_manifest(&manifest)
    }
}

/// Dedup ops the way `apply_batch` does (sorted, last wins).
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

/// A naive `(key, kind)` diff over two sorted key/value lists, for
/// cross-checking the prolly `diff`.
fn naive_scan_diff(
    a: &[(Vec<u8>, Vec<u8>)],
    b: &[(Vec<u8>, Vec<u8>)],
) -> Vec<(Vec<u8>, &'static str)> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && (j >= b.len() || a[i].0 < b[j].0) {
            out.push((a[i].0.clone(), "removed"));
            i += 1;
        } else if j < b.len() && (i >= a.len() || b[j].0 < a[i].0) {
            out.push((b[j].0.clone(), "added"));
            j += 1;
        } else {
            if a[i].1 != b[j].1 {
                out.push((a[i].0.clone(), "modified"));
            }
            i += 1;
            j += 1;
        }
    }
    out.sort();
    out
}

/// A fresh temporary bare-repo path under the system temp dir, without a
/// tempfile dependency in the binary. Used for the history-independence
/// scratch store. Uniqueness comes from the pid, a wall-clock nanosecond
/// stamp and a process-lifetime counter.
fn tempdir_repo() -> BenchResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(std::env::temp_dir().join(format!(
        "acetone-bench-scratch-{}-{nanos}-{seq}",
        std::process::id()
    )))
}

/// Run the requested scenarios (or [`ALL_SCENARIOS`] when `None`) in canonical
/// order against a fresh repo, returning the structural [`Report`]. `bulk-load`
/// always runs first regardless of the requested set, since every other
/// scenario reads the repo it creates.
pub fn run(cfg: Config, scenarios: Option<&[String]>) -> BenchResult<Report> {
    let requested: Vec<String> = scenarios
        .map(|s| s.to_vec())
        .unwrap_or_else(|| ALL_SCENARIOS.iter().map(|s| s.to_string()).collect());
    let want = |name: &str| requested.iter().any(|s| s == name);

    let (mut bench, mut report) = Bench::create(cfg)?;
    if want("point-read") {
        bench.point_read()?;
    }
    if want("scan") {
        bench.scan(&mut report)?;
    }
    if want("update") {
        bench.update(&mut report)?;
    }
    if want("diff") {
        bench.diff(&mut report)?;
    }
    if want("growth") {
        bench.growth(&mut report)?;
    }
    if want("repack-read") {
        bench.repack_read(&mut report)?;
    }
    Ok(report)
}
