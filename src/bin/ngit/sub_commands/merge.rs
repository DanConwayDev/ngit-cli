use std::path::Path;

use anyhow::{Context, Result, bail};
use ngit::{
    ci::trust::Coverage,
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache, get_state_from_cache,
    },
    fetch::ensure_commit_local,
    git_events::{
        KIND_COVER_NOTE, KIND_LABEL, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_commit_id_from_patch, get_pr_tip_event_or_most_recent_patch_with_ancestors,
        is_event_proposal_root_for_branch, pr_event_clone_tag_urls, process_cover_note,
        process_subject, tag_value,
    },
    git_http_auth::prepare_private_git_auth_for_repo,
    login::{get_curent_user, user::extract_user_metadata},
    proposal_base::resolve_target_branch_tip_with_known_tip,
    utils::get_open_or_draft_proposals,
};
use nostr::prelude::{EventId, PublicKey, RelayUrl, ToBech32, nip19::Nip19Event};

use crate::{
    ci_projection::{ProjectionRequest, Tier, build_report, pull_request_target, relay_coverage},
    cli::{CiTrustFloor, SignerParams},
    client::{
        Client, Connect, get_events_from_local_cache, get_repo_ref_from_cache,
        warn_if_invited_as_maintainer,
    },
    git::{Repo, RepoActions, str_to_sha1},
    git_events::event_to_cover_letter,
    repo_ref::{RepoRef, get_repo_coordinates_when_remote_unknown},
    sub_commands::{
        id_resolver::{pr_description, proposal_roots, resolve_pr_root_id_or_prefix},
        repository_fetch::fetching_with_account,
    },
};

