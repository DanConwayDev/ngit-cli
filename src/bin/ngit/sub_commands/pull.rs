use anyhow::{bail, Context, Result};
use ngit::git_events::is_event_proposal_root_for_branch;
use nostr_sdk::PublicKey;

use crate::{
    client::{
        fetching_with_report, get_all_proposal_patch_events_from_cache,
        get_proposals_and_revisions_from_cache, get_repo_ref_from_cache, Client, Connect,
    },
    git::{str_to_sha1, Repo, RepoActions},
    git_events::{get_commit_id_from_patch, get_most_recent_patch_with_ancestors, tag_value},
    repo_ref::get_repo_coordinates,
};

#[allow(clippy::too_many_lines)]
pub async fn launch() -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let (main_or_master_branch_name, _) = git_repo
        .get_main_or_master_branch()
        .context("no main or master branch")?;

    let branch_name = git_repo
        .get_checked_out_branch_name()
        .context("cannot get checked out branch name")?;

    if branch_name == main_or_master_branch_name {
        bail!("checkout a branch associated with a proposal first")
    }
    let client = Client::default();

    let repo_coordinates = get_repo_coordinates(&git_repo, &client).await?;
    fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &repo_coordinates).await?;

    let logged_in_public_key =
        if let Ok(Some(npub)) = git_repo.get_git_config_item("nostr.npub", None) {
            PublicKey::parse(npub).ok()
        } else {
            None
        };

    let proposal_root_event =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .find(|e| {
                is_event_proposal_root_for_branch(e, &branch_name, &logged_in_public_key)
                    .unwrap_or(false)
            })
            .context("cannot find proposal that matches the current branch name")?
            .clone();

    let commit_events = get_all_proposal_patch_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal_root_event.id(),
    )
    .await?;

    let most_recent_proposal_patch_chain =
        get_most_recent_patch_with_ancestors(commit_events.clone())
            .context("cannot get most recent patch for proposal")?;

    let local_branch_tip = git_repo.get_tip_of_branch(&branch_name)?;

    let (main_branch_name, master_tip) = git_repo.get_main_or_master_branch()?;

    let (local_ahead_of_main, local_beind_main) =
        git_repo.get_commits_ahead_behind(&master_tip, &local_branch_tip)?;

    let proposal_base_commit = str_to_sha1(&tag_value(
        most_recent_proposal_patch_chain
            .last()
            .context("there should be at least one patch as we have already checked for this")?,
        "parent-commit",
    )?)
    .context("cannot get valid parent commit id from patch")?;

    let (_, proposal_behind_main) =
        git_repo.get_commits_ahead_behind(&master_tip, &proposal_base_commit)?;

    let proposal_tip =
        str_to_sha1(
            &get_commit_id_from_patch(most_recent_proposal_patch_chain.first().context(
                "there should be at least one patch as we have already checked for this",
            )?)
            .context("cannot get valid commit_id from patch")?,
        )
        .context("cannot get valid commit_id from patch")?;

    // if uptodate
    if proposal_tip.eq(&local_branch_tip) {
        println!("branch already up-to-date");
    }
    // if new appendments
    else if most_recent_proposal_patch_chain.iter().any(|patch| {
        get_commit_id_from_patch(patch)
            .unwrap_or_default()
            .eq(&local_branch_tip.to_string())
    }) {
        check_clean(&git_repo)?;
        let applied = git_repo
            .apply_patch_chain(&branch_name, most_recent_proposal_patch_chain)
            .context("cannot apply patch chain")?;
        println!("applied {} new commits", applied.len(),);
    }
    // if parent commit doesnt exist
    else if !git_repo.does_commit_exist(&proposal_base_commit.to_string())? {
        println!(
            "a new version of the proposal has a prant commit that doesnt exist in your local repository."
        );
        println!("your '{main_branch_name}' branch may not be up-to-date.");
        println!("manually run `git pull` on '{main_branch_name}' and try again");
    }
    // if new revision and no local changes (tip of local in proposal history)
    else if commit_events.iter().any(|patch| {
        get_commit_id_from_patch(patch)
            .unwrap_or_default()
            .eq(&local_branch_tip.to_string())
    }) {
        check_clean(&git_repo)?;

        git_repo.create_branch_at_commit(&branch_name, &proposal_base_commit.to_string())?;
        let applied = git_repo
            .apply_patch_chain(&branch_name, most_recent_proposal_patch_chain)
            .context("cannot apply patch chain")?;

        println!(
            "pulled new version of proposal ({} ahead {} behind '{main_branch_name}'), replacing old version ({} ahead {} behind '{main_branch_name}')",
            applied.len(),
            proposal_behind_main.len(),
            local_ahead_of_main.len(),
            local_beind_main.len(),
        );
    }
    // if tip of proposal in branch in history (local appendments made to up-to-date
    // proposal)
    else if git_repo.ancestor_of(&local_branch_tip, &proposal_tip)? {
        let (local_ahead_of_proposal, _) = git_repo
            .get_commits_ahead_behind(&proposal_tip, &local_branch_tip)
            .context("cannot get commits ahead behind for propsal_top and local_branch_tip")?;
        println!(
            "local proposal branch exists with {} unpublished commits on top of the most up-to-date version of the proposal",
            local_ahead_of_proposal.len()
        );
    } else {
        println!("you have an amended/rebase version the proposal that is unpublished");
        // user probably has a unpublished amended or rebase version of the latest
        // proposal version
        // if tip of proposal commits exist (were once part of branch but have been
        // amended and git clean up job hasn't removed them)
        if git_repo.does_commit_exist(&proposal_tip.to_string())? {
            println!(
                "you have previously applied the latest version of the proposal ({} ahead {} behind '{main_branch_name}') but your local proposal branch has amended or rebased it ({} ahead {} behind '{main_branch_name}')",
                most_recent_proposal_patch_chain.len(),
                proposal_behind_main.len(),
                local_ahead_of_main.len(),
                local_beind_main.len(),
            );
        }
        // user probably has a unpublished amended or rebase version of an older
        // proposal version
        else {
            println!(
                "your local proposal branch ({} ahead {} behind '{main_branch_name}') has conflicting changes with the latest published proposal ({} ahead {} behind '{main_branch_name}')",
                local_ahead_of_main.len(),
                local_beind_main.len(),
                most_recent_proposal_patch_chain.len(),
                proposal_behind_main.len(),
            );

            println!(
                "its likely that you have rebased / amended an old proposal version because git has no record of the latest proposal commit."
            );
            println!(
                "it is possible that you have been working off the latest version and git has delete this commit as part of a clean up"
            );
        }
        println!("to view the latest proposal but retain your changes:");
        println!("  1) create a new branch off the tip commit of this one to store your changes");
        println!("  2) run `ngit list` and checkout the latest published version of this proposal");

        println!("if you are confident in your changes consider running `ngit push --force`");

        // TODO: this copy could be refined further based on this:
        //  - amended commits in the proposal
        //     - if local_base eq proposal base
        //  - amended an older version of proposal
        //     - if local_base is behind proposal_base
        //  - rebased the proposal
        //     - if local_base is ahead of proposal_base
    }
    Ok(())
}

fn check_clean(git_repo: &Repo) -> Result<()> {
    if git_repo.has_outstanding_changes()? {
        bail!(
            "cannot pull proposal branch when repository is not clean. discard or stash (un)staged changes and try again."
        );
    }
    Ok(())
}
