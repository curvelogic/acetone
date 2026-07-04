# prollytree crate — hands-on evaluation (bead acetone-28x.3, Gate A input)

*July 2026. Evaluated: crates.io v0.4.0 (latest release, May 2026) and git main (0.4.1-beta,
commit d48f129). Sandbox: `scratchpad/prollytree-eval` (kept out of the acetone repo).*

## Verdict: **reference-only**

Do not adopt; do not fork. Four disqualifiers, each individually sufficient:

1. **History independence — acetone's most load-bearing invariant — is broken in every
   released version.** Fixed on unreleased main only weeks ago (#185, post-v0.4.0).
2. **Structural diff is broken** for any tree deeper than one leaf, and the "working" diff
   is an O(n) full materialisation of both versions — the headline prolly-tree property
   (diff proportional to change) is not actually delivered anywhere in the crate.
3. **The git backend fails the point of git backing**: prolly node blobs are *unreachable*
   from any ref. `git gc --prune=now` destroys all data; `git clone` yields an empty store.
4. **No range scans at all** (forward or reverse) — a hard requirement for spec §3.2 and
   for every label/edge scan in the query layer.

What it *is* good for: the post-#185 streaming chunker (`src/streaming_chunker.rs`) is a
genuinely Dolt-style, well-documented, demonstrably history-independent design and is fast
(100k bulk load in 32 ms). Read it before writing `acetone-prolly`; do not depend on it.

## 1. API fit

- `insert/insert_batch/update/delete/delete_batch` mutate the tree in place; new root via
  `get_root_hash()`. Old roots stay resolvable in the content-addressed store, so a
  "batch → new root" discipline can be layered on. Acceptable but not the persistent-map
  API the spec wants.
- **Range scan: absent.** Only point `find()`, whole-tree `collect_keys()`/`list_keys()`
  (full materialisation) and a string-building `traverse()`. Nothing reverse. Disqualifying.
- **Structural diff: broken.** `Tree::diff` merge-joins the two *root nodes'* entries and
  never recurses; for internal roots it compares child hashes as user values. Empirically
  (`src/bin/diffbug.rs`): at n=20 (single leaf) diff is correct; at n=2,000 and n=10,000 a
  two-key change reports one bogus entry, `Modified asset:00000000` with binary hash noise
  as before/after values. The versioned store's `diff(from, to)` is correct but collects
  *every* pair of both versions into `HashMap`s — O(n), not O(diff).
- **Three-way merge: works, at KV level.** `merge(source_root, dest_root, base_root)`
  returns `Vec<MergeResult>` including `MergeConflict { key, base/source/destination }`;
  the git store's `try_merge_generic` correctly reported our overlapping-edit conflict
  (base `host-1` / theirs `feature-edit` / ours `main-edit`). Requires all three roots in
  one storage. Merge-base discovery via the git commit graph exists.

## 2. History independence (empirical)

Same 2,000-key set built five ways (sorted single batch; two shuffle seeds one-by-one;
shuffled batches of 97; superset-then-delete; overwrite-then-correct in shuffled passes):

| build order | v0.4.0 root (first 16 hex) | main root |
|---|---|---|
| sorted batch | `12c44f7e26196a26` | `12c44f7e26196a26` |
| shuffled singles | `12c44f7e26196a26` | `12c44f7e26196a26` |
| shuffled batches | **`b424c3d89bed63dd`** | `12c44f7e26196a26` |
| superset + delete | `12c44f7e26196a26` | `12c44f7e26196a26` |
| double-write | `12c44f7e26196a26` | `12c44f7e26196a26` |

v0.4.0 **FAILS** (batched shuffled insertion diverges); main **PASSES** all five, and a
mutate-then-revert sequence returned the exact base root. The fix (#185) rewrote the whole
mutation path (streaming chunker with cursor merge) and is unreleased and unsoaked. A
further design smell: `ProllyNode::get_hash` hashes the *unframed* concatenation of keys
then values, so `["ab","c"]` and `["a","bc"]` collide by construction.

## 3. Chunking control

`TreeConfig { base, modulus, pattern, min_chunk_size, max_chunk_size }` — but boundaries
are probabilistic in **entries, not bytes** (`pattern = 0xFF` ⇒ split probability 1/256
per entry; min/max are entry counts). With ~84-byte records (n=20,000):

| pattern | avg entries/node | approx leaf size |
|---|---|---|
| `0xFF` (default) | 344.8 | ~28 KiB |
| `0x3F` | 75.5 | ~6.2 KiB |
| `0x1F` | 34.2 | ~2.8 KiB |

A ~4 KiB *byte* target is only approachable indirectly and drifts with record size — spec
§3.2 wants a byte-based CDC target. The splitter (xxhash-per-item polynomial rolling hash
over a `min_chunk_size` window) is well commented on main but not a documented stable
format, and config persistence is a bincode sidecar file.

## 4. Git-backed storage

- Commits are **real git commits** (`git cat-file -t` ⇒ `commit`; `git log` renders them).
- But `git ls-tree -r` of a 1,000-key commit shows only two files:
  `data/prolly_config_tree_config` and `data/prolly_hash_mappings`. Node blobs are written
  loose via `gix` and referenced only from the *content* of the sidecar mapping file —
  `git fsck` reports them all dangling. Measured consequences (`src/bin/gitreach.rs`):
  after `git gc --prune=now`, `get()` returns `None` for all keys (the crate itself prints
  "Failed to load tree… missing git objects"); a `git clone` of the repo opens as an empty
  store. Push/pull/hosting — the entire rationale for Option A — silently lose the data.
- **Dual addressing scheme**: tree nodes are addressed by an internal SHA-256 digest,
  mapped to git OIDs via the sidecar `HashMap`. This is precisely what design doc
  Decision 1 rules out ("exactly one addressing scheme"). Swapping `NodeStorage` for our
  `ChunkStore` over gitoxide would *not* fix it: `NodeStorage` traffics in structured
  `ProllyNode`s keyed by `ValueDigest`, and the digest is baked into node identity and
  hashing — making the git OID the address is a fork-level rewrite, not a trait swap.
- I/O is fully synchronous/blocking gix calls; all writes are loose objects (`in-pack: 0`;
  3,511 loose objects after one 10k commit), no pack-first writing.

## 5. Performance sanity check (Apple Silicon, release build)

- In-memory 100k bulk load: v0.4.0 `insert_batch` **18.5 s** (quadratic-ish balancing);
  main **32 ms**. Point reads ~30–90 µs. Single-key update on a 100k tree: 28 ms (main).
  One-by-one inserts on main: ~1.5 ms each (10k in 15.3 s) — batch or suffer.
- Git-backed (main): 1,000 staged inserts 67 ms, commit 16 ms; 10k staged inserts 2.4 s,
  commit 33 ms; point get 67 µs. Orders of magnitude are fine for workbench scale — the
  problem is durability semantics, not speed.

## 6. Maturity signals

- **Bus factor 1**: 129/136 commits by the author (plus dependabot); 29 stars, 7 forks.
- Cadence: created May 2024; 0.2.0 → 0.4.0 between Jul 2025 and May 2026; the crate's
  core invariant fix landed after the last release. 2 open issues, 9 open PRs.
- Licence Apache-2.0 (compatible). ~32.5k LoC `src`, 416 `#[test]` fns, essentially no
  `unsafe` (one block in an optional feature). Scope creep: SQL (GlueSQL), Python
  bindings, "agent memory", vector search all in one crate.
- **Dependency weight: heavy.** `arrow`, `parquet` and `schemars` are *unconditional*
  dependencies; even with default features off plus `git`, the tree resolves ~329 crates.

## Recommendation for Gate A

Build `acetone-prolly` from scratch (roadmap Phase 1 as planned), treating this crate as:
(a) a worked example of the streaming-chunker construction to study, (b) a catalogue of
mistakes to test against — order-dependent roots, unframed hashing, unreachable git blobs,
entry-based chunk targets — and (c) mild validation that gix-based blob I/O is workable.
Nothing here undermines Decision 1 Option A itself: the failures are implementation
choices (sidecar mapping, dangling blobs), not evidence against git-as-chunk-store. The
Phase 0 benchmark gate on the git ODB still needs to be run with our own spike.
