use anyhow::{Context, Result, bail};
use ngit::{
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache,
    },
    fetch::ensure_commit_local,
    git::sha1_to_oid,
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, get_commit_id_from_patch,
        get_parent_commit_from_patch, get_pr_tip_event_or_most_recent_patch_with_ancestors,
        pr_event_clone_tag_urls, tag_value,
    },
    repo_ref::RepoRef,
};
use nostr::{EventId, FromBech32, nips::nip19::Nip19};

use crate::{
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions, str_to_sha1},
    git_events::event_to_cover_letter,
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

pub async fn launch(id: &str, force: bool, offline: bool) -> Result<()> {
    let event_id = parse_event_id(id)?;

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    let nostr_remote = git_repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await
        .ok()
        .flatten();

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal = proposals_and_revisions
        .iter()
        .find(|e| e.id == event_id)
        .context(format!(
            "proposal with id {} not found in cache",
            event_id.to_hex()
        ))?;

    let cover_letter = event_to_cover_letter(proposal)
        .context("failed to extract proposal details from proposal root event")?;

    let commits_events: Vec<nostr::Event> = get_all_proposal_patch_pr_pr_update_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal.id,
    )
    .await?;

    let most_recent_proposal_patch_chain_or_pr_or_pr_update =
        get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events.clone())
            .context("failed to find any PR or patch events on this proposal")?;

    if most_recent_proposal_patch_chain_or_pr_or_pr_update
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind))
    {
        checkout_pr(
            &git_repo,
            &repo_ref,
            &cover_letter,
            &most_recent_proposal_patch_chain_or_pr_or_pr_update,
            nostr_remote.as_ref().map(|(name, _)| name.as_str()),
            force,
        )
    } else {
        checkout_patch(
            &git_repo,
            &repo_ref,
            &cover_letter,
            &most_recent_proposal_patch_chain_or_pr_or_pr_update,
            nostr_remote.as_ref().map(|(name, _)| name.as_str()),
            force,
        )
    }
}

fn parse_event_id(id: &str) -> Result<EventId> {
    if let Ok(nip19) = Nip19::from_bech32(id) {
        match nip19 {
            Nip19::Event(e) => return Ok(e.event_id),
            Nip19::EventId(event_id) => return Ok(event_id),
            _ => {}
        }
    }
    if let Ok(event_id) = EventId::from_hex(id) {
        return Ok(event_id);
    }
    bail!("invalid event-id or nevent: {id}")
}

fn print_diverged_branch_help(branch_name: &str) {
    eprintln!(
        "{}",
        console::style(format!(
            "Branch '{branch_name}' has diverged from the published proposal."
        ))
        .yellow()
    );
    eprintln!(
        "{}",
        console::style(
            "This may be because you have local amendments, or the author force-pushed a new revision."
        )
        .yellow()
    );
    eprintln!(
        "{}",
        console::style("To overwrite local branch with the published version:").yellow()
    );
    eprintln!(
        "{}",
        console::style("  ngit pr checkout --force <id>").yellow()
    );
    eprintln!(
        "{}",
        console::style("To publish your local amendments as a new revision:").yellow()
    );
    eprintln!("{}", console::style("  ngit push --force").yellow());
}

