//! Where a graph's refs live (ADR-0049).
//!
//! A [`GraphRefNamespace`] is the single source of truth mapping a graph's
//! *logical* refs — branch and tag short names — to the *physical* git ref
//! paths that hold them. A [`Repository`](crate::Repository) constructs one
//! at `init`/`open` and every `Repository`-borne ref-path site goes through
//! it, so a graph's layout is described in one value rather than scattered
//! across `format!("{prefix}{name}")` concatenations. (The one exception is
//! the store-level `fsck` scan, which runs repo-less — on a bare store with no
//! `Repository`, so it can check a repository whose workspace is damaged — and
//! so reads the standalone prefix constants directly.)
//!
//! Today the only layout is [`GraphRefNamespace::standalone`]: branches under
//! `refs/heads/*`, tags under `refs/tags/*`, exactly as acetone has always
//! stored them, so a fresh `git clone` still shows the graph on `main`. The
//! co-tenant layout — a graph namespaced under `refs/heads/acetone/<graph>/*`
//! alongside code in one repository — is added by `acetone-5w6`, which
//! constructs a different `GraphRefNamespace` at `open`. The ref-handling code
//! does not branch on mode; only this value differs (ADR-0049).

use crate::repo::{
    BRANCH_REF_PREFIX, GRAPHS_REF_PREFIX, TAG_REF_PREFIX, WORKTREE_ANCHOR_PREFIX,
    WORKTREE_GRAPH_ANCHOR_PREFIX, WORKTREE_MERGE_HEAD_REF, WORKTREE_WORKSPACE_REF,
};

/// The physical ref layout of one graph: where its branches and tags live and
/// which ref is its current-branch pointer.
///
/// Maps branch/tag short names to full git ref paths and back, and names the
/// graph's head pointer. Cheap to clone; held by a
/// [`Repository`](crate::Repository) for its lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRefNamespace {
    branch_prefix: String,
    tag_prefix: String,
    head_ref: String,
    /// Whether this layout owns the *whole* repository — true for standalone
    /// (the repo is the graph, so every ref, including `refs/remotes/*` and the
    /// like, is the graph's), false for co-tenant (only the graph's own refs
    /// are owned; the rest is the user's code). Drives [`Self::owns_ref`]'s
    /// treatment of refs outside branches/tags/HEAD.
    owns_whole_repo: bool,
    /// Ref *prefixes* (each ending in `/`) outside the branch/tag namespaces
    /// that this graph owns — co-tenant only (empty for standalone, which owns
    /// everything anyway): the graph's private `refs/acetone/<graph>/`
    /// namespace and the shared worktree anchors. An enumerated allow-list, so
    /// another graph's `refs/acetone/<other>/*` is foreign by construction
    /// (acetone-c2a) rather than swept up by a `refs/acetone/` catch-all.
    private_prefixes: Vec<String>,
    /// Exact full ref names outside the branch/tag namespaces that this graph
    /// owns — co-tenant only: its own marker `refs/acetone/graphs/<graph>`.
    /// Exact matches, so the marker of a prefix-confusable graph name
    /// (`g` vs `g2`) can never be claimed.
    private_refs: Vec<String>,
    /// Where an in-flight `acetone migrate` journals its planned ref swings
    /// (acetone-ejj). Local-only transient state — present exactly while a
    /// migration's ref swing is in flight, so its existence is the "migration
    /// interrupted" marker.
    migrate_journal_ref: String,
    /// This graph's per-worktree uncommitted-workspace ref (acetone-j6ui):
    /// standalone keeps the global `refs/worktree/acetone/workspace`;
    /// co-tenant is `refs/worktree/acetone/<graph>/workspace`, so two
    /// co-tenant graphs in one worktree no longer race one shared
    /// workspace. Disposable per-worktree state, so the split needs no
    /// format bump — reads fall back to the pre-split shared name
    /// ([`Self::legacy_workspace_ref`]), and the first write moves the
    /// workspace to the per-graph name (exactly as the pre-ADR-0014
    /// migration already does).
    workspace_ref: String,
    /// This graph's per-worktree merge-in-progress head, split from the
    /// global `refs/worktree/acetone/merge-head` for the same reason.
    merge_head_ref: String,
    /// The pre-split shared workspace/merge-head names a co-tenant graph
    /// may still find its state under (a repo written before the split):
    /// `Some((workspace, merge_head))` for co-tenant, `None` for
    /// standalone (whose names never changed).
    legacy_worktree_refs: Option<(String, String)>,
    /// The graph component of this graph's linked-worktree durability
    /// anchors (acetone-j6ui.4): `Some(graph)` for co-tenant — anchors keyed
    /// `refs/acetone/worktree-graph-anchors/<worktree>/<graph>` (a prefix of
    /// their own; a legacy flat anchor is a ref FILE git's D/F rule forbids
    /// writing beneath), so two co-tenant graphs saved from one linked
    /// worktree no longer clobber one shared anchor — `None` for standalone,
    /// which keeps the original flat `worktree-anchors/<worktree>` key (one
    /// graph; back-compatible by construction).
    anchor_graph: Option<String>,
}

