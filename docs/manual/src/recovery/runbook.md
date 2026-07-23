# Recovery runbook

This is the chapter you reach for when something is broken. It is organised
by **symptom**: find the section whose symptom matches what you are seeing,
and each walks the same path — symptom, diagnosis, recovery, prevention.

Every procedure here was driven for real while writing: we built scratch
copies of the [asset registry](../getting-started/asset-registry.md),
actually broke them the way each section describes — killed locks, flipped
bytes in objects, deleted chunks, damaged refs — and ran the recovery. The
transcripts are quoted verbatim from those sessions (each section is
self-contained; commit hashes differ between sessions), with one cosmetic
change: the repositories lived in temporary directories, shown throughout as
`/srv/registry`. Where a scenario could not be honestly reproduced it is
clearly labelled as such.

A quick index:

| Symptom | Section |
|---|---|
| Writes fail with a lock error; reads fine | [Stale writer lock](#a-stale-writer-lock) |
| A query fails with an object-store error | [Corrupt object](#a-corrupt-object) |
| `fsck` says a chunk is absent from the store | [Missing object](#a-missing-object) |
| `fsck` reports a ref/commit error or advisory | [Damaged refs](#damaged-refs) |
| `status` suddenly says `(no commits yet)` | [HEAD names a missing branch](#head-names-a-branch-that-does-not-exist) |
| Dirty with changes you never made, after a crash | [Interrupted checkout](#an-interrupted-checkout) |
| You committed (or wrote) something you regret | [Undoing changes](#undoing-changes-the-happy-side-of-the-runbook) |
| `gc` refuses to run | [gc refuses while worktrees exist](#gc-refuses-while-linked-worktrees-exist) |
| Preparing for all of the above | [Backup and restore](#backup-and-restore) |

## First aid

Three rules before touching a repository you suspect is damaged:

1. **Stop the writers.** acetone is single-writer per repository; most of the
   states below are made worse only by more writes. Reads are almost never
   dangerous — diagnose freely.
2. **Copy the repository first.** An acetone repository is one directory.
   Before any surgery, `cp -R` it somewhere safe. Every recovery below is
   then reversible.
3. **Run `acetone fsck` before and after.** It is read-only, it works even
   when the workspace manifest itself is damaged, and "`fsck: clean`" after
   a repair is your evidence the repair worked.

## Reading fsck output

`acetone fsck` walks every branch, every commit reachable from it, and the
workspace, verifying that every manifest decodes, every chunk is present and
valid, and the repository's consistency properties hold. Findings come in two
severities:

- **`[error]`** — the version is damaged: some data cannot be read back
  faithfully. `fsck` exits non-zero.
- **`[advisory]`** — a consistency property is violated but the data is
  structurally intact (for example, a derived index disagreeing with the
  nodes map, repairable with `acetone reindex`, or a symbolic ref that
  resolves to nothing). `fsck` prints the advisory and exits **zero**.

Error findings you may meet, and where this chapter handles them: a manifest
that is missing or does not decode, a ref whose target is not a readable
acetone commit ([damaged refs](#damaged-refs)), a chunk that is absent
([missing object](#a-missing-object)) or unreadable
([corrupt object](#a-corrupt-object)), a map whose stored tree is not the
canonical tree for its contents, and an edge referencing an absent endpoint.
The advisory kinds — edge-map asymmetry, index inconsistency and
"nothing to verify" refs — are consistency drifts, not damage; the first two
are repaired by `acetone reindex` and should not arise from acetone's own
write path. (We did not manage to produce the derived-map advisories against
a healthy binary — which is the point: they exist to catch foreign writers
and older repositories.)

## A stale writer lock

**Symptom.** Every write fails after a ~5 second pause; reads work
normally:

```console
$ acetone query 'MATCH (s:Service {name: "billing"}) SET s.version = "7.0.3"'
error: git backend error while locking refs for compare-and-swap: The lock for resource '/srv/registry/acetone-refs' could not be obtained after 5.00s after 26 attempt(s). The lockfile at '/srv/registry/acetone-refs.lock' might need manual deletion.
```

**What happened.** All acetone writers on a repository are serialised
through one lock file, `acetone-refs.lock`, in the repository's git
directory. It is held only for the duration of one ref update and removed
when the writer finishes — unless the writer was killed while holding it
(SIGKILL, power loss, a container evicted mid-write). The stale file then
makes every subsequent write back off for about five seconds and fail,
rather than hang or corrupt anything.

**Diagnosis.** Reads are never blocked by this lock, so the repository is
fully inspectable:

```console
$ acetone query 'MATCH (s:Service {name: "billing"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 7.0.2     │
└───────────┘
1 row
$ acetone status
On branch main
HEAD: 26405c52d75030aa4c20b862af6f12aa0f354eb3
workspace: clean
nodes: 12, edges: 15, schema entries: 7
```

Confirm the lock file exists and that **no acetone process is still
running** against this repository (`ps` / your process supervisor). A live
writer legitimately holds the lock for milliseconds; the stale case is a
lock file with no living owner.

**Recovery.** Once you are sure no acetone process is running against the
repository, delete the file and retry:

```console
$ rm /srv/registry/acetone-refs.lock
$ acetone query 'MATCH (s:Service {name: "billing"}) SET s.version = "7.0.3"'
(no columns)
1 property set
```

This is safe because the lock only serialises writers; it protects no data
itself. Ref updates are atomic compare-and-swap operations underneath, so
even the killed writer left either the old state or the new state — never
half of each.

For a repository operated through linked git worktrees, the lock lives in
the **common** (main) git directory, not the per-worktree one.

**Prevention.** None needed in normal operation — the lock cannot go stale
without a killed process or power loss. If a supervisor regularly SIGKILLs
acetone mid-write, give it a grace period.

## A corrupt object

**Symptom.** A query that was fine yesterday fails with an object-store
error:

```console
$ acetone query 'MATCH (s:Service) RETURN s.name, s.version ORDER BY s.name'
error: git backend error while reading object header: An error occurred while obtaining an object from the loose object store
```

This is what disk-level damage looks like: a graph chunk is stored as a git
object, and one of them no longer inflates. (We produced it by flipping
eight bytes in the middle of a loose object file.)

**Diagnosis.** `fsck` names the damaged chunk and every version that
depends on it:

```console
$ acetone fsck
[error] workspace refs/worktree/acetone/workspace / nodes / chunk 6cc09bd53ab9d19e9d7792961a58dc10a4b60298: store could not return the chunk: git backend error while reading object header: An error occurred while obtaining an object from the loose object store
[error] commit 63226109e4a2646b7586d827d32e41004feed57f (via refs/heads/main) / nodes / chunk 6cc09bd53ab9d19e9d7792961a58dc10a4b60298: store could not return the chunk: git backend error while reading object header: An error occurred while obtaining an object from the loose object store
fsck: 2 error(s), 0 advisory(ies)
error: repository has integrity errors
```

Because the repository is a git repository, `git fsck` confirms the same
damage from git's side:

```console
$ git fsck
error: inflate: data stream error (invalid distance too far back)
error: unable to unpack header of ./objects/6c/c09bd53ab9d19e9d7792961a58dc10a4b60298
error: 6cc09bd53ab9d19e9d7792961a58dc10a4b60298: object corrupt or missing: ./objects/6c/c09bd53ab9d19e9d7792961a58dc10a4b60298
```

**Recovery.** Corruption cannot be repaired in place — the bytes are gone.
But every object is content-addressed, so an intact copy of the same object
from **any** clone or backup is, provably, the right bytes. If you have a
backup (see [Backup and restore](#backup-and-restore)), restore the one
object:

```console
$ git -C /srv/registry-backup.git cat-file blob 6cc09bd53ab9d19e9d7792961a58dc10a4b60298 > /tmp/chunk.bin
$ rm /srv/registry/objects/6c/c09bd53ab9d19e9d7792961a58dc10a4b60298
$ git hash-object -w -t blob /tmp/chunk.bin
6cc09bd53ab9d19e9d7792961a58dc10a4b60298
$ acetone fsck
fsck: clean
```

Note `hash-object -w` printed the id we needed — content addressing means
writing the right bytes *is* writing the right object; there is nothing
else to fix up. If many objects are damaged (a failing disk), do not repair
object by object: re-clone from the backup instead, as in the
[next section](#a-missing-object), and retire the disk.

**Prevention.** Backups (below), and `acetone fsck` on a schedule — it is
cheap on repositories of this scale and turns silent damage into a named,
dated finding.

## A missing object

**Symptom / diagnosis.** As above, but the object file is gone entirely
rather than unreadable — `fsck` distinguishes the two:

```console
$ acetone fsck
[error] workspace refs/worktree/acetone/workspace / nodes / chunk 6cc09bd53ab9d19e9d7792961a58dc10a4b60298: referenced by the tree but absent from the store
[error] commit 63226109e4a2646b7586d827d32e41004feed57f (via refs/heads/main) / nodes / chunk 6cc09bd53ab9d19e9d7792961a58dc10a4b60298: referenced by the tree but absent from the store
fsck: 2 error(s), 0 advisory(ies)
error: repository has integrity errors
```

**Recovery.** The same two options as for corruption: restore the single
object from a healthy clone (exactly as above), or — the robust path when
you do not know how much is missing — make a fresh clone of the backup and
verify it:

```console
$ git clone --mirror /srv/registry-backup.git /srv/registry-recovered
Cloning into bare repository '/srv/registry-recovered'...
done.
$ cd /srv/registry-recovered
$ acetone fsck
fsck: clean
$ acetone status
On branch main
HEAD: 63226109e4a2646b7586d827d32e41004feed57f
workspace: clean
nodes: 12, edges: 15, schema entries: 7
```

Then point your tooling at the recovered repository and retire the damaged
one. Anything committed before the last backup refresh is recovered
exactly; uncommitted workspace changes are lost (they never travel — see
[Backup and restore](#backup-and-restore)).

## Damaged refs

Refs are the one part of the repository that is *not* content-addressed —
they are small files naming hashes — so they are where hand edits, sync
tools and interrupted operations leave marks. Three real cases:

### A branch pointing at a commit that does not exist

We planted `refs/heads/ghost` containing a hash that names nothing:

```console
$ acetone checkout ghost
error: checking out branch "ghost": ref "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" does not point at an acetone commit
$ acetone fsck
[error] commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef (via refs/heads/ghost): commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef is referenced by history but absent from the store
fsck: 1 error(s), 0 advisory(ies)
error: repository has integrity errors
```

**Recovery.** Delete the ref, or repoint it at a commit that exists. Ref
surgery is one of the few places this manual reaches for plain git — ref
deletion does not touch graph data:

```console
$ git update-ref -d refs/heads/ghost
$ acetone fsck
fsck: clean
```

If the branch *should* exist, find the commit it ought to name (`acetone
log` on a healthy clone, or `git reflog` if you have one) and
`git update-ref refs/heads/<name> <commit>` instead of deleting.

### A symbolic ref that resolves to nothing

A branch that is a symbolic ref to an absent branch is intact-but-empty,
so `fsck` reports it as an advisory, not an error — there is nothing to
verify behind it, and the sin fsck avoids is silence:

```console
$ acetone fsck
[advisory] ref refs/heads/spare: symbolic ref (-> refs/heads/decommissioned) resolves to no object (dangling or unborn), so there is nothing to verify
fsck: 0 error(s), 1 advisory(ies)
```

A symbolic-ref **cycle**, on the other hand, is damage:

```console
$ acetone fsck
[error] ref refs/heads/loop-a: symbolic ref could not be resolved: corrupt symbolic ref: chain from "refs/heads/loop-a" exceeds 10 levels (a cycle, or hostile nesting)
[error] ref refs/heads/loop-b: symbolic ref could not be resolved: corrupt symbolic ref: chain from "refs/heads/loop-b" exceeds 10 levels (a cycle, or hostile nesting)
fsck: 2 error(s), 0 advisory(ies)
error: repository has integrity errors
```

**Recovery.** Delete the offending symrefs:

```console
$ git symbolic-ref --delete refs/heads/loop-a
$ git symbolic-ref --delete refs/heads/loop-b
$ acetone fsck
fsck: clean
```

**Prevention.** Do not hand-write files under `refs/`; when you must do ref
surgery, use `git update-ref` / `git symbolic-ref`, which validate what they
can and update atomically.

### HEAD names a branch that does not exist

**Symptom.** After a typo'd bit of ref surgery (`git symbolic-ref HEAD
refs/heads/mian`), the repository suddenly looks unborn — and dirty:

```console
$ acetone status
On branch mian
HEAD: (no commits yet)
workspace: dirty
nodes: 12, edges: 15, schema entries: 7
```

Nothing is lost: `main` and all its history are intact; HEAD just points at
a branch name with no commits. The workspace (still holding your real data)
now differs from that unborn branch's nothing, hence `dirty`. This state has
a trap: **a `commit` here would create the misspelled branch** with your
whole graph as a parentless first commit. Diagnose with git if in doubt —
`git symbolic-ref HEAD` prints where HEAD really points (`refs/heads/mian`).

**Recovery.** Check out the branch you meant. The dirty guard measures
*content*, not the dirty flag — and the workspace content here *is* `main`'s
committed state, so nothing can be discarded and checkout proceeds:

```console
$ acetone checkout main
switched to branch "main"
$ acetone status
On branch main
HEAD: 88ad6ed4a96ee852a46dc6a25c1fc903d106414f
workspace: clean
nodes: 12, edges: 15, schema entries: 7
```

(If the workspace had also held real uncommitted changes, checkout would
refuse as usual; the escape is then `git symbolic-ref HEAD refs/heads/main`
— pointing HEAD back by hand, which is exactly what the typo mispointed.)

## An interrupted checkout

**Symptom.** A `checkout` was interrupted (crash, kill, power loss).
`status` reports the *old* branch, dirty, with "uncommitted changes" you do
not remember making — and checking out any *other* branch refuses:

```console
$ acetone status
On branch audit
HEAD: 18ce05c10ca544fa65718ef15ada5d115fa538a6
workspace: dirty
nodes: 12, edges: 15, schema entries: 7
$ acetone checkout audit
error: checking out branch "audit": workspace has uncommitted changes; commit them first
```

**What happened.** `checkout` makes two ref updates: first it resets the
workspace to the target branch's committed state, then it moves HEAD. An
interruption between the two leaves the workspace holding the *target*
branch's content while HEAD still names the *old* branch — which reads as
"dirty on the old branch". (We constructed this state deliberately — a full
`checkout main` from `audit`, then winding HEAD back — which is exactly the
state a kill between the two updates leaves.)

**Diagnosis.** The fingerprint is that the "uncommitted changes" are
precisely the checkout target's committed state. Here HEAD claims `audit`
(whose commit set `edge1.audited`), but the workspace shows `main`'s
content:

```console
$ acetone query 'MATCH (h:Host {name: "edge1"}) RETURN h.audited'
┌───────────┐
│ h.audited │
├───────────┤
│ NULL      │
└───────────┘
1 row
```

If instead the workspace holds real uncommitted work you recognise, you are
not in this scenario — you are just dirty; commit and carry on.

**Recovery.** Run the same checkout again. Because the workspace already
holds exactly the target's committed content, nothing can be discarded — the
dirty guard measures content, not the flag — so the re-run simply completes
the interrupted move of HEAD:

```console
$ acetone checkout main
switched to branch "main"
$ acetone status
On branch main
HEAD: 88ad6ed4a96ee852a46dc6a25c1fc903d106414f
workspace: clean
nodes: 12, edges: 15, schema entries: 7
$ acetone fsck
fsck: clean
```

Checking out any branch whose content differs still refuses, as the symptom
showed — the guard narrows only when it can prove nothing is lost.

Note the ordering is itself a safety property: from the interrupted state, a
`commit` would have recorded the target's committed content onto the old
branch — explicit and recoverable history, never silent data loss.

## Undoing changes: the happy side of the runbook

Nothing here is damage — this is what a version-controlled graph is *for*.
Two situations, one mechanism.

### Discarding uncommitted workspace changes

The workspace holds an unwanted change (here, a `billing` version bump that
should not have happened):

```console
$ acetone status
On branch main
HEAD: 26405c52d75030aa4c20b862af6f12aa0f354eb3
workspace: dirty
nodes: 12, edges: 15, schema entries: 7
```

There is no `discard` command, and `checkout` deliberately refuses to throw
work away:

```console
$ acetone branch parked
created branch "parked" at 26405c52d75030aa4c20b862af6f12aa0f354eb3
$ acetone checkout parked
error: checking out branch "parked": workspace has uncommitted changes; commit them first
```

So the discard route is: **commit the unwanted state, then wind the branch
back**. The unwanted work becomes an ordinary (unreferenced) commit — which
also means the "discard" is itself undoable until it is garbage-collected.

```console
$ acetone commit -m "WIP: unwanted billing bump, to be discarded"
committed e04f9ee4e2ae6b044fa6e40aecbe83a69b727bfc
$ acetone diff parked main
~ node "Service" ["billing"]
$ acetone checkout parked
switched to branch "parked"
$ git update-ref refs/heads/main 26405c52d75030aa4c20b862af6f12aa0f354eb3 e04f9ee4e2ae6b044fa6e40aecbe83a69b727bfc
$ acetone checkout main
switched to branch "main"
$ acetone status
On branch main
HEAD: 26405c52d75030aa4c20b862af6f12aa0f354eb3
workspace: clean
nodes: 12, edges: 15, schema entries: 7
$ git update-ref -d refs/heads/parked
```

Two details worth reading twice:

- The `git update-ref` carries **three** arguments: new value *and expected
  old value*. That makes it a compare-and-swap — it fails rather than
  clobber if anything else moved `main` meanwhile.
- Winding a branch back is the one recovery in this chapter that rewrites
  (local) history. If `main` has been pushed or pulled elsewhere, prefer a
  *forward* fix: commit the correcting change instead.

### Undoing a bad commit

Identical mechanism: find the last good commit with `acetone log` and
`acetone diff`, inspect it with time travel, then wind the branch back to
it with the same three-argument `git update-ref`. The version-control core
loop — `log`, `diff`, `query --at` — is your investigation kit:

```console
$ acetone query --at e04f9ee4e2ae6b044fa6e40aecbe83a69b727bfc 'MATCH (s:Service {name: "billing"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 7.0.3     │
└───────────┘
1 row
```

That is the *discarded* commit, still perfectly readable by hash. How long
does it stay recoverable? `acetone gc` is **representation-only** — it
repacks, it never deletes objects — so even after:

```console
$ acetone gc
gc: packed 24 object(s) (1 delta, 23 whole) into 3228 bytes; pruned 24 loose object(s), 0 superseded pack(s)
$ acetone query --at e04f9ee4e2ae6b044fa6e40aecbe83a69b727bfc 'MATCH (s:Service {name: "billing"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 7.0.3     │
└───────────┘
1 row
```

…the commit is still there. Only stock **git** gc expires unreachable
objects:

```console
$ git gc --prune=now
$ acetone query --at e04f9ee4e2ae6b044fa6e40aecbe83a69b727bfc 'MATCH (s:Service) RETURN s.name'
error: cannot resolve "e04f9ee4e2ae6b044fa6e40aecbe83a69b727bfc" to a branch, ref or commit
```

So: if you might want a wound-back commit again, put a branch on it before
winding back. (Creating a branch at an arbitrary commit is `git update-ref
refs/heads/<name> <commit>` for now — the CLI's `branch` command currently
creates branches only at the current head.)

Reassuringly, even `git gc --prune=now` did not touch uncommitted work: the
workspace we had dirtied beforehand survived it intact —

```console
$ acetone status
On branch main
HEAD: 26405c52d75030aa4c20b862af6f12aa0f354eb3
workspace: dirty
nodes: 12, edges: 15, schema entries: 7
$ acetone query 'MATCH (h:Host {name: "db2"}) RETURN h.os'
┌───────────┐
│ h.os      │
├───────────┤
│ linux-6.9 │
└───────────┘
1 row
```

— because the workspace is anchored by a real ref that git's gc respects.
(Stock `git gc` does cost you acetone's hand-chosen pack deltas — space,
never data; the next `acetone gc` restores the ratio.)

### A merge gone wrong

Merges have their own escape hatch, covered with the merge machinery in the
[history chapter](../working/history-branch-merge.md): `acetone merge
--abort` discards a merge in progress and restores the branch tip, and it
is deliberately idempotent — if an abort is itself interrupted, running it
again finishes the job.

## gc refuses while linked worktrees exist

**Symptom.**

```console
$ acetone gc
error: consolidating the object store: gc is not safe while linked worktrees exist (it could prune their uncommitted work); run it with a single worktree
$ git worktree list
/srv/registry     (bare)
/srv/registry-wt  26405c5 [main]
```

**What happened.** Nothing is broken — this refusal *is* the safety
mechanism. Each linked worktree keeps its uncommitted workspace under
private per-worktree refs that a reachability walk from another worktree
does not see. A gc that pruned "unreachable" objects could therefore
destroy a sibling worktree's saved-but-uncommitted work. acetone refuses
the whole operation instead; when in doubt, gc keeps data.

**Recovery.** Not a repair — a scheduling decision. Run gc when the
repository is back to a single worktree:

```console
$ git worktree remove /srv/registry-wt
$ acetone gc
gc: packed 24 object(s) (1 delta, 23 whole) into 3228 bytes; pruned 24 loose object(s), 0 superseded pack(s)
```

(Remove worktrees with `git worktree remove`, which refuses if the worktree
is itself dirty in git's terms — do not just `rm -rf` the directory.)

**Prevention.** Treat `gc` as periodic maintenance for quiet,
single-worktree moments. Skipping it costs disk space, never correctness.

## Backup and restore

The whole backup story rests on one fact from the
[first chapter](../getting-started/first-graph.md): an acetone repository
**is** a git repository. Backup is `git clone`; refreshing the backup is a
fetch; restoring is cloning back. The remote end never needs to know
acetone exists.

**Taking a backup.**

```console
$ git clone --mirror /srv/registry /srv/registry-backup.git
Cloning into bare repository '/srv/registry-backup.git'...
done.
```

**Refreshing it** (run on a schedule):

```console
$ cd /srv/registry-backup.git && git remote update
From /srv/registry
   26405c5..fbe66f0  main       -> main
 * [new ref]         refs/worktree/acetone/workspace -> refs/worktree/acetone/workspace
```

(You will see the workspace ref in mirror transfers like that last line;
it is harmless and inert in the backup — more on this in rule 1 below.)

**Restoring** is the clone in the [missing object](#a-missing-object)
section: `git clone --mirror` back, then `acetone fsck` to certify the
result, then `acetone status` and carry on. Pushing to a hosted remote
(`git push --mirror <url>`) works identically for off-machine backup.

Three rules that make backups trustworthy:

1. **Only committed history travels.** We cloned the backup above while the
   source workspace was dirty (an uncommitted `db2` change). The backup
   read back clean, without the change:

   ```console
   $ cd /srv/registry-backup.git && acetone status
   On branch main
   HEAD: 26405c52d75030aa4c20b862af6f12aa0f354eb3
   workspace: clean
   nodes: 12, edges: 15, schema entries: 7
   $ acetone query 'MATCH (h:Host {name: "db2"}) RETURN h.os'
   ┌───────┐
   │ h.os  │
   ├───────┤
   │ linux │
   └───────┘
   1 row
   ```

   The workspace lives under a per-worktree ref that does not take effect
   in a clone — by design, it is state that is *correct to lose* on
   transfer. The operational rule: **commit before you back up**; a backup
   is exactly as fresh as the last commit it contains.

2. **Rely only on `refs/heads/*` and `refs/tags/*` crossing the wire.**
   Everything durable in an acetone repository — commits, schema, data,
   conflict state — is reachable from branches and tags precisely so that
   backups need nothing else. This is a hard constraint, not a
   convention: git hosting proxies have been observed refusing
   (HTTP 403) pushes and fetches of custom ref namespaces outright.
   (That observation is recorded in the project's operational notes; we
   could not reproduce a hostile proxy in the lab, so this rule is
   documented from the field rather than driven here.)

3. **Local caches do not need backing up.** Files like
   `acetone-pack-bases` and `acetone-consolidation-packs` in the git
   directory are consolidation hints: losing them only makes the next
   `acetone gc` store more objects whole — never wrong, never lossy. The
   same goes for the deltas themselves: a backup taken after a stock
   `git gc` is bigger, not less correct.

And a closing symmetry worth knowing: `acetone fsck` runs identically on
the backup, so certify your backups the same way you certify a repair —

```console
$ cd /srv/registry-backup.git && acetone fsck
fsck: clean
```

— because the only backup that counts is one you have verified.