#[allow(clippy::too_many_lines)]
pub async fn launch(
    id: Option<&str>,
    offline: bool,
    require_ci_trust: Option<CiTrustFloor>,
    exclude_description: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let mut repo_coordinates =
        get_repo_coordinates_when_remote_unknown(&git_repo, &mut client).await?;

    let fetch_report = if offline {
        None
    } else {
        Some(
            fetching_with_account(
                &git_repo,
                git_repo_path,
                &mut client,
                &mut repo_coordinates,
                auth,
            )
            .await?,
        )
    };

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;
    let private_signer =
        prepare_private_git_auth_for_repo(&repo_ref, &git_repo, auth.info, auth.password).await?;

    let proposals_and_revisions =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    // Resolve which proposal to merge: either the explicit id, or — when no id
    // is given — the proposal encoded in the checked-out `pr/...` branch name.
    let event_id = if let Some(id) = id {
        resolve_pr_root_id_or_prefix(id, proposals_and_revisions.iter(), pr_description)?
    } else {
        resolve_event_id_from_current_branch(&git_repo, &repo_ref, &proposals_and_revisions).await?
    };

    let proposal = proposal_roots(&proposals_and_revisions)
        .find(|e| e.id == event_id)
        .context(format!(
            "PR with id {} not found in cache",
            event_id.to_hex()
        ))?
        .clone();

    // Evaluate a requested gate before creating branches or touching HEAD,
    // the index, or the working tree.
    let gated_ci = if let Some(floor) = require_ci_trust {
        let input_coverage = fetch_report.as_ref().map_or(Coverage::Complete, |report| {
            relay_coverage(&repo_ref.relays, &report.state_per_relay)
        });
        let target = pull_request_target(git_repo_path, &repo_ref, proposal.id).await?;
        let ci = build_report(
            &git_repo,
            git_repo_path,
            &repo_ref,
            &client,
            &ProjectionRequest {
                target: &target,
                tier: if offline { Tier::Cache } else { Tier::Full },
                include_outdated: false,
                input_coverage,
            },
        )
        .await?;
        if ci.has_results() {
            ci.print_checks();
        }
        if let Some(reason) = ci.gate_failure(floor) {
            if crate::output::is_json() {
                crate::output::set_value(super::pr_merge::merge_json(
                    proposal.id,
                    repo_ref.relays.first(),
                    None,
                    &ci,
                    None,
                    Some(&reason),
                ));
            }
            println!("{}", console::style(&reason).red());
            crate::output::finish_and_exit(1);
        }
        Some(ci)
    } else {
        None
    };

    let cover_letter = event_to_cover_letter(&proposal).context("failed to extract PR details")?;
    // Canonical branch name created by `ngit pr checkout`:
    // `pr/<name>(<8-hex-of-event-id>)`.
    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
    // Bare branch name a self-submitting author pushes by hand
    // (`git push -u origin pr/<name>`); it carries no shorthand. The same
    // mapping `git-remote-nostr` and `resolve_event_id_from_current_branch`
    // use to link such a branch back to its published PR.
    let bare_branch_name = format!("pr/{}", cover_letter.branch_name_without_id_or_prefix);

    // Find the PR tip commit.
    let commits_events = get_all_proposal_patch_pr_pr_update_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal.id,
    )
    .await?;

    let tip_chain = get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events)
        .context("failed to find any PR or patch events on this proposal")?;

    let tip_event = tip_chain.first().context("tip chain is empty")?;
    let is_pr_kind = tip_chain
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind));

    let tip_commit_str = published_tip_commit_str(&tip_chain)?;

    // Determine which local branch represents this PR. `ngit pr checkout`
    // creates the canonical `pr/<name>(<shorthand>)` form, but a self-
    // submitting author who pushed `pr/<name>` by hand has only the bare form
    // checked out. Either may carry unpublished local commits, so we must run
    // the drift check against whichever one actually exists locally.
    let local_branch_names = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?;
    let canonical_exists = local_branch_names.iter().any(|n| n.eq(&branch_name));
    let bare_exists = local_branch_names.iter().any(|n| n.eq(&bare_branch_name));

    // The branch we will actually merge. The bare self-submitted form is used
    // only when it exists and the canonical form does not; otherwise the
    // canonical (shorthand) form is used — created below at the published tip
    // when neither exists.
    let merge_branch = if !canonical_exists && bare_exists {
        bare_branch_name.clone()
    } else {
        branch_name.clone()
    };

    if canonical_exists || bare_exists {
        // The local PR branch already exists. The merge commit message we
        // autogenerate (subject, nevent, author, cover note) describes the
        // *published* PR state, and other maintainers can only reproduce a
        // merge of the published tip. If the local branch tip has drifted from
        // the published tip, merging would produce a commit that misrepresents
        // — or simply cannot be reproduced from — what is on the relays. Refuse
        // and tell the user how to reconcile.
        ensure_local_branch_matches_published_tip(
            &git_repo,
            &merge_branch,
            &tip_commit_str,
            &event_id,
        )?;
    } else {
        // For PR-kind proposals the tip commit lives on a git server, so try
        // to fetch it. (Patch-kind proposals reconstruct the tip from patch
        // events via `ngit pr checkout`, which we don't replicate here.)
        if !git_repo.does_commit_exist(&tip_commit_str)? && !offline && is_pr_kind {
            let _ = ensure_commit_local(
                &tip_commit_str,
                &git_repo,
                &repo_ref,
                &pr_event_clone_tag_urls(tip_event),
                &console::Term::stderr(),
                private_signer.as_ref(),
            )
            .await;
        }
        if !git_repo.does_commit_exist(&tip_commit_str)? {
            bail!(
                "PR tip commit {tip_commit_str} not found locally. Run `ngit pr checkout {}` first.",
                event_id.to_hex()
            );
        }
        git_repo.create_branch_at_commit(&branch_name, &tip_commit_str)?;
    }

    // An explicit `b` tag overrides the repository default. The target is
    // stored on the immutable root PR event, not on tip updates.
    let explicit_target = tag_value(&proposal, "b").ok();
    let target_branch = if let Some(branch) = &explicit_target {
        branch.clone()
    } else {
        git_repo
            .get_default_branch_name(None)?
            .context("could not determine the repository's default branch (e.g. main or master)")?
    };

    let target_tip_to_install = if explicit_target.is_some() {
        // Unlike a default-branch merge, a maintainer may never have checked
        // out this release or maintenance branch. Its remote-tracking ref can
        // therefore be stale even though the online event refresh above has
        // learned the latest authoritative repository state. Include that
        // state tip when selecting the merge target and fetch its object by
        // OID without mutating any tracking refs.
        let state = get_state_from_cache(Some(git_repo_path), &repo_ref)
            .await
            .context("failed to read the latest repository state for the PR target")?;
        let target_ref = format!("refs/heads/{target_branch}");
        let state_target = state.state.get(&target_ref).with_context(|| {
            format!(
                "target branch '{target_branch}' is absent from the latest repository state; it may have been deleted"
            )
        })?;
        let state_target_tip = str_to_sha1(state_target)
            .with_context(|| format!("repository state has an invalid tip for '{target_ref}'"))?;
        if !git_repo.does_commit_exist(state_target)? {
            if offline {
                bail!(
                    "target branch '{target_branch}' tip {state_target} is not available locally; rerun without --offline"
                );
            }
            ensure_commit_local(
                state_target,
                &git_repo,
                &repo_ref,
                &[],
                &console::Term::stderr(),
                private_signer.as_ref(),
            )
            .await
            .with_context(|| {
                format!("failed to fetch target branch '{target_branch}' tip {state_target}")
            })?;
        }
        let target_tip = resolve_target_branch_tip_with_known_tip(
            &git_repo,
            &target_branch,
            None,
            false,
            Some(state_target_tip),
        )?;
        Some(target_tip.to_string())
    } else if !git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|n| n.eq(&target_branch))
    {
        bail!(
            "default branch '{target_branch}' does not exist locally; check it out before merging"
        );
    } else {
        None
    };

    // A local branch belongs to at most one linked worktree. Refuse before
    // saving user changes or refreshing an explicit target ref: libgit2 will
    // reject the later checkout, but by then those mutations may already have
    // happened. Merging from the worktree that currently owns the target is
    // safe and remains supported.
    ensure_target_available_in_current_worktree(&git_repo, &target_branch)?;

    // Remember the target ref before an explicit-target merge refreshes it.
    // This lets us put the repository back exactly as it was if the user's
    // saved work cannot be reapplied to the completed merge.
    let original_target_tip = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|name| name == &target_branch)
        .then(|| {
            git_repo
                .get_tip_of_branch(&target_branch)
                .map(|tip| tip.to_string())
        })
        .transpose()?;

    // Resolve the effective (latest edited) title via the #subject label
    // override, falling back to the root proposal's title.
    let label_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .event(proposal.id)
                .kind(KIND_LABEL),
        ],
    )
    .await
    .unwrap_or_default();
    let title = process_subject(&proposal, &repo_ref, &label_events)
        .unwrap_or_else(|| cover_letter.title.clone());

    // Compose the merge commit subject: `Merge #<hex8>: <title>`, truncated so
    // it fits within git/gitlint's 72-char subject limit. When the title is
    // truncated the full title is preserved on the first body line so nothing
    // is lost in `git log` (it also remains available via the nevent below).
    let relay_hint = repo_ref.relays.first();
    let nevent = event_id_to_nevent(event_id, relay_hint);
    let (subject, truncated) = build_subject(&event_id.to_hex(), &title);

    let mut message = subject;
    if truncated {
        message.push_str("\n\n");
        message.push_str(title.trim());
    }

    // The nevent is emitted as a bare `nostr:` URI line so it is recognised as
    // a nostr URI and is exempt from body line-length linting (see .gitlint).
    message.push_str("\n\nnostr:");
    message.push_str(&nevent);

    // Attribute the PR's author with a `PR-Author:` trailer. The display name
    // is only emitted when kind-0 metadata for the author is found in the
    // cache; the npub is always emitted as a bare `nostr:` URI line so it is
    // recognised as a nostr URI and exempt from body line-length linting.
    message.push_str(&author_trailer(&proposal.pubkey, git_repo_path).await);

    // Append the cover note (latest authorised kind-1624) when present,
    // otherwise the PR description. Suppressed by --exclude-description.
    if !exclude_description {
        let cover_note_events = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .event(proposal.id)
                    .kind(KIND_COVER_NOTE),
            ],
        )
        .await
        .unwrap_or_default();

        if let Some((cover_note, _)) = process_cover_note(&proposal, &repo_ref, &cover_note_events)
        {
            let body = cover_note.content.trim();
            if !body.is_empty() {
                message.push_str("\n\nCoverNote:\n\n");
                message.push_str(body);
            }
        } else {
            let description = cover_letter.description.trim();
            if !description.is_empty() {
                message.push_str("\n\nPR description:\n\n");
                message.push_str(description);
            }
        }
    }

    // Stash only after all network and cache work is complete, but before any
    // branch ref or checkout is changed. `--include-untracked` captures the
    // same set of changes `has_outstanding_changes` reports; reapplying with
    // `--index` below restores the staged/unstaged distinction.
    let saved_worktree = SavedWorktree::capture(&git_repo)?;

    let merge_result = perform_merge(
        &git_repo,
        &target_branch,
        target_tip_to_install.as_deref(),
        &merge_branch,
        &message,
    );

    match merge_result {
        Ok(MergeResult::Created) => {
            if let Some(saved) = &saved_worktree {
                if let Err(apply_error) = saved.apply() {
                    return match saved.rollback(
                        &git_repo,
                        &target_branch,
                        original_target_tip.as_deref(),
                    ) {
                        Ok(()) => Err(apply_error.context(
                            "the merge was rolled back because the saved working-directory changes conflict with the merged tree; the original branch and changes were restored",
                        )),
                        Err(rollback_error) => bail!(
                            "the merge commit was created, but the saved working-directory changes could not be restored ({apply_error}). Automatic rollback also failed ({rollback_error}). The changes remain backed up as stash commit {}; recover them with `git stash apply --index {}`.",
                            saved.stash_oid,
                            saved.stash_oid,
                        ),
                    };
                }
                saved.drop_stash_with_warning();
            }
        }
        Ok(MergeResult::Conflicted) => {
            if let Some(saved) = &saved_worktree {
                match saved.rollback(&git_repo, &target_branch, original_target_tip.as_deref()) {
                    Ok(()) => bail!(
                        "the PR has merge conflicts. Because the working directory also had uncommitted changes, the merge was rolled back and the original branch and changes were restored. Commit or stash them before retrying if you want to resolve the PR conflicts manually."
                    ),
                    Err(rollback_error) => bail!(
                        "the PR has merge conflicts and the saved working-directory changes could not be restored automatically ({rollback_error}). They remain backed up as stash commit {}; recover them with `git stash apply --index {}` after resolving or aborting the merge.",
                        saved.stash_oid,
                        saved.stash_oid,
                    ),
                }
            }

            println!(
                "{}",
                console::style(format!(
                    "the merge has conflicts that must be resolved manually on {target_branch}."
                ))
                .yellow()
            );
            println!(
                "resolve the conflicts (see `git status`), `git add` the resolved files, then run `git commit` to complete the merge."
            );
            println!(
                "the merge commit message describing the PR has been prepared for you, so leave it unchanged when `git commit` opens your editor."
            );
            println!("to abandon the merge, run `git merge --abort`.");
            return Ok(());
        }
        Err(merge_error) => {
            if let Some(saved) = &saved_worktree {
                return match saved.rollback(
                    &git_repo,
                    &target_branch,
                    original_target_tip.as_deref(),
                ) {
                    Ok(()) => Err(merge_error.context(
                        "the merge failed; the original branch and working-directory changes were restored",
                    )),
                    Err(rollback_error) => bail!(
                        "the merge failed ({merge_error}) and the saved working-directory changes could not be restored automatically ({rollback_error}). They remain backed up as stash commit {}; recover them with `git stash apply --index {}`.",
                        saved.stash_oid,
                        saved.stash_oid,
                    ),
                };
            }
            return Err(merge_error);
        }
    }

    println!(
        "{}",
        console::style(format!(
            "merge commit created on {target_branch}. don't forget to push"
        ))
        .green()
    );

    if crate::output::is_json() {
        if let Some(ci) = &gated_ci {
            crate::output::set_value(super::pr_merge::merge_json(
                proposal.id,
                repo_ref.relays.first(),
                None,
                ci,
                None,
                None,
            ));
        }
    }

    Ok(())
}