fn checkout_pr(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    cover_letter: &crate::git_events::CoverLetter,
    most_recent_proposal_patch_chain_or_pr_or_pr_update: &[nostr::Event],
    nostr_remote_name: Option<&str>,
    force: bool,
) -> Result<()> {
    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
    let proposal_tip_event = most_recent_proposal_patch_chain_or_pr_or_pr_update
        .first()
        .context("most_recent_proposal_patch_chain_or_pr_or_pr_update will always contain an event with c tag")?;
    let proposal_tip = tag_value(proposal_tip_event, "c")?;
    let proposal_tip_sha1 = str_to_sha1(&proposal_tip)?;

    // Case 1: branch doesn't exist yet — create it.
    let Ok(local_branch_tip) = git_repo.get_tip_of_branch(&branch_name) else {
        ensure_commit_local(
            &proposal_tip,
            git_repo,
            repo_ref,
            &pr_event_clone_tag_urls(proposal_tip_event),
            &console::Term::stderr(),
        )?;
        git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
        git_repo.checkout(&branch_name)?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "created and checked out proposal branch '{branch_name}'{}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    };

    // Case 2: up to date.
    if local_branch_tip.to_string() == proposal_tip {
        git_repo
            .checkout(&branch_name)
            .context("cannot checkout existing proposal branch")?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "checked out up-to-date proposal branch '{branch_name}'{}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    }

    if git_repo.does_commit_exist(&proposal_tip)? {
        let local_is_ancestor_of_published =
            git_repo.ancestor_of(&proposal_tip_sha1, &local_branch_tip)?;
        let published_is_ancestor_of_local =
            git_repo.ancestor_of(&local_branch_tip, &proposal_tip_sha1)?;

        // Case 3: branch is behind — fast-forward.
        if local_is_ancestor_of_published {
            git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
            git_repo.checkout(&branch_name)?;
            let tracked =
                maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
            println!(
                "checked out proposal branch and updated tip '{branch_name}'{}",
                tracking_suffix(tracked, nostr_remote_name),
            );
            return Ok(());
        }

        // Case 4: local commits on top — check out without touching them.
        if published_is_ancestor_of_local {
            git_repo
                .checkout(&branch_name)
                .context("cannot checkout existing proposal branch")?;
            let tracked =
                maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
            println!(
                "checked out proposal branch '{branch_name}' (local branch has unpublished commits on top){}",
                tracking_suffix(tracked, nostr_remote_name),
            );
            return Ok(());
        }
    }

    // Case 5 (and tip-not-found): diverged — require --force.
    if force {
        ensure_commit_local(
            &proposal_tip,
            git_repo,
            repo_ref,
            &pr_event_clone_tag_urls(proposal_tip_event),
            &console::Term::stderr(),
        )?;
        git_repo.create_branch_at_commit(&branch_name, &proposal_tip)?;
        git_repo.checkout(&branch_name)?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "checked out proposal branch '{branch_name}' updated to published tip (overwrote diverged local branch){}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    }

    git_repo
        .checkout(&branch_name)
        .context("cannot checkout existing proposal branch")?;
    print_diverged_branch_help(&branch_name);
    bail!(
        "branch '{branch_name}' has diverged from the published proposal; use --force to overwrite"
    )
}

#[allow(clippy::too_many_lines)]
fn checkout_patch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    cover_letter: &crate::git_events::CoverLetter,
    most_recent_proposal_patch_chain_or_pr_or_pr_update: &[nostr::Event],
    nostr_remote_name: Option<&str>,
    force: bool,
) -> Result<()> {
    if git_repo.has_outstanding_changes()? {
        bail!("working directory is not clean. Discard or stash (un)staged changes and try again.");
    }

    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;

    // Best-effort: ensure the patch chain's parent commit is available locally
    // before any call to `apply_patch_chain` below. The proposal may be based
    // on a commit we haven't pulled yet (e.g. a new revision authored against
    // a newer `main`). `ensure_commit_local` short-circuits when the commit is
    // already present, so this is free in the common case. Errors are ignored
    // — `apply_patch_chain` will surface a meaningful error if the commit is
    // still missing.
    let ensure_patch_parent = || {
        if let Some(oldest_patch) = most_recent_proposal_patch_chain_or_pr_or_pr_update.last() {
            if let Ok(parent_oid) = get_parent_commit_from_patch(oldest_patch, Some(git_repo)) {
                let _ = ensure_commit_local(
                    &parent_oid,
                    git_repo,
                    repo_ref,
                    &[],
                    &console::Term::stderr(),
                );
            }
        }
    };

    // Case 1: branch doesn't exist yet — create and apply.
    let branch_exists = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|n| n.eq(&branch_name));

    if !branch_exists {
        ensure_patch_parent();
        let _ = git_repo
            .apply_patch_chain(
                &branch_name,
                most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
            )
            .context("failed to apply patch chain")?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "checked out proposal as '{branch_name}' branch{}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    }

    let local_branch_tip = git_repo.get_tip_of_branch(&branch_name)?;

    // Resolve the published tip commit id. If we can't (no commit tag), fall
    // through to apply_patch_chain which handles idempotency itself.
    let Ok(proposal_tip_str) = get_commit_id_from_patch(
        most_recent_proposal_patch_chain_or_pr_or_pr_update
            .first()
            .context("there should be at least one patch")?,
    ) else {
        git_repo.checkout(&branch_name)?;
        ensure_patch_parent();
        let _ = git_repo
            .apply_patch_chain(
                &branch_name,
                most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
            )
            .context("failed to apply patch chain")?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "checked out updated proposal as '{branch_name}' branch{}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    };

    let Ok(proposal_tip) = str_to_sha1(&proposal_tip_str) else {
        git_repo.checkout(&branch_name)?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "checked out proposal as '{branch_name}' branch{}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    };

    // Case 2: already up to date.
    if proposal_tip.eq(&local_branch_tip) {
        git_repo.checkout(&branch_name)?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "branch '{branch_name}' checked out and up-to-date{}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    }

    // For cases 3-5 we need to know the ancestry relationship.
    if git_repo.does_commit_exist(&proposal_tip_str)? {
        let published_is_ancestor_of_local =
            git_repo.ancestor_of(&local_branch_tip, &proposal_tip)?;
        let local_is_ancestor_of_published =
            git_repo.ancestor_of(&proposal_tip, &local_branch_tip)?;

        // Case 3: branch is behind — local tip is an ancestor of the published
        // tip, meaning the author appended new patches. Fast-forward.
        if local_is_ancestor_of_published {
            git_repo.checkout(&branch_name)?;
            ensure_patch_parent();
            let _ = git_repo
                .apply_patch_chain(
                    &branch_name,
                    most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
                )
                .context("failed to apply patch chain")?;
            let tracked =
                maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
            println!(
                "checked out updated proposal as '{branch_name}' branch{}",
                tracking_suffix(tracked, nostr_remote_name),
            );
            return Ok(());
        }

        // Case 4: local has commits stacked on top of the published tip —
        // published tip is an ancestor of local tip. Check out without touching
        // commits.
        if published_is_ancestor_of_local {
            git_repo.checkout(&branch_name)?;
            let tracked =
                maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
            println!(
                "checked out proposal branch '{branch_name}' (local branch has unpublished commits on top){}",
                tracking_suffix(tracked, nostr_remote_name),
            );
            return Ok(());
        }

        // Case 5: diverged — neither is an ancestor of the other.
        // This covers both local amendments and author force-pushes.
        // Require --force to overwrite.
        if force {
            git_repo.checkout(&branch_name)?;
            ensure_patch_parent();
            let _ = git_repo
                .apply_patch_chain(
                    &branch_name,
                    most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
                )
                .context("failed to apply patch chain")?;
            let tracked =
                maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
            println!(
                "checked out updated proposal as '{branch_name}' branch (overwrote diverged local branch){}",
                tracking_suffix(tracked, nostr_remote_name),
            );
            return Ok(());
        }

        git_repo.checkout(&branch_name)?;
        print_diverged_branch_help(&branch_name);
        bail!(
            "branch '{branch_name}' has diverged from the published proposal; use --force to overwrite"
        );
    }

    // Published tip not found locally and branch already exists — the author
    // has published a new revision whose commits we don't have yet. Treat as
    // diverged: require --force to overwrite.
    if force {
        git_repo.checkout(&branch_name)?;
        ensure_patch_parent();
        let _ = git_repo
            .apply_patch_chain(
                &branch_name,
                most_recent_proposal_patch_chain_or_pr_or_pr_update.to_vec(),
            )
            .context("failed to apply patch chain")?;
        let tracked = maybe_setup_nostr_remote_tracking(git_repo, nostr_remote_name, &branch_name)?;
        println!(
            "checked out updated proposal as '{branch_name}' branch (overwrote diverged local branch){}",
            tracking_suffix(tracked, nostr_remote_name),
        );
        return Ok(());
    }

    git_repo.checkout(&branch_name)?;
    print_diverged_branch_help(&branch_name);
    bail!(
        "branch '{branch_name}' has diverged from the published proposal; use --force to overwrite"
    )
}

/// After a successful checkout of a PR/patch branch, configure it to track
/// the nostr remote so the user can run `git pull` later.
///
/// - Updates (or creates) `refs/remotes/<remote>/<branch>` at the branch's
///   current tip.
/// - If the branch has no upstream yet, sets the upstream to
///   `<remote>/<branch>`. Existing upstreams (e.g. user-configured) are
///   preserved.
///
/// Returns `Ok(true)` if a nostr remote was found and the ref/upstream was
/// updated, `Ok(false)` if no nostr remote is configured (no-op).
///
/// Determinism note: the OID written to the remote-tracking ref is the local
/// branch tip. For PR-kind proposals this matches the canonical `c` tag.
/// For patch-kind proposals the OID is whatever `apply_patch_chain`
/// produced; `git-remote-nostr` reconstructs from the same patch events via
/// the same `create_commit_from_patch` code path, so a later `git pull`
/// will see a matching OID and report "already up to date".
pub fn maybe_setup_nostr_remote_tracking(
    git_repo: &Repo,
    nostr_remote_name: Option<&str>,
    branch_name: &str,
) -> Result<bool> {
    let Some(remote_name) = nostr_remote_name else {
        return Ok(false);
    };

    let tip_sha1 = git_repo
        .get_tip_of_branch(branch_name)
        .context("failed to read tip of branch to set up nostr-remote tracking")?;
    let tip_oid = sha1_to_oid(&tip_sha1)?;

    set_nostr_remote_tracking_ref(git_repo, remote_name, branch_name, tip_oid)?;

    if git_repo
        .get_upstream_for_branch(branch_name)
        .context("failed to look up branch upstream")?
        .is_none()
    {
        let mut branch = git_repo
            .git_repo
            .find_branch(branch_name, git2::BranchType::Local)
            .context("failed to find local branch to set upstream")?;
        branch
            .set_upstream(Some(&format!("{remote_name}/{branch_name}")))
            .context("failed to set upstream tracking")?;
    }
    Ok(true)
}

/// Write (or update) `refs/remotes/<remote>/<branch>` to point at `oid`.
///
/// The caller must have verified `oid` is in the local object store. This
/// is purely local bookkeeping; the next `git pull` against the nostr
/// remote will re-advertise the same ref from the PR event's `c` tag
/// (PR-kind) or by reconstructing commits from patch events (patch-kind)
/// via the same `create_commit_from_patch` code path that
/// `apply_patch_chain` uses, so the OIDs match.
fn set_nostr_remote_tracking_ref(
    git_repo: &Repo,
    remote_name: &str,
    branch_name: &str,
    oid: git2::Oid,
) -> Result<()> {
    let target_ref = format!("refs/remotes/{remote_name}/{branch_name}");
    if let Ok(mut r) = git_repo.git_repo.find_reference(&target_ref) {
        r.set_target(oid, "set by ngit pr checkout")
            .context("failed to update remote-tracking ref")?;
    } else {
        git_repo
            .git_repo
            .reference(&target_ref, oid, false, "created by ngit pr checkout")
            .context("failed to create remote-tracking ref")?;
    }
    Ok(())
}

pub fn tracking_suffix(tracked: bool, nostr_remote_name: Option<&str>) -> String {
    match (tracked, nostr_remote_name) {
        (true, Some(name)) => format!(" with tracking to {name}"),
        _ => String::new(),
    }
}
