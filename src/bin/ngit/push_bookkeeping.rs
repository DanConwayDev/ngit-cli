//! Local bookkeeping that `git push` would have performed.
//!
//! When ngit pushes git data in-process (through the state transaction)
//! instead of shelling out to `git push`, git's own post-push side
//! effects never happen. This module replicates the two that matter,
//! with git's semantics:
//!
//! - remote-tracking-ref maintenance: a successful branch push updates
//!   `refs/remotes/<remote>/<branch>` (reflog message `update by push`,
//!   matching git) and a successful `:refs/heads/<branch>` deletion removes it.
//!   Tags are untouched — git does not track tags per-remote.
//! - upstream configuration, replacing `git push -u`: `branch.<branch>.remote`
//!   + `branch.<branch>.merge`.
//!
//! The remote helper binary keeps its own equivalent
//! (`git_remote_helper::push::update_remote_refs_pushed`).
//! Converging the two was attempted and abandoned: they run in
//! different process contexts with different jobs. This module is the
//! *only* writer after an in-process push and must replicate git's
//! semantics exactly (hence the `update by push` reflog message). The
//! helper's runs where git itself also updates the remote-tracking
//! refs for every refspec it reported `ok` — its writes are a
//! belt-and-braces mirror plus work git will never do: deleting legacy
//! tag tracking refs written by old ngit versions, and resolving the
//! remote name from the nostr URL when the helper wasn't given one.
//! Merging them would either change the helper's observable reflog
//! messages and drop its URL-based remote resolution, or burden this
//! module with legacy cleanup it cannot need. The refspec parsing they
//! share is trivial (`split_refspec` here, `refspec_to_from_to`
//! there).

use anyhow::{Context, Result, bail};
use ngit::git::{Repo, RepoActions, sha1_to_oid};

/// Reflog message git writes when `git push` updates a remote-tracking
/// ref.
const PUSH_REFLOG_MESSAGE: &str = "update by push";

/// Replicate `git push`'s local bookkeeping for branch `refspecs` the
/// server accepted: update `refs/remotes/<remote_name>/<branch>` for
/// each branch push and delete it for each branch deletion
/// (`:refs/heads/<branch>`). Non-branch refspecs (tags, oid-only
/// destinations) are ignored.
///
/// Callers must only pass refspecs the server actually accepted; after
/// a partial failure the rejected refspecs must be excluded so the
/// tracking refs keep describing what the server holds.
pub fn record_accepted_push_refspecs(
    git_repo: &Repo,
    remote_name: &str,
    refspecs: &[String],
) -> Result<()> {
    for refspec in refspecs {
        let (from, to) = split_refspec(refspec)?;
        let Some(branch) = to.strip_prefix("refs/heads/") else {
            continue;
        };
        let tracking_ref = format!("refs/remotes/{remote_name}/{branch}");
        if from.is_empty() {
            if let Ok(mut reference) = git_repo.git_repo.find_reference(&tracking_ref) {
                reference
                    .delete()
                    .with_context(|| format!("failed to delete tracking ref {tracking_ref}"))?;
            }
        } else {
            let tip = git_repo
                .get_commit_or_tip_of_reference(from)
                .with_context(|| format!("failed to resolve pushed ref {from} to a commit"))?;
            let oid = sha1_to_oid(&tip)?;
            git_repo
                .git_repo
                .reference(&tracking_ref, oid, true, PUSH_REFLOG_MESSAGE)
                .with_context(|| format!("failed to update tracking ref {tracking_ref}"))?;
        }
    }
    Ok(())
}

/// Replicate `git push -u <remote_name> <branch>`'s upstream setup:
/// record `remote_name` as the upstream of the local branch `branch`.
pub fn set_branch_upstream(git_repo: &Repo, remote_name: &str, branch: &str) -> Result<()> {
    git_repo
        .save_git_config_item(&format!("branch.{branch}.remote"), remote_name, false)
        .context("failed to set upstream remote in git config")?;
    git_repo
        .save_git_config_item(
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
            false,
        )
        .context("failed to set upstream merge ref in git config")?;
    Ok(())
}

/// Split a refspec into its (source, destination) halves, stripping a
/// leading force marker from the source.
fn split_refspec(refspec: &str) -> Result<(&str, &str)> {
    let Some((from, to)) = refspec.split_once(':') else {
        bail!("refspec should contain a colon (:) but consists of: {refspec}");
    };
    Ok((from.strip_prefix('+').unwrap_or(from), to))
}

