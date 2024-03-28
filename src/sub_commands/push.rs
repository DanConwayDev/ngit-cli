use anyhow::{bail, Context, Result};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::Connect,
    git::{str_to_sha1, Repo, RepoActions},
    login,
    repo_ref::{self, RepoRef},
    sub_commands::{
        self,
        list::{
            find_commits_for_proposal_root_events, find_proposal_events, get_commit_id_from_patch,
            get_most_recent_patch_with_ancestors, tag_value,
        },
        send::{event_is_revision_root, event_to_cover_letter, generate_patch_event, send_events},
    },
    Cli,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[arg(long, action)]
    /// send proposal revision from checked out proposal branch
    force: bool,
    #[arg(long, action)]
    /// dont prompt for cover letter when force pushing
    no_cover_letter: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
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
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let repo_ref = repo_ref::fetch(
        &git_repo,
        root_commit.to_string(),
        &client,
        client.get_fallback_relays().clone(),
        true,
    )
    .await?;

    let (proposal_root_event, commit_events) = fetch_proposal_root_and_most_recent_patch_chain(
        &client,
        &repo_ref,
        &root_commit,
        &branch_name,
    )
    .await?;

    let most_recent_proposal_patch_chain = get_most_recent_patch_with_ancestors(commit_events)
        .context("cannot get most recent patch for proposal")?;

    let branch_tip = git_repo.get_tip_of_branch(&branch_name)?;

    let most_recent_patch_commit_id = str_to_sha1(
        &get_commit_id_from_patch(
            most_recent_proposal_patch_chain
                .first()
                .context("no patches found")?,
        )
        .context("latest patch event doesnt have a commit tag")?,
    )
    .context("latest patch event commit tag isn't a valid SHA1 hash")?;

    let proposal_base_commit_id = str_to_sha1(
        &tag_value(
            most_recent_proposal_patch_chain
                .last()
                .context("no patches found")?,
            "parent-commit",
        )
        .context("patch is incorrectly formatted")?,
    )
    .context("latest patch event parent-commit tag isn't a valid SHA1 hash")?;

    if most_recent_patch_commit_id.eq(&branch_tip) {
        bail!("proposal already up-to-date with local branch");
    }

    if args.force {
        println!("preparing to force push proposal revision...");
        sub_commands::send::launch(
            cli_args,
            &sub_commands::send::SubCommandArgs {
                since_or_range: String::new(),
                in_reply_to: Some(proposal_root_event.id.to_string()),
                title: None,
                description: None,
                no_cover_letter: args.no_cover_letter,
            },
        )
        .await?;
        println!("force pushed proposal revision");
        return Ok(());
    }

    if most_recent_proposal_patch_chain.iter().any(|e| {
        let c = tag_value(e, "parent-commit").unwrap_or_default();
        c.eq(&branch_tip.to_string())
    }) {
        bail!("proposal is ahead of local branch");
    }

    let Ok((ahead, behind)) = git_repo
        .get_commits_ahead_behind(&most_recent_patch_commit_id, &branch_tip)
        .context("the latest patch in proposal doesnt share an ancestor with your branch.")
    else {
        if git_repo.ancestor_of(&proposal_base_commit_id, &branch_tip)? {
            bail!("local unpublished proposal ammendments. consider force pushing.");
        }
        bail!("local unpublished proposal has been rebased. consider force pushing");
    };

    if !behind.is_empty() {
        bail!(
            "your local proposal branch is {} behind patches on nostr. consider rebasing or force pushing",
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
            generate_patch_event(
                &git_repo,
                &root_commit,
                commit,
                Some(proposal_root_event.id),
                &keys,
                &repo_ref,
                patch_events.last().map(nostr::Event::id),
                None,
                None,
                &None,
            )
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

pub async fn fetch_proposal_root_and_most_recent_patch_chain(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_ref: &RepoRef,
    root_commit: &Sha1Hash,
    branch_name: &String,
) -> Result<(nostr::Event, Vec<nostr::Event>)> {
    println!("finding proposal root event...");

    let proposal_events_and_revisions: Vec<nostr::Event> =
        find_proposal_events(client, repo_ref, &root_commit.to_string())
            .await
            .context("cannot get proposal events for repo")?;

    let proposal_events: Vec<&nostr::Event> = proposal_events_and_revisions
        .iter()
        .filter(|e| !event_is_revision_root(e))
        .collect::<Vec<&nostr::Event>>();

    let proposal_root_event: &nostr::Event = proposal_events
        .iter()
        .find(|e| {
            event_to_cover_letter(e).is_ok_and(|cl| cl.branch_name.eq(branch_name))
            // TODO remove the dependancy on same branch name and replace with
            // references stored in .git/ngit
        })
        .context("cannot find a proposal root event associated with the checked out branch name")?
        .to_owned();

    println!("found proposal root event. finding commits...");

    let commits_events: Vec<nostr::Event> = find_commits_for_proposal_root_events(
        client,
        &[
            vec![proposal_root_event],
            proposal_events_and_revisions
                .iter()
                .filter(|e| {
                    e.tags.iter().any(|t| {
                        t.as_vec().len().gt(&1)
                            && t.as_vec()[1].eq(&proposal_root_event.id.to_string())
                    })
                })
                .collect::<Vec<&nostr::Event>>(),
        ]
        .concat(),
        repo_ref,
    )
    .await?;

    Ok((proposal_root_event.clone(), commits_events))
}
