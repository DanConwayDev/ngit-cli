//! Repo state snapshot for assertions.
//!
//! The relay-only PR only needs `HEAD` and refs. Working tree status,
//! config key sweeps, etc. accrue here as migrated tests demand them —
//! the explicit goal being that all common repo-state assertions go
//! through one diffable struct.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};

/// Refs and HEAD as of capture time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSnapshot {
    /// Symbolic-ref target of `HEAD` (e.g. `refs/heads/main`), or `None`
    /// for an unborn HEAD on a freshly-init'd repo.
    pub head: Option<String>,
    /// Map of ref name (e.g. `refs/heads/main`) to commit OID hex.
    pub refs: BTreeMap<String, String>,
}

impl RepoSnapshot {
    /// Read state from disk via `git2`. The repo must exist; non-existent
    /// HEAD (unborn branch) is normal and returned as `head: None`.
    pub fn capture(dir: &Path) -> Result<Self> {
        let repo = git2::Repository::open(dir)
            .with_context(|| format!("failed to open repo at {}", dir.display()))?;

        let head = match repo.head() {
            Ok(reference) => reference.name().map(|s| s.to_string()),
            Err(e)
                if e.code() == git2::ErrorCode::UnbornBranch
                    || e.code() == git2::ErrorCode::NotFound =>
            {
                None
            }
            Err(e) => return Err(anyhow::Error::from(e).context("failed to read HEAD")),
        };

        let mut refs = BTreeMap::new();
        for entry in repo.references().context("failed to iterate references")? {
            let r = entry.context("failed to read reference entry")?;
            // Symbolic refs (like HEAD itself) aren't useful here — we want
            // resolved OIDs only.
            if let Some(name) = r.name() {
                if let Some(oid) = r.target() {
                    refs.insert(name.to_string(), oid.to_string());
                }
            }
        }

        Ok(Self { head, refs })
    }
}
