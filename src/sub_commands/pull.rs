use anyhow::{bail, Context, Result};

use super::list::{get_commit_id_from_patch, tag_value};
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::Connect,
    git::{str_to_sha1, Repo, RepoActions},
    repo_ref,
    sub_commands::{
        list::get_most_recent_patch_with_ancestors,
        push::fetch_proposal_root_and_most_recent_patch_chain,
    },
};

#[allow(clippy::too_many_lines)]
pub async fn launch() -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let (main_or_master_branch_name, _) = git_repo
        .get_main_or_master_branch()
        .context("no main or master branch")?;

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    let branch_name = git_repo
        .get_checked_out_branch_name()
        .context("cannot get checked out branch name")?;

    if branch_name == main_or_master_branch_name {
        bail!("checkout a branch associated with a proposal first")
    }
    #[cfg(not(test))]
    let client = Client::default();
    #[cfg(test)]
    let client = <MockConnect as std::default::Default>::default();

    let repo_ref = repo_ref::fetch(
        &git_repo,
        root_commit.to_string(),
        &client,
        client.get_fallback_relays().clone(),
    )
    .await?;

    let (_proposal_root_event, commit_events) = fetch_proposal_root_and_most_recent_patch_chain(
        &client,
        &repo_ref,
        &root_commit,
        &branch_name,
    )
    .await?;

    let most_recent_proposal_patch_chain =
        get_most_recent_patch_with_ancestors(commit_events.clone())
            .context("cannot get most recent patch for proposal")?;

    let local_branch_tip = git_repo.get_tip_of_local_branch(&branch_name)?;

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
    // if tip of local in proposal history (new, ammended or rebased version but no
    // local changes)
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
    else if let Ok((local_ahead_of_proposal, _)) =
        git_repo.get_commits_ahead_behind(&proposal_tip, &local_branch_tip)
    {
        println!(
            "local proposal branch exists with {} unpublished commits on top of the most up-to-date version of the proposal",
            local_ahead_of_proposal.len()
        );
    }
    // user has probably has an unpublished rebase of the latest proposal version
    // if tip of proposal commits exist (were once part of branch but have been
    // ammended and git clean up job hasn't removed them)
    else if git_repo.does_commit_exist(&proposal_tip.to_string())? {
        println!(
            "you have previously applied the latest version of the proposal ({} ahead {} behind '{main_branch_name}') but your local proposal branch has other unpublished changes ({} ahead {} behind '{main_branch_name}')",
            most_recent_proposal_patch_chain.len(),
            proposal_behind_main.len(),
            local_ahead_of_main.len(),
            local_beind_main.len(),
        );
        println!(
            "if this sounds right then consider publishing your rebase `ngit push --force` or discarding your local branch"
        );
    }
    // user has probaly has an unpublished rebase of an older version of the
    // proposal
    else {
        println!(
            "your local proposal branch ({} ahead {} behind '{main_branch_name}') has conflicting changes with the latest published proposal ({} ahead {} behind '{main_branch_name}')",
            local_ahead_of_main.len(),
            local_beind_main.len(),
            most_recent_proposal_patch_chain.len(),
            proposal_behind_main.len(),
        );
        println!(
            "its likely that you are working off an old proposal version because git has no record of the latest proposal commit."
        );
        println!(
            "it is possible that you have ammended the latest version and git has delete this commit as part of a clean up"
        );

        println!("to view the latest proposal but retain your changes:");
        println!("  1) create a new branch off the tip commit of this one to store your changes");
        println!("  2) run `ngit list` and checkout the latest published version of this proposal");

        println!("if you are confident in your changes consider running `ngit push --force`");
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