enum MergeResult {
    Created,
    Conflicted,
}

fn ensure_target_available_in_current_worktree(git_repo: &Repo, target_branch: &str) -> Result<()> {
    if git_repo.get_local_head_branch_name()?.as_deref() == Some(target_branch) {
        return Ok(());
    }

    let repo_path = git_repo.get_path()?;
    let worktrees = run_git_stdout(
        repo_path,
        &["worktree", "list", "--porcelain"],
        "list linked worktrees",
    )?;
    let target_ref = format!("refs/heads/{target_branch}");

    for entry in worktrees.split("\n\n") {
        let branch = entry.lines().find_map(|line| line.strip_prefix("branch "));
        if branch != Some(target_ref.as_str()) {
            continue;
        }

        let path = entry
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))
            .unwrap_or("an unknown path");
        bail!(
            "target branch '{target_branch}' is checked out in another worktree at '{path}'. Run `ngit merge` from that worktree, or check out a different branch there first."
        );
    }

    Ok(())
}

/// Create the merge after all fallible asynchronous preparation is complete.
/// Keeping this phase synchronous makes it possible for the caller to restore
/// a saved worktree on every error path.
fn perform_merge(
    git_repo: &Repo,
    target_branch: &str,
    target_tip_to_install: Option<&str>,
    merge_branch: &str,
    message: &str,
) -> Result<MergeResult> {
    if let Some(target_tip) = target_tip_to_install {
        if git_repo.get_local_head_branch_name()?.as_deref() == Some(target_branch) {
            run_git_checked(
                git_repo.get_path()?,
                &["reset", "--hard", target_tip],
                "update the checked-out target branch to its authoritative tip",
            )?;
        } else {
            git_repo.create_branch_at_commit(target_branch, target_tip)?;
        }
    }

    git_repo
        .checkout(target_branch)
        .with_context(|| format!("failed to check out target branch '{target_branch}'"))?;

    let output = std::process::Command::new("git")
        .current_dir(git_repo.get_path()?)
        .args(["merge", "--no-ff", "-m", message, merge_branch])
        .output()
        .context("failed to run git merge")?;

    if output.status.success() {
        return Ok(MergeResult::Created);
    }

    // A `git merge` that stops on conflicts leaves the merge in progress:
    // `.git/MERGE_HEAD` is written, the index carries the unmerged stages and
    // the working tree has conflict markers. Git does *not* honour the `-m`
    // message in this case, so preserve ngit's prepared provenance for the
    // user's eventual `git commit`.
    if git_repo.merge_in_progress()? {
        write_prepared_merge_message(git_repo, message)
            .context("failed to record the prepared merge commit message")?;
        return Ok(MergeResult::Conflicted);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    bail!("git merge failed:\n{stdout}{stderr}")
}

/// A recoverable snapshot of the user's staged, unstaged, and untracked work.
///
/// The stash entry is addressed by its commit OID rather than assuming it
/// remains `stash@{0}`. That keeps an existing user stash stack intact and
/// lets recovery instructions remain valid if cleanup cannot complete.
struct SavedWorktree {
    repo_path: std::path::PathBuf,
    original_branch: Option<String>,
    original_head: String,
    stash_oid: String,
}

impl SavedWorktree {
    fn capture(git_repo: &Repo) -> Result<Option<Self>> {
        if !git_repo.has_outstanding_changes()? {
            return Ok(None);
        }

        let repo_path = git_repo.get_path()?.to_path_buf();
        let original_branch = git_repo.get_local_head_branch_name()?;
        let original_head = git_repo.get_head_commit()?.to_string();
        let message = format!("ngit merge automatic backup ({})", std::process::id());
        let previous_stash_oid = git_ref_oid(&repo_path, "refs/stash")?;
        run_git_checked(
            &repo_path,
            &[
                "stash",
                "push",
                "--include-untracked",
                "--message",
                &message,
            ],
            "save working-directory changes before merging",
        )?;

        let stash_oid = git_ref_oid(&repo_path, "refs/stash")?
            .filter(|oid| Some(oid) != previous_stash_oid.as_ref())
            .context("git stash did not create an automatic backup")?;
        let saved = Self {
            repo_path,
            original_branch,
            original_head,
            stash_oid,
        };

        // Some dirty states (notably changes inside submodules) cannot be
        // captured by `git stash`. Restore what was captured and refuse before
        // changing branches instead of proceeding with an incomplete backup.
        if git_repo.has_outstanding_changes()? {
            saved.apply().context(
                "git stash did not capture every working-directory change; the captured changes remain in the automatic stash",
            )?;
            saved.drop_stash_with_warning();
            bail!(
                "not every working-directory change could be saved before merging (changes inside submodules are not supported)"
            );
        }

        Ok(Some(saved))
    }

    fn apply(&self) -> Result<()> {
        run_git_checked(
            &self.repo_path,
            &["stash", "apply", "--index", &self.stash_oid],
            "restore staged, unstaged, and untracked changes",
        )
    }

    fn rollback(
        &self,
        git_repo: &Repo,
        target_branch: &str,
        original_target_tip: Option<&str>,
    ) -> Result<()> {
        if git_repo.merge_in_progress()? {
            run_git_checked(
                &self.repo_path,
                &["merge", "--abort"],
                "abort the unsuccessful merge",
            )?;
        } else {
            run_git_checked(
                &self.repo_path,
                &["reset", "--hard", "HEAD"],
                "discard the partially restored automatic stash",
            )?;
        }

        // `stash apply` may have recreated some of its untracked files before
        // discovering a conflict. They are all represented in the stash, so
        // clear them before returning to the source branch and applying the
        // complete snapshot there.
        run_git_checked(
            &self.repo_path,
            &["clean", "-fd"],
            "clear partially restored untracked files",
        )?;

        let current_branch = git_repo.get_local_head_branch_name()?;
        match &self.original_branch {
            Some(branch)
                if branch == target_branch && current_branch.as_deref() == Some(target_branch) =>
            {
                run_git_checked(
                    &self.repo_path,
                    &["reset", "--hard", &self.original_head],
                    "restore the original target branch tip",
                )?;
            }
            Some(branch) if branch == target_branch => {
                restore_target_ref(&self.repo_path, target_branch, original_target_tip)?;
                run_git_checked(
                    &self.repo_path,
                    &["checkout", "--force", branch],
                    "restore the original branch",
                )?;
            }
            Some(branch) => {
                run_git_checked(
                    &self.repo_path,
                    &["checkout", "--force", branch],
                    "restore the original branch",
                )?;
                restore_target_ref(&self.repo_path, target_branch, original_target_tip)?;
            }
            None => {
                run_git_checked(
                    &self.repo_path,
                    &["checkout", "--detach", "--force", &self.original_head],
                    "restore the original detached HEAD",
                )?;
                restore_target_ref(&self.repo_path, target_branch, original_target_tip)?;
            }
        }

        self.apply()?;
        self.drop_stash_with_warning();
        Ok(())
    }

    fn drop_stash_with_warning(&self) {
        if let Err(error) = self.drop_stash() {
            eprintln!(
                "warning: your changes were restored, but ngit could not remove its automatic backup: {error}"
            );
        }
    }

    fn drop_stash(&self) -> Result<()> {
        let entries = run_git_stdout(
            &self.repo_path,
            &["stash", "list", "--format=%H%x09%gd"],
            "list stashes while removing the automatic backup",
        )?;
        let selector = entries.lines().find_map(|line| {
            let (oid, selector) = line.split_once('\t')?;
            (oid == self.stash_oid).then_some(selector)
        });
        let selector = selector.context(
            "the automatic backup was restored but its entry was not found in the stash list",
        )?;
        run_git_checked(
            &self.repo_path,
            &["stash", "drop", "--quiet", selector],
            "remove the restored automatic stash",
        )
    }
}

fn restore_target_ref(
    repo_path: &Path,
    target_branch: &str,
    original_target_tip: Option<&str>,
) -> Result<()> {
    let target_ref = format!("refs/heads/{target_branch}");
    if let Some(tip) = original_target_tip {
        run_git_checked(
            repo_path,
            &["update-ref", &target_ref, tip],
            "restore the original target branch tip",
        )
    } else if git_ref_exists(repo_path, &target_ref)? {
        run_git_checked(
            repo_path,
            &["update-ref", "-d", &target_ref],
            "remove the target branch created by the unsuccessful merge",
        )
    } else {
        Ok(())
    }
}

fn git_ref_oid(repo_path: &Path, reference: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .context("failed to resolve a Git ref")?;
    match output.status.code() {
        Some(0) => {
            let oid = String::from_utf8(output.stdout)
                .context("git rev-parse returned a non-UTF-8 object ID")?;
            Ok(Some(oid.trim().to_string()))
        }
        Some(1) => Ok(None),
        _ => bail!(
            "git rev-parse failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn git_ref_exists(repo_path: &Path, reference: &str) -> Result<bool> {
    let status = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["show-ref", "--verify", "--quiet", reference])
        .status()
        .context("failed to check whether a Git ref exists")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git show-ref failed with status {status}"),
    }
}

fn run_git_checked(repo_path: &Path, args: &[&str], action: &str) -> Result<()> {
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .with_context(|| format!("failed to {action}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to {action}:\n{stdout}{stderr}")
    }
}

fn run_git_stdout(repo_path: &Path, args: &[&str], action: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .with_context(|| format!("failed to {action}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to {action}:\n{stdout}{stderr}");
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("failed to read output while trying to {action}"))?;
    Ok(stdout.trim_end().to_string())
}

/// Overwrite the in-progress merge's prepared commit message
/// (`.git/MERGE_MSG`) with `message`.
///
/// When `git merge` stops on conflicts it writes its own generic `MERGE_MSG`,
/// ignoring the `-m` we passed. The user's eventual `git commit` reads that
/// file, so to preserve ngit's composed message — subject, `nostr:` nevent,
/// `PR-Author:` trailer and cover note — we replace it here once the conflict
/// is detected.
///
/// The path is resolved with `git rev-parse --git-path MERGE_MSG` so it is
/// correct regardless of worktree layout or a separated git dir, rather than
/// assuming `.git/MERGE_MSG` under the working tree.
fn write_prepared_merge_message(git_repo: &Repo, message: &str) -> Result<()> {
    let git_repo_path = git_repo.get_path()?;
    let output = std::process::Command::new("git")
        .current_dir(git_repo_path)
        .args(["rev-parse", "--git-path", "MERGE_MSG"])
        .output()
        .context("failed to locate MERGE_MSG via git rev-parse")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --git-path MERGE_MSG failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let rel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // `--git-path` yields a path relative to the working directory we invoked
    // git in (or already absolute); resolve it against that same directory.
    let path = git_repo_path.join(rel);
    // Preserve a trailing newline so the message matches git's own formatting.
    std::fs::write(&path, format!("{}\n", message.trim_end())).context(format!(
        "failed to write merge message to {}",
        path.display()
    ))?;
    Ok(())
}

/// Build the merge commit subject `Merge #<hex8>: <title>`, truncating the
/// title with an ellipsis so the whole line stays within git/gitlint's 72-char
/// subject limit. The first 8 hex chars of the event id mirror the web UI's
/// `#e2df2001` shorthand.
///
/// Returns `(subject, truncated)` where `truncated` is `true` when the title
/// did not fit and was shortened — the caller then preserves the full title on
/// the first body line so it is not lost.
fn build_subject(event_id_hex: &str, title: &str) -> (String, bool) {
    const MAX_SUBJECT_LEN: usize = 72;
    let shorthand = &event_id_hex[..8.min(event_id_hex.len())];
    let prefix = format!("Merge #{shorthand}: ");
    let title = title.trim();

    // Characters available for the title after the prefix.
    let budget = MAX_SUBJECT_LEN.saturating_sub(prefix.chars().count());
    let title_chars = title.chars().count();

    if title_chars <= budget {
        return (format!("{prefix}{title}"), false);
    }

    // Truncate to budget, reserving one char for the ellipsis. Iterate over
    // chars (not bytes) to avoid splitting multi-byte UTF-8.
    let keep = budget.saturating_sub(1);
    let truncated: String = title.chars().take(keep).collect();
    (format!("{prefix}{}\u{2026}", truncated.trim_end()), true)
}

/// The published tip commit id of a proposal, read from its tip event chain
/// (as returned by `get_pr_tip_event_or_most_recent_patch_with_ancestors`):
/// the `c` tag of the tip event when the chain carries a PR/PR-update event,
/// otherwise the commit id recorded in the most recent patch.
fn published_tip_commit_str(tip_chain: &[nostr::prelude::Event]) -> Result<String> {
    let tip_event = tip_chain.first().context("tip chain is empty")?;
    if tip_chain
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind))
    {
        tag_value(tip_event, "c").context("PR event missing tip commit tag 'c'")
    } else {
        get_commit_id_from_patch(tip_event).context("failed to get commit id from patch")
    }
}

