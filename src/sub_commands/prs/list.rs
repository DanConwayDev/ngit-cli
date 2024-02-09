use anyhow::{bail, Context, Result};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms},
    client::Connect,
    git::{Repo, RepoActions},
    repo_ref,
    sub_commands::prs::create::{PATCH_KIND, PR_KIND},
    Cli,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    /// TODO ignore merged, and closed
    #[arg(long, action)]
    open_only: bool,
}

pub async fn launch(
    _cli_args: &Cli,
    _pr_args: &super::SubCommandArgs,
    _args: &SubCommandArgs,
) -> Result<()> {
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

    let pr_events: Vec<nostr::Event> = client
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
        .filter(|e| {
            e.kind.as_u64() == PR_KIND
                && e.tags
                    .iter()
                    .any(|t| t.as_vec().len() > 1 && t.as_vec()[1].eq(&format!("r-{root_commit}")))
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect();

    // let pr_branch_names: Vec<String> = pr_events
    //     .iter()
    //     .map(|e| {
    //         format!(
    //             "{}-{}",
    //             &e.id.to_string()[..5],
    //             if let Some(t) = e.tags.iter().find(|t| t.as_vec()[0] ==
    // "branch-name") {                 t.as_vec()[1].to_string()
    //             } else {
    //                 "".to_string()
    //             } // git_repo.get_checked_out_branch_name(),
    //         )
    //     })
    //     .collect();

    let selected_index = Interactor::default().choice(
        PromptChoiceParms::default()
            .with_prompt("All PRs")
            .with_choices(
                pr_events
                    .iter()
                    .map(|e| {
                        if let Ok(name) = tag_value(e, "name") {
                            name
                        } else {
                            e.id.to_string()
                        }
                    })
                    .collect(),
            ),
    )?;
    // println!("prs:{:?}", &pr_events);

    println!("finding commits...");

    let commits_events: Vec<nostr::Event> = client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .event(pr_events[selected_index].id),
            ],
        )
        .await?
        .iter()
        .filter(|e| {
            e.kind.as_u64() == PATCH_KIND
                && e.tags.iter().any(|t| {
                    t.as_vec().len() > 2
                        && t.as_vec()[1].eq(&pr_events[selected_index].id.to_string())
                })
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect();

    confirm_checkout(&git_repo)?;

    let most_recent_pr_patch_chain = get_most_recent_patch_with_ancestors(commits_events)
        .context("cannot get most recent patch for PR")?;

    let branch_name = tag_value(&pr_events[selected_index], "branch-name")?;

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

pub fn get_most_recent_patch_with_ancestors(
    mut patches: Vec<nostr::Event>,
) -> Result<Vec<nostr::Event>> {
    patches.sort_by_key(|e| e.created_at);

    let mut res = vec![];

    let latest_commit_id = tag_value(patches.first().context("no patches found")?, "commit")?;

    let mut commit_id_to_search = latest_commit_id;

    while let Some(event) = patches.iter().find(|e| {
        tag_value(e, "commit")
            .context("patch event doesnt contain commit tag")
            .unwrap()
            .eq(&commit_id_to_search)
    }) {
        res.push(event.clone());
        commit_id_to_search = tag_value(event, "parent-commit")?;
    }
    Ok(res)
}