impl GraphRefNamespace {
    /// The standalone layout: the repository *is* the graph. Branches under
    /// `refs/heads/*`, tags under `refs/tags/*` — the git-native namespaces,
    /// so the graph is visible to plain `git` out of the box — and the graph's
    /// current-branch pointer is git `HEAD`. The default for every repository
    /// today (ADR-0049).
    pub fn standalone() -> Self {
        GraphRefNamespace {
            branch_prefix: BRANCH_REF_PREFIX.to_owned(),
            tag_prefix: TAG_REF_PREFIX.to_owned(),
            head_ref: "HEAD".to_owned(),
            owns_whole_repo: true,
            private_prefixes: Vec::new(),
            private_refs: Vec::new(),
            migrate_journal_ref: "refs/acetone/migrate-journal".to_owned(),
            workspace_ref: WORKTREE_WORKSPACE_REF.to_owned(),
            merge_head_ref: WORKTREE_MERGE_HEAD_REF.to_owned(),
            legacy_worktree_refs: None,
            anchor_graph: None,
        }
    }

    /// The co-tenant layout (ADR-0050): a graph living inside a code
    /// repository, on its own ref namespace. Branches under
    /// `refs/heads/acetone/<graph>/*` (a proxy-safe subnamespace of
    /// `refs/heads`, distinct from the user's code branches), tags under
    /// `refs/tags/acetone/<graph>/*`, and the graph's current-branch pointer at
    /// `refs/acetone/<graph>/HEAD` — a local-only symref, so the shared git
    /// `HEAD` stays with the user's code checkout.
    ///
    /// **Precondition:** `graph` must be a single valid ref-path component — no
    /// empty string, `/`, `..`, or other characters git's ref-format rejects.
    /// This constructor does not validate it (it is infallible and builds ref
    /// *paths*); both callers that supply a graph name validate it first —
    /// `init_co_tenant` for the name it is given, and `detect_namespace` for
    /// the name recovered from a marker ref (acetone-c2a). Malformed names are
    /// still caught at the store door (`validated_ref_name`) before any ref
    /// write, so they cannot escape the ref namespace; the contract only keeps
    /// the failure close to its cause.
    pub fn co_tenant(graph: &str) -> Self {
        GraphRefNamespace {
            branch_prefix: format!("refs/heads/acetone/{graph}/"),
            tag_prefix: format!("refs/tags/acetone/{graph}/"),
            head_ref: format!("refs/acetone/{graph}/HEAD"),
            owns_whole_repo: false,
            private_prefixes: vec![
                // The graph's own private namespace: its head pointer and any
                // future per-graph state.
                format!("refs/acetone/{graph}/"),
                // Linked-worktree durability anchors (ADR-0044): since
                // acetone-j6ui.4 a co-tenant graph writes per-graph anchors
                // under worktree-graph-anchors/<worktree>/<graph> (its own
                // prefix — a legacy flat anchor is a ref FILE whose name
                // git's D/F rule would forbid writing beneath), so two
                // graphs saved from one linked worktree each keep their own
                // foreign-gc protection. The OLD flat prefix stays in the
                // allow-list: a legacy anchor from before the split is
                // deliberately left until its worktree disappears — the
                // other graph may not have re-saved since upgrading and may
                // still rely on its coverage; gc's staleness sweep and
                // fsck's coverage scan parse the worktree id as the FIRST
                // path component, which reads both key forms identically.
                WORKTREE_ANCHOR_PREFIX.to_owned(),
                WORKTREE_GRAPH_ANCHOR_PREFIX.to_owned(),
                // Per-worktree acetone state (`refs/worktree/acetone/*`:
                // workspace and merge refs, ADR-0014). Same single-graph
                // sharing caveat as the anchors. Without this, gc's
                // worktree enumeration (acetone-6g5.10) would classify a
                // linked worktree's acetone workspace as foreign — safe
                // (guarded, preserved in place) but never consolidated.
                "refs/worktree/acetone/".to_owned(),
            ],
            // The graph's own marker only — matched exactly, never by prefix,
            // so `g` cannot claim `g2`'s marker.
            private_refs: vec![format!("{GRAPHS_REF_PREFIX}{graph}")],
            // Inside the graph's private prefix, so ownership classification
            // (gc) covers it without a separate rule.
            migrate_journal_ref: format!("refs/acetone/{graph}/migrate-journal"),
            // Per-graph, still under the owned `refs/worktree/acetone/`
            // prefix so gc/ownership is unchanged (acetone-j6ui). The
            // legacy pair is the shared global names a single-graph
            // co-tenant repo written before the split still has its state
            // under — read as a fallback, superseded by the first
            // per-graph write.
            workspace_ref: format!("refs/worktree/acetone/{graph}/workspace"),
            merge_head_ref: format!("refs/worktree/acetone/{graph}/merge-head"),
            legacy_worktree_refs: Some((
                WORKTREE_WORKSPACE_REF.to_owned(),
                WORKTREE_MERGE_HEAD_REF.to_owned(),
            )),
            anchor_graph: Some(graph.to_owned()),
        }
    }