/// When invoked without an id, infer the PR from the checked-out branch.
///
/// Three matching strategies are tried in order:
///
/// 1. Branches created by `ngit pr checkout` are named
///    `pr/<name>(<first-8-hex-of-event-id>)`; the shorthand is extracted and
///    matched against the known proposals.
///
/// 2. Branches the current user authored and published themselves with a plain
///    `git push <remote> -u pr/<name>` carry no shorthand. These are linked to
///    a published PR by matching the bare `pr/<name>` against proposals
///    authored by the logged-in user — the same mapping `git-remote-nostr` uses
///    on push (`is_event_proposal_root_for_branch`).
///
/// 3. When the author-based mapping finds nothing (a PR authored by someone
///    else, or no logged-in user) or several same-named candidates, a bare
///    `pr/<name>` branch is resolved by comparing its tip commit against the
///    published tip of every open/draft proposal. A unique tip match resolves
///    the PR regardless of login state or authorship. A PR resolved this way
///    trivially passes the later `ensure_local_branch_matches_published_tip`
///    drift check: tip equality is exactly the no-drift condition.
async fn resolve_event_id_from_current_branch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    proposals_and_revisions: &[nostr::prelude::Event],
) -> Result<EventId> {
    let branch = git_repo
        .get_checked_out_branch_name()
        .context("failed to determine the checked-out branch")?;

    if !branch.starts_with("pr/") {
        bail!("not on a `pr/` branch; specify a PR event-id or nevent, e.g. `ngit merge <id>`");
    }

    // Convention 1: `pr/<name>(<8-hex>)` created by `ngit pr checkout`.
    if let Some(shorthand) = branch
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
    {
        return resolve_pr_root_id_or_prefix(shorthand, proposals_and_revisions.iter(), pr_description)
        .with_context(|| {
            format!(
                "failed to resolve PR id encoded in branch '{branch}'; specify a PR event-id or nevent"
            )
        });
    }

    // Convention 2: a plain `pr/<name>` the current user pushed themselves
    // (`git push <remote> -u pr/<name>`). Link it to a published PR the
    // logged-in user authored, mirroring the push-side mapping.
    let current_user =
        get_curent_user(git_repo).context("failed to read the logged-in user from git config")?;

    let matches: Vec<&nostr::prelude::Event> = proposal_roots(proposals_and_revisions)
        .filter(|e| {
            is_event_proposal_root_for_branch(e, &branch, current_user.as_ref()).unwrap_or(false)
        })
        .collect();

    if let [only] = matches.as_slice() {
        return Ok(only.id);
    }

    // Convention 3 (fallback): the author-based mapping found zero or several
    // candidates, so match the branch's tip commit against the published tips
    // of open/draft proposals instead. This resolves the cases convention 2
    // cannot: a maintainer merging someone else's PR from a bare branch, a
    // logged-out author merging their own, and same-named PRs told apart by
    // their tips.
    let tip_matches = proposals_with_published_tip_at_branch_tip(git_repo, repo_ref, &branch)
        .await
        .with_context(|| {
            format!("failed to match the tip of branch '{branch}' against published PR tips")
        })?;
    match tip_matches.as_slice() {
        [(only, _)] => return Ok(*only),
        [] => {}
        multiple => {
            // Several proposals share the branch's tip commit; the branch
            // name can still single one out when exactly one of them carries
            // it as its bare `pr/<name>` form.
            let named: Vec<&EventId> = multiple
                .iter()
                .filter(|(_, bare_branch_name)| bare_branch_name.eq(&branch))
                .map(|(id, _)| id)
                .collect();
            if let [only] = named.as_slice() {
                return Ok(**only);
            }
            bail!(
                "branch '{branch}' tip matches more than one open PR; specify the PR event-id or nevent"
            );
        }
    }

    if matches.is_empty() {
        if current_user.is_none() {
            bail!(
                "branch '{branch}' does not encode a PR id, its tip does not match the published tip of any open PR, and no logged-in user is configured to link it to a PR you authored; specify a PR event-id or nevent, or run `ngit login`"
            );
        }
        bail!(
            "branch '{branch}' does not encode a PR id, no PR you authored matches it, and its tip does not match the published tip of any open PR; specify a PR event-id or nevent"
        )
    }
    bail!(
        "branch '{branch}' matches more than one of your PRs and its tip does not identify one of them; specify the PR event-id or nevent"
    )
}

