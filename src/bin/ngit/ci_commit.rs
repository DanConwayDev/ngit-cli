//! The NIP's rule for CI `c` values, enforced in one place: every `c` names
//! one commit — the peeled commit first, then the annotated tag object id
//! when the commit-ish was one — and every id peels to that commit.
//!
//! Publisher-side strictness and consumer-side tolerance are named modes
//! over the same resolution, not separate code paths: a publisher must not
//! sign an event whose `c` values disagree, so it errors; a consumer
//! resolving a target or reading a published event skips what it cannot
//! resolve.

use anyhow::{Context, Result, bail};

use crate::git::Repo;

/// The `c` tag values for a commit-ish.
pub struct ResolvedCommit {
    /// The peeled commit id.
    pub commit: String,
    /// Every published `c` value: the peeled commit first, then the annotated
    /// tag object id when the commit-ish was one.
    pub ids: Vec<String>,
}

/// Publisher mode: resolve `commit_ish`, peel-verifying every `c` value it
/// produces.
///
/// # Errors
///
/// Returns an error when `commit_ish` does not name a commit in this
/// repository, or when a derived `c` value does not peel back to it.
pub fn resolve_commit_ish_strict(git_repo: &Repo, commit_ish: &str) -> Result<ResolvedCommit> {
    let object = git_repo
        .git_repo
        .revparse_single(commit_ish)
        .with_context(|| format!("`{commit_ish}` is not a commit-ish in this repository"))?;
    let commit = object
        .peel_to_commit()
        .with_context(|| format!("`{commit_ish}` does not resolve to a commit"))?;

    let mut ids = vec![commit.id().to_string()];
    if object.id() != commit.id() {
        // An annotated tag: the NIP has the tag object published as a further
        // `c` so one `#c` query finds the run from either spelling.
        ids.push(object.id().to_string());
    }

    // A coordinator ignores a request whose `c` objects do not all peel to
    // one commit, so ngit checks what it is about to publish rather than
    // relying on how the ids were derived.
    for id in &ids {
        let peeled = git_repo
            .git_repo
            .find_object(git2::Oid::from_str(id)?, None)
            .with_context(|| format!("object {id} is not in this repository"))?
            .peel_to_commit()
            .with_context(|| format!("object {id} does not peel to a commit"))?;
        if peeled.id() != commit.id() {
            bail!(
                "object {id} peels to commit {} but {commit_ish} resolves to {}; every `c` tag must name the same commit",
                peeled.id(),
                commit.id(),
            );
        }
    }

    Ok(ResolvedCommit {
        commit: commit.id().to_string(),
        ids,
    })
}

/// Consumer mode: the same resolution, skipping a commit-ish that does not
/// resolve instead of failing the surface reading it.
#[must_use]
pub fn resolve_commit_ish_tolerant(git_repo: &Repo, commit_ish: &str) -> Option<ResolvedCommit> {
    resolve_commit_ish_strict(git_repo, commit_ish).ok()
}

/// Consumer mode over *published* `c` values: the first id ngit holds
/// locally that peels to a commit. Every value is tried, whatever order the
/// publisher used — the NIP puts the commit first, but an annotated-tag run
/// also names the tag object, and a publisher that ordered them the other
/// way round still describes the same commit.
#[must_use]
pub fn first_local_commit<'r>(git_repo: &'r Repo, ids: &[String]) -> Option<git2::Commit<'r>> {
    ids.iter()
        .filter_map(|candidate| git2::Oid::from_str(candidate).ok())
        .filter_map(|oid| git_repo.git_repo.find_object(oid, None).ok())
        .find_map(|object| object.peel_to_commit().ok())
}
