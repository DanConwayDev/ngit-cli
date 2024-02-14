use anyhow::{bail, Context, Result};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::Connect,
    git::{Repo, RepoActions},
    repo_ref,
    sub_commands::{
        list::get_most_recent_patch_with_ancestors, push::fetch_pr_and_most_recent_patch_chain,
    },
};

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
        bail!("checkout a branch associated with a PR first")
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

    let (_pr_event, commit_events) =
        fetch_pr_and_most_recent_patch_chain(&client, &repo_ref, &root_commit, &branch_name)
            .await?;

    if git_repo.has_outstanding_changes()? {
        bail!("cannot pull changes when repository is not clean. discard changes and try again.");
    }

    let most_recent_pr_patch_chain = get_most_recent_patch_with_ancestors(commit_events)
        .context("cannot get most recent patch for PR")?;

    let applied = git_repo
        .apply_patch_chain(&branch_name, most_recent_pr_patch_chain)
        .context("cannot apply patch chain")?;

    if applied.is_empty() {
        println!("branch already up-to-date");
    } else {
        println!("applied {} new commits", applied.len(),);
    }

    Ok(())
}