/// Open or draft proposals whose published tip commit equals the local tip of
/// `branch`, as `(proposal root id, bare pr/<name> branch name)` pairs.
///
/// The published tip is derived from the same event chain the drift check
/// uses (`get_open_or_draft_proposals` builds it with
/// `get_pr_tip_event_or_most_recent_patch_with_ancestors`): the `c` tag of
/// the latest PR/PR-update event, or the commit id recorded in the most
/// recent patch. Proposals whose tip cannot be determined or parsed are
/// skipped — they cannot be what the branch points at.
async fn proposals_with_published_tip_at_branch_tip(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    branch: &str,
) -> Result<Vec<(EventId, String)>> {
    let local_tip = git_repo
        .get_tip_of_branch(branch)
        .context(format!("failed to read local tip of branch '{branch}'"))?;

    let open_or_draft = get_open_or_draft_proposals(git_repo, repo_ref)
        .await
        .context("failed to load open and draft proposals from cache")?;

    let mut matches = vec![];
    for (root_id, (proposal, tip_chain, _)) in &open_or_draft {
        let Ok(tip_commit_str) = published_tip_commit_str(tip_chain) else {
            continue;
        };
        let Ok(published_tip) = str_to_sha1(&tip_commit_str) else {
            continue;
        };
        if published_tip.eq(&local_tip) {
            let bare_branch_name = event_to_cover_letter(proposal)
                .map(|cl| format!("pr/{}", cl.branch_name_without_id_or_prefix))
                .unwrap_or_default();
            matches.push((*root_id, bare_branch_name));
        }
    }
    Ok(matches)
}

