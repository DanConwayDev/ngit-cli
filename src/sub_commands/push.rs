use anyhow::{bail, Context, Result};
use nostr::prelude::sha1::Hash as Sha1Hash;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::Connect,
    git::{str_to_sha1, Repo, RepoActions},
    login,
    repo_ref::{self, RepoRef},
    sub_commands::prs::{
        create::{generate_patch_event, send_events, PATCH_KIND, PR_KIND},
        list::{get_most_recent_patch_with_ancestors, tag_value},
    },
    Cli,
};

pub async fn launch(cli_args: &Cli) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let (main_or_master_branch_name, _) = git_repo
        .get_main_or_master_branch()
        .context("no main or master branch")?;

    let root_commit = git_repo
        .get_root_commit(main_or_master_branch_name)
        .context("failed to get root commit of the repository")?;

    let branch_name = git_repo
        .get_checked_out_branch_name()
        .context("cannot get checked out branch name")?;

    if branch_name == main_or_master_branch_name {
        bail!("checkout a branch associated with a PR first")
    }
    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let repo_ref = repo_ref::fetch(
        &git_repo,
        root_commit.to_string(),
        &client,
        client.get_more_fallback_relays().clone(),
    )
    .await?;

    let (pr_event, commit_events) =
        fetch_pr_and_most_recent_patch_chain(&client, &repo_ref, &root_commit, &branch_name)
            .await?;

    // TODO: fix these scenarios:
    // - local PR branch is 2 behind and 1 ahead. intructions: ...
    // - PR has been rebased. (against commit in main) instructions: ...
    // - PR has been rebased. (against commit not in repo) instructions: ..

    let most_recent_pr_patch_chain = get_most_recent_patch_with_ancestors(commit_events)
        .context("cannot get most recent patch for PR")?;

    let branch_tip = git_repo.get_tip_of_local_branch(&branch_name)?;

    let most_recent_patch_commit_id = str_to_sha1(
        &tag_value(&most_recent_pr_patch_chain[0], "commit")
            .context("latest patch event doesnt have a commit tag")?,
    )
    .context("latest patch event commit tag isn't a valid SHA1 hash")?;

    if most_recent_patch_commit_id.eq(&branch_tip) {
        bail!("nostr pr already up-to-date with local branch");
    }

    if most_recent_pr_patch_chain.iter().any(|e| {
        let c = tag_value(e, "parent-commit").unwrap_or_default();
        c.eq(&branch_tip.to_string())
    }) {
        bail!("nostr pr is ahead of local branch");
    }

    let (ahead, behind) = git_repo
        .get_commits_ahead_behind(&most_recent_patch_commit_id, &branch_tip)
        .context("the latest patch in pr doesnt share an ancestor with your branch.")?;

    if !behind.is_empty() {
        bail!(
            "your local pr branch is {} behind patches on nostr. consider rebasing or force pushing",
            behind.len()
        )
    }

    println!(
        "{} commits ahead. preparing to create creating patch events.",
        ahead.len()
    );

    let (keys, user_ref) = login::launch(&cli_args.nsec, &cli_args.password, Some(&client)).await?;

    client.set_keys(&keys).await;

    let mut patch_events: Vec<nostr::Event> = vec![];
    for commit in &ahead {
        patch_events.push(
            generate_patch_event(&git_repo, &root_commit, commit, pr_event.id, &keys)
                .context("cannot make patch event from commit")?,
        );
    }
    println!("pushing {} commits", ahead.len());

    send_events(
        &client,
        patch_events,
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
    )
    .await?;

    println!("pushed {} commits", ahead.len());

    Ok(())
}

async fn fetch_pr_and_most_recent_patch_chain(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_ref: &RepoRef,
    root_commit: &Sha1Hash,
    branch_name: &String,
) -> Result<(nostr::Event, Vec<nostr::Event>)> {
    println!("finding PR event...");

    let pr_event: nostr::Event = client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PR_KIND))
                    .reference(format!("r-{root_commit}")),
            ],
        )
        .await?
        .iter()
        .find(|e| {
            e.kind.as_u64() == PR_KIND
                && e.tags
                    .iter()
                    .any(|t| t.as_vec().len() > 1 && t.as_vec()[1].eq(&format!("r-{root_commit}")))
                && tag_value(e, "branch-name")
                    .unwrap_or_default()
                    .eq(branch_name)
        })
        .context("cannot find a PR event associated with the checked out branch name")?
        .to_owned();

    println!("found PR event. finding commits...");

    let commits_events: Vec<nostr::Event> = client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .event(pr_event.id)
                    .reference(format!("r-{root_commit}")),
            ],
        )
        .await?
        .iter()
        .filter(|e| {
            e.kind.as_u64() == PATCH_KIND
                && e.tags
                    .iter()
                    .any(|t| t.as_vec().len() > 2 && t.as_vec()[1].eq(&pr_event.id.to_string()))
                && e.tags
                    .iter()
                    .any(|t| t.as_vec().len() > 1 && t.as_vec()[1].eq(&format!("r-{root_commit}")))
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect();
    Ok((pr_event, commits_events))
}
