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

use crate::repo::{BRANCH_REF_PREFIX, TAG_REF_PREFIX};

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
        }
    }

    /// The co-tenant layout (ADR-0050): a graph living inside a code
    /// repository, on its own ref namespace. Branches under
    /// `refs/heads/acetone/<graph>/*` (a proxy-safe subnamespace of
    /// `refs/heads`, distinct from the user's code branches), tags under
    /// `refs/tags/acetone/<graph>/*`, and the graph's current-branch pointer at
    /// `refs/acetone/<graph>/HEAD` — a local-only symref, so the shared git
    /// `HEAD` stays with the user's code checkout.
    pub fn co_tenant(graph: &str) -> Self {
        GraphRefNamespace {
            branch_prefix: format!("refs/heads/acetone/{graph}/"),
            tag_prefix: format!("refs/tags/acetone/{graph}/"),
            head_ref: format!("refs/acetone/{graph}/HEAD"),
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