/// Refuse to merge when the local `pr/...` branch tip has drifted from the
/// published PR tip.
///
/// `ngit merge` autogenerates the merge commit message — subject, nevent,
/// `PR-Author:` trailer and cover note — to describe the *published* state of
/// the PR, and other maintainers can only reproduce the merge against the tip
/// recorded on the relays. If the local branch points somewhere else we must
/// not merge:
///
/// * **local ahead** — the user has commits that are not yet published; merging
///   would bake unpublished work into the default branch and produce a merge no
///   one else can reproduce. They should push first so the PR tip is updated.
/// * **local behind** — the local branch is stale; merging would land an older
///   revision than what is published. They should fast-forward to the published
///   tip first.
/// * **diverged** — both, or histories that share no relationship.
///
/// When the local tip already equals the published tip this is a no-op.
fn ensure_local_branch_matches_published_tip(
    git_repo: &Repo,
    branch_name: &str,
    tip_commit_str: &str,
    event_id: &EventId,
) -> Result<()> {
    let local_tip = git_repo.get_tip_of_branch(branch_name).context(format!(
        "failed to read local tip of branch '{branch_name}'"
    ))?;

    let published_tip =
        str_to_sha1(tip_commit_str).context("PR event recorded an invalid tip commit id")?;

    if local_tip.eq(&published_tip) {
        return Ok(());
    }

    // If the published tip is not in the local object database we cannot
    // classify the drift; report the mismatch plainly.
    if !git_repo.does_commit_exist(tip_commit_str)? {
        bail!(
            "local branch '{branch_name}' (at {local_tip}) does not match the published PR tip {tip_commit_str}, which is not present locally. Reconcile the branch with the published PR before merging (e.g. fetch, then `ngit pr checkout {}`).",
            event_id.to_hex()
        );
    }

    // Both commits are present locally: classify the drift to give precise
    // guidance. `get_commits_ahead_behind(base, latest)` returns
    // `(ahead, behind)` relative to `latest`: `ahead` = commits on the local
    // branch missing from the published tip, `behind` = commits on the
    // published tip missing locally. It errors when the two share no common
    // ancestor at all, which we surface as an unrelated-histories mismatch.
    let Ok((ahead, behind)) = git_repo.get_commits_ahead_behind(&published_tip, &local_tip) else {
        bail!(
            "local branch '{branch_name}' (at {local_tip}) shares no history with the published PR tip {tip_commit_str}. Reconcile the branch with the published PR before merging (e.g. `ngit pr checkout {}`).",
            event_id.to_hex()
        )
    };

    match (ahead.is_empty(), behind.is_empty()) {
        // ahead only: unpublished local commits.
        (false, true) => bail!(
            "local branch '{branch_name}' is {} commit(s) ahead of the published PR tip {tip_commit_str}. Push your changes so the PR is updated before merging, e.g. `git push <remote> {branch_name}`.",
            ahead.len()
        ),
        // behind only: stale local branch.
        (true, false) => bail!(
            "local branch '{branch_name}' is {} commit(s) behind the published PR tip {tip_commit_str}. Fast-forward it to the published tip before merging, e.g. `git checkout {branch_name} && git merge --ff-only {tip_commit_str}`.",
            behind.len()
        ),
        // diverged: both ahead and behind.
        (false, false) => bail!(
            "local branch '{branch_name}' has diverged from the published PR tip {tip_commit_str} ({} ahead, {} behind). Reconcile it with the published PR (push your changes or reset to the published tip) before merging.",
            ahead.len(),
            behind.len()
        ),
        // equal commit lists but differing tips is unreachable (we returned
        // early on equality) — treat defensively as a plain mismatch.
        (true, true) => bail!(
            "local branch '{branch_name}' (at {local_tip}) does not match the published PR tip {tip_commit_str}. Reconcile the branch with the published PR before merging."
        ),
    }
}

