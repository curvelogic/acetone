//! The CLI's authorship rule (acetone-gid, ADR-0079): an explicit
//! `--author 'Name <email>'` wins; otherwise `GIT_AUTHOR_NAME` and
//! `GIT_AUTHOR_EMAIL` from the environment; otherwise `None`, which the
//! library stamps as its neutral signature.
//!
//! The environment fallback is the **CLI's** alone: the person running the
//! command is the author. The daemon never calls this — a daemon serves many
//! clients, and its launcher's identity must not be stamped on their commits.
//! Git config (global or repository-local) is deliberately not read: the
//! store opens repositories isolated (ADR-0034), and trusting a config file
//! for identity is a separate decision, deferred.
//!
//! Every identity is checked by the store's own rule
//! ([`Signature::validate`]), never a parallel validator.

use acetone_core::store::Signature;
use anyhow::{Context, Result, bail};

/// The author for a CLI write: `--author` if given, else the
/// `GIT_AUTHOR_*` environment, else `None` (neutral).
pub fn resolve(flag: Option<&str>) -> Result<Option<Signature>> {
    resolve_with(flag, |name| std::env::var(name).ok())
}

/// [`resolve`] over an explicit environment lookup, so tests need not mutate
/// the process environment.
pub fn resolve_with(
    flag: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Signature>> {
    if let Some(text) = flag {
        let sig = Signature::parse(text).with_context(|| format!("--author {text:?}"))?;
        return Ok(Some(sig));
    }
    // An empty variable counts as unset (the shell idiom `VAR= cmd`).
    let get = |name: &str| env(name).filter(|v| !v.is_empty());
    match (get("GIT_AUTHOR_NAME"), get("GIT_AUTHOR_EMAIL")) {
        (None, None) => Ok(None),
        (Some(name), Some(email)) => {
            let sig = Signature { name, email };
            sig.validate()
                .context("GIT_AUTHOR_NAME/GIT_AUTHOR_EMAIL from the environment")?;
            Ok(Some(sig))
        }
        // Half an identity is refused rather than completed with a guess.
        (Some(_), None) => bail!(
            "GIT_AUTHOR_NAME is set but GIT_AUTHOR_EMAIL is not: set both, \
             or pass --author 'Name <email>'"
        ),
        (None, Some(_)) => bail!(
            "GIT_AUTHOR_EMAIL is set but GIT_AUTHOR_NAME is not: set both, \
             or pass --author 'Name <email>'"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn flag_wins_over_environment() {
        let sig = resolve_with(
            Some("Ada <ada@example.com>"),
            env(&[("GIT_AUTHOR_NAME", "Env"), ("GIT_AUTHOR_EMAIL", "e@x")]),
        )
        .expect("ok")
        .expect("some");
        assert_eq!(sig.name, "Ada");
        assert_eq!(sig.email, "ada@example.com");
    }

    #[test]
    fn environment_is_the_fallback() {
        let sig = resolve_with(
            None,
            env(&[
                ("GIT_AUTHOR_NAME", "Env Person"),
                ("GIT_AUTHOR_EMAIL", "e@x"),
            ]),
        )
        .expect("ok")
        .expect("some");
        assert_eq!(sig.name, "Env Person");
        assert_eq!(sig.email, "e@x");
    }

    #[test]
    fn nothing_set_is_neutral() {
        assert!(resolve_with(None, env(&[])).expect("ok").is_none());
        assert!(
            resolve_with(
                None,
                env(&[("GIT_AUTHOR_NAME", ""), ("GIT_AUTHOR_EMAIL", "")])
            )
            .expect("ok")
            .is_none()
        );
    }

    #[test]
    fn half_an_identity_is_refused() {
        assert!(resolve_with(None, env(&[("GIT_AUTHOR_NAME", "A")])).is_err());
        assert!(resolve_with(None, env(&[("GIT_AUTHOR_EMAIL", "a@b")])).is_err());
    }

    #[test]
    fn injected_identities_are_refused() {
        assert!(resolve_with(Some("Eve <e@x>\ncommitter M <m@x>"), env(&[])).is_err());
        assert!(
            resolve_with(
                None,
                env(&[
                    ("GIT_AUTHOR_NAME", "Eve"),
                    ("GIT_AUTHOR_EMAIL", "e@x>\ncommitter M <m@x")
                ])
            )
            .is_err()
        );
    }
}