#[cfg(test)]
mod tests {
    use std::{
        env::current_dir,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use git2::{RepositoryInitOptions, Signature, Time};

    use super::*;

    /// Minimal in-process git repository fixture, following the pattern
    /// used by `sub_commands::sync`'s unit tests (the binary crate can't
    /// see the lib's `#[cfg(test)]` helpers).
    struct TestRepo {
        dir: PathBuf,
        repo: Repo,
    }

    impl TestRepo {
        fn new() -> TestRepo {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = current_dir()
                .unwrap()
                .join(format!("tmpgit-bookkeeping-{}-{n}", std::process::id()));
            let git_repo = git2::Repository::init_opts(
                &dir,
                RepositoryInitOptions::new()
                    .initial_head("main")
                    .mkpath(true),
            )
            .unwrap();
            let sig = Signature::new("Joe Bloggs", "joe.bloggs@pm.me", &Time::new(0, 0)).unwrap();
            let tree_oid = git_repo.index().unwrap().write_tree().unwrap();
            let tree = git_repo.find_tree(tree_oid).unwrap();
            git_repo
                .commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
                .unwrap();
            drop(tree);
            drop(git_repo);
            let repo = Repo::from_path(&dir).unwrap();
            TestRepo { dir, repo }
        }

        fn head_oid(&self) -> git2::Oid {
            self.repo
                .git_repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn branch_push_creates_tracking_ref_with_push_reflog_message() {
        let t = TestRepo::new();
        record_accepted_push_refspecs(
            &t.repo,
            "origin",
            &["refs/heads/main:refs/heads/main".to_string()],
        )
        .unwrap();

        let tracking = t
            .repo
            .git_repo
            .find_reference("refs/remotes/origin/main")
            .unwrap();
        assert_eq!(tracking.target().unwrap(), t.head_oid());

        let reflog = t.repo.git_repo.reflog("refs/remotes/origin/main").unwrap();
        let last = reflog.get(0).unwrap();
        assert_eq!(last.message().unwrap(), Some(PUSH_REFLOG_MESSAGE));
    }

    #[test]
    fn forced_refspec_updates_existing_tracking_ref() {
        let t = TestRepo::new();
        let stale = t.head_oid();
        t.repo
            .git_repo
            .reference("refs/remotes/origin/main", stale, true, "seed")
            .unwrap();
        // move main forward
        let sig = Signature::new("Joe Bloggs", "joe.bloggs@pm.me", &Time::new(0, 0)).unwrap();
        let parent = t.repo.git_repo.head().unwrap().peel_to_commit().unwrap();
        let tree = parent.tree().unwrap();
        t.repo
            .git_repo
            .commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        record_accepted_push_refspecs(
            &t.repo,
            "origin",
            &["+refs/heads/main:refs/heads/main".to_string()],
        )
        .unwrap();

        assert_eq!(
            t.repo
                .git_repo
                .find_reference("refs/remotes/origin/main")
                .unwrap()
                .target()
                .unwrap(),
            t.head_oid(),
        );
    }

    #[test]
    fn accepted_deletion_removes_tracking_ref() {
        let t = TestRepo::new();
        t.repo
            .git_repo
            .reference("refs/remotes/origin/gone", t.head_oid(), true, "seed")
            .unwrap();

        record_accepted_push_refspecs(&t.repo, "origin", &[":refs/heads/gone".to_string()])
            .unwrap();

        assert!(
            t.repo
                .git_repo
                .find_reference("refs/remotes/origin/gone")
                .is_err()
        );
    }

    #[test]
    fn deletion_of_absent_tracking_ref_is_a_no_op() {
        let t = TestRepo::new();
        record_accepted_push_refspecs(
            &t.repo,
            "origin",
            &[":refs/heads/never-existed".to_string()],
        )
        .unwrap();
    }

    #[test]
    fn tag_refspec_writes_no_tracking_ref() {
        let t = TestRepo::new();
        t.repo
            .git_repo
            .reference("refs/tags/v1.0.0", t.head_oid(), true, "tag")
            .unwrap();

        record_accepted_push_refspecs(
            &t.repo,
            "origin",
            &["refs/tags/v1.0.0:refs/tags/v1.0.0".to_string()],
        )
        .unwrap();

        assert!(
            t.repo
                .git_repo
                .find_reference("refs/remotes/origin/v1.0.0")
                .is_err(),
            "tags must not be recorded in the remote-tracking branch namespace"
        );
    }

    #[test]
    fn upstream_config_matches_git_push_u() {
        let t = TestRepo::new();
        set_branch_upstream(&t.repo, "origin", "main").unwrap();

        let config = t.repo.git_repo.config().unwrap();
        assert_eq!(
            config.get_string("branch.main.remote").unwrap(),
            "origin".to_string()
        );
        assert_eq!(
            config.get_string("branch.main.merge").unwrap(),
            "refs/heads/main".to_string()
        );
    }

    #[test]
    fn refspec_without_colon_errors() {
        let t = TestRepo::new();
        assert!(
            record_accepted_push_refspecs(&t.repo, "origin", &["refs/heads/main".to_string()])
                .is_err()
        );
    }
}