/// Build the `PR-Author:` trailer attributing the PR's author.
///
/// The trailer always carries the author's npub on its own line as a bare
/// `nostr:` URI (so git/gitlint treats it as a nostr URI exempt from body
/// line-length linting). When kind-0 metadata for the author is found in the
/// local cache, the resolved display name is emitted on the `PR-Author:` line;
/// otherwise that line carries only the label.
///
/// The lookup reads the **local** cache because that is where
/// `fetching_with_report` lands contributor profiles (kind-0) for proposal
/// authors — see `get_repo_coordinates`'s profile back-fill in client.rs.
async fn author_trailer(author: &PublicKey, git_repo_path: &std::path::Path) -> String {
    let npub = author.to_bech32().unwrap_or_else(|_| author.to_hex());

    // Only surface a display name when kind-0 metadata is actually present in
    // the cache. `extract_user_metadata` falls back to the npub when no
    // human-readable name is set, so treat that fallback as "no name".
    let metadata_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .author(*author)
                .kind(nostr::prelude::Kind::Metadata),
        ],
    )
    .await
    .unwrap_or_default();

    let display_name = extract_user_metadata(author, &metadata_events)
        .ok()
        .map(|m| m.name.trim().to_string())
        .filter(|name| !name.is_empty() && *name != npub);

    let label = match display_name {
        Some(name) => format!("\n\nPR-Author: {name}"),
        None => "\n\nPR-Author:".to_string(),
    };
    format!("{label}\nnostr:{npub}")
}

