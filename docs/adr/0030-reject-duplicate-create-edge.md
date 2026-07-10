# ADR-0030: `CREATE` of a duplicate/parallel edge is rejected in v0.1

*Status: accepted — decided by ADR under the Autonomous Protocol (pre-0.1
hardening sprint); flagged in the pre-0.1 review report for Greg's retrospective
review · Date: 2026-07-11 · Bead: acetone-8yn*

## Context

The pre-0.1 review found (U11) that the Cypher write path **silently upserts on
`CREATE` of an existing edge, and collapses parallel `CREATE`s**. An edge's
storage key is `(src, type, dst, discriminator)`; the discriminator is a
first-class field of the frozen format, but the query surface always writes it
as `Null` and offers no way to set it. So two relationships with the same
`(src, type, dst)` share one key.

`persist_changes` put every relationship in `WriteChanges` blindly
(`txn.put_edge` overwrites). That merged set contained both `CREATE`/`MERGE`-
created edges and `SET`-modified edges. The result:

- `CREATE (a)-[:R]->(b)` when that edge already exists **overwrote** the existing
  edge's properties silently — data loss.
- `CREATE (a)-[:R]->(b), (a)-[:R]->(b)` (two parallel edges) **collapsed** to one,
  the second overwriting the first.

openCypher's `CREATE` always creates a *new* relationship, so two `CREATE`s of
the same pattern produce two (parallel) relationships. acetone cannot represent
that without a distinct, query-reachable discriminator — which v0.1 does not
have (stable relationship identity is the Phase-7 item `acetone-rid`).

## Decision

**A `CREATE` that would produce an edge whose key already exists is rejected**
(`PersistError::DuplicateEdge`), rather than silently overwriting or collapsing.
"Already exists" means: present in the base graph (and not deleted in the same
statement), or duplicated by another `CREATE` in the same statement.

To distinguish the cases, `WriteChanges` now carries relationships in two lists:

- **`created_rels`** — `CREATE`, or a `MERGE` that did not match. These are new
  edges; `persist_changes` rejects one whose key already exists.
- **`modified_rels`** — `SET` on a matched edge. Their keys already exist by
  definition; they are updates and are always put.

A `MERGE` that *matched* an existing edge never reaches persistence as a create
(it binds the existing relationship), so **`MERGE` upsert semantics are
unchanged** — `MERGE (a)-[:R]->(b)` on an existing edge is a no-op/`ON MATCH`,
and on an absent edge creates it. `SET r.x = …` on a matched edge continues to
update it.

## Consequences

- `CREATE` of a duplicate edge, and parallel-edge `CREATE`, now fail with a
  clear error pointing at `MERGE` (to upsert) or `SET` (to modify), instead of
  silent data loss.
- **Parallel edges are not supported in v0.1.** A model that genuinely needs two
  relationships of the same type between the same pair must wait for
  query-reachable discriminators / stable relationship identity (`acetone-rid`,
  Phase 7). The on-disk format already reserves the discriminator, so this is a
  query-surface limitation, not a format one — no migration when it lands.
- **Cost:** the created-edge collision check scans the base edge set once per
  statement that contains a `CREATE` (to build the existing-key set). At
  workbench scale (spec §1) this is acceptable; a targeted store-backed edge
  existence check is a follow-up alongside the lazy read path (`acetone-cbl.11`).
- No on-disk format change. Relates to `acetone-rid` (the root-cause fix) and
  the U11 finding.
