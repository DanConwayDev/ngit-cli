use anyhow::{bail, Context, Result};

use super::send::event_is_patch_set_root;
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms},
    client::Connect,
    git::{Repo, RepoActions},
    repo_ref::{self, RepoRef, REPO_REF_KIND},
    sub_commands::send::{event_is_cover_letter, event_to_cover_letter, PATCH_KIND},
    Cli,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    /// TODO ignore merged, and closed
    #[arg(long, action)]
    open_only: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(_cli_args: &Cli, _args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    // TODO: check for empty repo
    // TODO: check for existing maintaiers file
    // TODO: check for other claims

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

    println!("finding PRs...");

    let pr_events: Vec<nostr::Event> =
        find_pr_events(&client, &repo_ref, &root_commit.to_string()).await?;

    if pr_events.is_empty() {
        println!("no PRs found... create one? try `ngit send`");
        return Ok(());
    }

    let selected_index = Interactor::default().choice(
        PromptChoiceParms::default()
            .with_prompt("All PRs")
            .with_choices(
                pr_events
                    .iter()
                    .map(|e| {
                        if let Ok(cl) = event_to_cover_letter(e) {
                            cl.title
                        } else if let Ok(msg) = tag_value(e, "description") {
                            msg.split('\n').collect::<Vec<&str>>()[0].to_string()
                        } else {
                            e.id.to_string()
                        }
                    })
                    .collect(),
            ),
    )?;

    println!("finding commits...");

    let commits_events: Vec<nostr::Event> =
        find_commits_for_pr_event(&client, &pr_events[selected_index], &repo_ref).await?;

    confirm_checkout(&git_repo)?;

    let most_recent_pr_patch_chain = get_most_recent_patch_with_ancestors(commits_events)
        .context("cannot get most recent patch for PR")?;

    let branch_name: String = event_to_cover_letter(&pr_events[selected_index])
        .context("cannot assign a branch name as event is not a patch set root")?
        .branch_name;

    let applied = git_repo
        .apply_patch_chain(&branch_name, most_recent_pr_patch_chain)
        .context("cannot apply patch chain")?;

    if applied.is_empty() {
        println!("checked out PR branch. no new commits to pull");
    } else {
        println!(
            "checked out PR branch. pulled {} new commits",
            applied.len(),
        );
    }
    Ok(())
}

fn confirm_checkout(git_repo: &Repo) -> Result<()> {
    if !Interactor::default().confirm(
        PromptConfirmParms::default()
            .with_prompt("check out branch?")
            .with_default(true),
    )? {
        bail!("Exiting...");
    }

    if git_repo.has_outstanding_changes()? {
        bail!(
            "cannot pull PR branch when repository is not clean. discard or stash (un)staged changes and try again."
        );
    }
    Ok(())
}

pub fn tag_value(event: &nostr::Event, tag_name: &str) -> Result<String> {
    Ok(event
        .tags
        .iter()
        .find(|t| t.as_vec()[0].eq(tag_name))
        .context(format!("tag '{tag_name}'not present"))?
        .as_vec()[1]
        .clone())
}

pub fn get_commit_id_from_patch(event: &nostr::Event) -> Result<String> {
    let value = tag_value(event, "commit");

    if value.is_ok() {
        value
    } else if event.content.starts_with("From ") && event.content.len().gt(&45) {
        Ok(event.content[5..45].to_string())
    } else {
        bail!("event is not a patch")
    }
}

fn get_event_parent_id(event: &nostr::Event) -> Result<String> {
    Ok(if let Some(reply_tag) = event
        .tags
        .iter()
        .find(|t| t.as_vec().len().gt(&3) && t.as_vec()[3].eq("reply"))
    {
        reply_tag
    } else {
        event
            .tags
            .iter()
            .find(|t| t.as_vec().len().gt(&3) && t.as_vec()[3].eq("root"))
            .context("no reply or root e tag present".to_string())?
    }
    .as_vec()[1]
        .clone())
}

pub fn get_most_recent_patch_with_ancestors(
    mut patches: Vec<nostr::Event>,
) -> Result<Vec<nostr::Event>> {
    patches.sort_by_key(|e| e.created_at);

    let first_patch = patches.first().context("no patches found")?;

    let patches_with_youngest_created_at: Vec<&nostr::Event> = patches
        .iter()
        .filter(|p| p.created_at.eq(&first_patch.created_at))
        .collect();

    let mut res = vec![];

    let mut event_id_to_search = patches_with_youngest_created_at
        .clone()
        .iter()
        .find(|p| {
            !patches_with_youngest_created_at.iter().any(|p2| {
                if let Ok(reply_to) = get_event_parent_id(p2) {
                    reply_to.eq(&p.id.to_string())
                } else {
                    false
                }
            })
        })
        .context("cannot find patches_with_youngest_created_at")?
        .id
        .to_string();

    while let Some(event) = patches
        .iter()
        .find(|e| e.id.to_string().eq(&event_id_to_search))
    {
        res.push(event.clone());
        event_id_to_search = get_event_parent_id(event).unwrap_or_default();
    }
    Ok(res)
}

pub async fn find_pr_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_ref: &RepoRef,
    root_commit: &str,
) -> Result<Vec<nostr::Event>> {
    Ok(client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .custom_tag(nostr::Alphabet::T, vec!["root"])
                    .custom_tag(
                        nostr::Alphabet::A,
                        repo_ref
                            .maintainers
                            .iter()
                            .map(|m| format!("{REPO_REF_KIND}:{m}:{}", repo_ref.identifier)),
                    ),
                // also pick up prs from the same repo but no target at our maintainers repo events
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .custom_tag(nostr::Alphabet::T, vec!["root"])
                    .reference(root_commit),
            ],
        )
        .await
        .context("cannot get pr events")?
        .iter()
        .filter(|e| {
            event_is_patch_set_root(e)
                && (e
                    .tags
                    .iter()
                    .any(|t| t.as_vec().len() > 1 && t.as_vec()[1].eq(root_commit))
                    || e.tags.iter().any(|t| {
                        t.as_vec().len() > 1
                            && repo_ref
                                .maintainers
                                .iter()
                                .map(|m| format!("{REPO_REF_KIND}:{m}:{}", repo_ref.identifier))
                                .any(|d| t.as_vec()[1].eq(&d))
                    }))
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect::<Vec<nostr::Event>>())
}

pub async fn find_commits_for_pr_event(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    pr_event: &nostr::Event,
    repo_ref: &RepoRef,
) -> Result<Vec<nostr::Event>> {
    let mut patch_events: Vec<nostr::Event> = client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    // this requires every patch to reference the root event
                    // this will not pick up v2,v3 patch sets
                    // TODO: fetch commits for v2.. patch sets
                    .event(pr_event.id),
            ],
        )
        .await
        .context("cannot fetch patch events")?
        .iter()
        .filter(|e| {
            e.kind.as_u64() == PATCH_KIND
                && e.tags
                    .iter()
                    .any(|t| t.as_vec().len() > 2 && t.as_vec()[1].eq(&pr_event.id.to_string()))
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect();

    if !event_is_cover_letter(pr_event) {
        patch_events.push(pr_event.clone());
    }
    Ok(patch_events)
}