fn event_id_to_nevent(event_id: EventId, relay: Option<&RelayUrl>) -> String {
    let relays = relay.map(|r| vec![r.clone()]).unwrap_or_default();
    Nip19Event {
        event_id,
        relays,
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_else(|_| event_id.to_hex())
}

#[cfg(test)]
mod tests {
    use super::build_subject;

    const ID: &str = "e2df2001abcdef0123456789abcdef0123456789abcdef0123456789abcdef01";

    #[test]
    fn short_title_is_not_truncated() {
        let (subject, truncated) = build_subject(ID, "fix the thing");
        assert_eq!(subject, "Merge #e2df2001: fix the thing");
        assert!(!truncated);
    }

    #[test]
    fn long_title_is_truncated_within_72_chars_and_flagged() {
        let title = "a really long pull request title that goes well beyond the subject limit";
        let (subject, truncated) = build_subject(ID, title);
        assert!(
            truncated,
            "an over-length title must be flagged as truncated"
        );
        assert!(
            subject.chars().count() <= 72,
            "subject must fit within 72 chars, got {}: {subject}",
            subject.chars().count(),
        );
        assert!(subject.starts_with("Merge #e2df2001: "));
        assert!(
            subject.ends_with('\u{2026}'),
            "truncated subject ends with an ellipsis"
        );
    }

    #[test]
    fn truncation_does_not_split_multibyte_chars() {
        // 70 multibyte chars guarantees truncation; the result must remain
        // valid UTF-8 (no panic from slicing mid-codepoint).
        let title = "é".repeat(70);
        let (subject, truncated) = build_subject(ID, &title);
        assert!(truncated);
        assert!(subject.chars().count() <= 72);
    }
}