    /// The durability-anchor ref for linked worktree `worktree_id`
    /// (ADR-0044, acetone-j6ui.4): flat `<worktree>` for standalone,
    /// `<worktree>/<graph>` for co-tenant. Both prune-decision sites (gc
    /// staleness, fsck coverage) parse the worktree id back as the FIRST
    /// path component of the suffix, which reads both forms identically.
    pub fn worktree_anchor_ref(&self, worktree_id: &str) -> String {
        match &self.anchor_graph {
            // A prefix of its own: a legacy flat anchor is a ref FILE at
            // `<worktree>`, and git's D/F rule would forbid `<worktree>/…`
            // beneath it — separate namespaces coexist with no migration.
            Some(graph) => format!("{WORKTREE_GRAPH_ANCHOR_PREFIX}{worktree_id}/{graph}"),
            None => format!("{WORKTREE_ANCHOR_PREFIX}{worktree_id}"),
        }
    }

    /// The full ref path of branch `name` in this layout
    /// (e.g. `main` → `refs/heads/main`).
    pub fn branch_ref(&self, name: &str) -> String {
        format!("{}{name}", self.branch_prefix)
    }

    /// The branch short name of `full`, if `full` is a branch ref in this
    /// layout (the inverse of [`branch_ref`](Self::branch_ref)); `None`
    /// otherwise. Borrows from `full`.
    pub fn branch_name<'r>(&self, full: &'r str) -> Option<&'r str> {
        full.strip_prefix(&self.branch_prefix)
    }

    /// The full ref path of tag `name` in this layout
    /// (e.g. `v1` → `refs/tags/v1`).
    pub fn tag_ref(&self, name: &str) -> String {
        format!("{}{name}", self.tag_prefix)
    }

    /// The tag short name of `full`, if `full` is a tag ref in this layout;
    /// `None` otherwise. Borrows from `full`.
    pub fn tag_name<'r>(&self, full: &'r str) -> Option<&'r str> {
        full.strip_prefix(&self.tag_prefix)
    }

    /// The branch ref prefix, for listing/scanning a graph's branches
    /// (`RefStore::list_refs`) or matching them.
    pub fn branch_prefix(&self) -> &str {
        &self.branch_prefix
    }

    /// The tag ref prefix, for listing/scanning a graph's tags or matching
    /// them.
    pub fn tag_prefix(&self) -> &str {
        &self.tag_prefix
    }

    /// The graph's current-branch pointer ref: git `HEAD` in the standalone
    /// layout, or a private `refs/acetone/<graph>/HEAD` symref in the co-tenant
    /// layout. The store reads/sets/peels this pointer instead of assuming git
    /// `HEAD` (ADR-0050).
    pub fn head_ref(&self) -> &str {
        &self.head_ref
    }

    /// The ref where an in-flight `acetone migrate` journals its planned ref
    /// swings before performing them (acetone-ejj): a blob listing every
    /// `(ref, old, new)` swing. Present exactly while a swing is in flight —
    /// so a repository carrying this ref is a migration interrupted mid-swing,
    /// detectable and recoverable (`pending_migration`). Standalone:
    /// `refs/acetone/migrate-journal`; co-tenant: inside the graph's private
    /// `refs/acetone/<graph>/` namespace. Local-only; never pushed.
    pub fn migrate_journal_ref(&self) -> &str {
        &self.migrate_journal_ref
    }

    /// This graph's per-worktree uncommitted-workspace ref (acetone-j6ui):
    /// the global name for standalone, a per-graph name for co-tenant. The
    /// write path CAS-targets this; the read path additionally falls back
    /// to [`Self::legacy_workspace_ref`].
    pub fn workspace_ref(&self) -> &str {
        &self.workspace_ref
    }

    /// This graph's per-worktree merge-in-progress head ref.
    pub fn merge_head_ref(&self) -> &str {
        &self.merge_head_ref
    }

    /// The pre-split shared workspace ref a co-tenant graph may still find
    /// its workspace under (`None` for standalone). Read only as a
    /// fallback when [`Self::workspace_ref`] is absent.
    pub fn legacy_workspace_ref(&self) -> Option<&str> {
        self.legacy_worktree_refs.as_ref().map(|(w, _)| w.as_str())
    }

    /// The pre-split shared merge-head ref (`None` for standalone).
    pub fn legacy_merge_head_ref(&self) -> Option<&str> {
        self.legacy_worktree_refs.as_ref().map(|(_, m)| m.as_str())
    }

    /// Whether the ref `full` (a full name, e.g. `refs/heads/main`) belongs to
    /// this graph — the ownership test `gc` uses to decide what it may repack
    /// (ADR-0051 reading B). A ref under `refs/heads/` or `refs/tags/` is the
    /// graph's only if it sits under this namespace's branch/tag prefix, so a
    /// co-tenant's *code* branches and tags are foreign; git `HEAD` is the
    /// graph's only in the standalone layout (co-tenant leaves git `HEAD` to the
    /// code checkout).
    ///
    /// Refs of any *other* shape — `refs/remotes/*`, `refs/notes/*`,
    /// `refs/stash`, `refs/replace/*` — are handled by layout: in **standalone**
    /// the repo *is* the graph, so they are the graph's (and consolidation is
    /// byte-identical to before graph-scoping existed — the guard is empty). In
    /// **co-tenant** ownership is an enumerated allow-list — the graph's own
    /// `refs/acetone/<graph>/*` namespace, the worktree anchors and its own
    /// marker — and everything else is foreign and guarded: the user's
    /// remote-tracking refs, notes and stash, but also any *other* graph's
    /// `refs/acetone/<other>/*` refs and marker (acetone-c2a), so a future
    /// multi-graph repository can never have one graph gc another's objects.
    /// Foreign is the safe default: a guarded ref's objects are preserved
    /// untouched, so misclassifying towards "foreign" keeps data. Getting
    /// `refs/remotes/*` wrong would draw a cloned repo's code objects into
    /// acetone's pack — exactly what reading B forbids.
    pub fn owns_ref(&self, full: &str) -> bool {
        if full.starts_with("refs/heads/") {
            return full.starts_with(&self.branch_prefix);
        }
        if full.starts_with("refs/tags/") {
            return full.starts_with(&self.tag_prefix);
        }
        if full == "HEAD" {
            return self.head_ref == "HEAD";
        }
        // Any other ref shape: standalone owns the whole repo; co-tenant owns
        // only the enumerated acetone refs that are demonstrably this graph's,
        // and guards the rest.
        self.owns_whole_repo
            || self.private_prefixes.iter().any(|p| full.starts_with(p))
            || self.private_refs.iter().any(|r| full == r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_uses_git_native_prefixes() {
        let ns = GraphRefNamespace::standalone();
        assert_eq!(ns.branch_prefix(), "refs/heads/");
        assert_eq!(ns.tag_prefix(), "refs/tags/");
        assert_eq!(ns.head_ref(), "HEAD");
    }

    #[test]
    fn co_tenant_namespaces_under_acetone() {
        let ns = GraphRefNamespace::co_tenant("g");
        assert_eq!(ns.branch_prefix(), "refs/heads/acetone/g/");
        assert_eq!(ns.tag_prefix(), "refs/tags/acetone/g/");
        // The head pointer is a private ref, NOT git HEAD, so the user's HEAD
        // stays with their code checkout.
        assert_eq!(ns.head_ref(), "refs/acetone/g/HEAD");
        // Branch mapping still round-trips under the co-tenant prefix.
        assert_eq!(ns.branch_ref("main"), "refs/heads/acetone/g/main");
        assert_eq!(ns.branch_name(&ns.branch_ref("main")), Some("main"));
        // A user's plain code branch is NOT a graph branch in this layout.
        assert_eq!(ns.branch_name("refs/heads/main"), None);
    }

    #[test]
    fn branch_ref_prepends_the_prefix() {
        let ns = GraphRefNamespace::standalone();
        assert_eq!(ns.branch_ref("main"), "refs/heads/main");
        assert_eq!(ns.tag_ref("v1"), "refs/tags/v1");
    }

    #[test]
    fn standalone_owns_every_ref() {
        // In the standalone layout there is no foreign ref: gc's guard set is
        // empty, so consolidation packs the whole reachable set as before —
        // including a standalone graph that has been pushed/cloned and so has
        // remote-tracking, notes or stash refs.
        let ns = GraphRefNamespace::standalone();
        for r in [
            "refs/heads/main",
            "refs/heads/acetone/g/main",
            "refs/tags/v1",
            "HEAD",
            "refs/acetone/worktree-anchors/abc",
            "refs/acetone/g/HEAD",
            "refs/remotes/origin/main",
            "refs/notes/commits",
            "refs/stash",
            "refs/replace/0123456789abcdef0123456789abcdef01234567",
        ] {
            assert!(ns.owns_ref(r), "standalone should own {r}");
        }
    }

    #[test]
    fn co_tenant_owns_only_its_own_refs() {
        let ns = GraphRefNamespace::co_tenant("g");
        // The graph's own refs — packable: its branches and tags, its private
        // `refs/acetone/g/*` namespace (head pointer), its marker, and the
        // shared worktree anchors (whose trees mirror this graph's workspace
        // while co-tenancy is single-graph).
        for r in [
            "refs/heads/acetone/g/main",
            "refs/tags/acetone/g/v1",
            "refs/acetone/g/HEAD",
            "refs/acetone/graphs/g",
            "refs/acetone/worktree-anchors/abc",
            "refs/acetone/worktree-graph-anchors/abc/g",
        ] {
            assert!(ns.owns_ref(r), "co-tenant should own {r}");
        }
        // The user's code refs, git HEAD, AND the other ref shapes a real
        // (usually cloned) code repo carries — remote-tracking, notes, stash,
        // replace — are foreign: the prune guard. Owning any of these would
        // draw the user's code objects into acetone's pack (reading A).
        for r in [
            "refs/heads/main",
            "refs/heads/feature/x",
            "refs/tags/v1.0",
            "HEAD",
            "refs/remotes/origin/main",
            "refs/remotes/upstream/release",
            "refs/notes/commits",
            "refs/stash",
            "refs/replace/0123456789abcdef0123456789abcdef01234567",
        ] {
            assert!(!ns.owns_ref(r), "co-tenant must not own {r}");
        }
    }

    #[test]
    fn co_tenant_does_not_own_another_graphs_refs() {
        // Multi-graph hardening (acetone-c2a): in a repository that hosted a
        // second graph, that graph's private refs must be FOREIGN to this one —
        // landing in gc's prune guard (kept untouched), never drawn into this
        // graph's pack. Unreachable today (open rejects multiple markers), but
        // the classification must already be right so multi-graph co-tenancy
        // cannot silently inherit a cross-graph gc.
        let ns = GraphRefNamespace::co_tenant("g");
        for r in [
            // Another graph's namespace, including the prefix-confusable "g2".
            "refs/acetone/other/HEAD",
            "refs/acetone/g2/HEAD",
            "refs/acetone/g2/state",
            // Another graph's marker ("g" must not prefix-match "g2"/"gx").
            "refs/acetone/graphs/other",
            "refs/acetone/graphs/g2",
            // Another graph's branches and tags.
            "refs/heads/acetone/g2/main",
            "refs/tags/acetone/g2/v1",
            // An unknown refs/acetone/* shape: foreign by default — when in
            // doubt gc keeps data it does not positively own.
            "refs/acetone/unknown-future-namespace/x",
        ] {
            assert!(!ns.owns_ref(r), "co-tenant graph g must not own {r}");
        }
    }

    #[test]
    fn migrate_journal_ref_is_private_and_owned() {
        let ns = GraphRefNamespace::standalone();
        assert_eq!(ns.migrate_journal_ref(), "refs/acetone/migrate-journal");
        assert!(ns.owns_ref(ns.migrate_journal_ref()));

        let ns = GraphRefNamespace::co_tenant("g");
        assert_eq!(ns.migrate_journal_ref(), "refs/acetone/g/migrate-journal");
        // Inside the graph's private prefix, so gc's ownership classification
        // covers it without a separate rule — and another graph's journal is
        // foreign.
        assert!(ns.owns_ref(ns.migrate_journal_ref()));
        assert!(!ns.owns_ref("refs/acetone/g2/migrate-journal"));
    }

    #[test]
    fn branch_name_inverts_branch_ref() {
        let ns = GraphRefNamespace::standalone();
        for name in ["main", "feature/x", "acetone/g/main"] {
            assert_eq!(ns.branch_name(&ns.branch_ref(name)), Some(name));
            assert_eq!(ns.tag_name(&ns.tag_ref(name)), Some(name));
        }
    }

    #[test]
    fn branch_name_rejects_non_branch_refs() {
        let ns = GraphRefNamespace::standalone();
        // A tag ref is not a branch, and vice versa.
        assert_eq!(ns.branch_name("refs/tags/v1"), None);
        assert_eq!(ns.tag_name("refs/heads/main"), None);
        // An acetone-private ref is neither.
        assert_eq!(ns.branch_name("refs/acetone/workspaces/default"), None);
        // The prefix itself with no name still round-trips as the empty name;
        // callers never pass empty names, but the mapping stays total.
        assert_eq!(ns.branch_name("refs/heads/"), Some(""));
    }
}
