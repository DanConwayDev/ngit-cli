use std::collections::HashMap;

use anyhow::{Context, Result};
use client::get_state_from_cache;
use git::RepoActions;
use ngit::{
    client,
    fetch::fetch_from_git_server,
    git::{self},
    git_events::{KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, event_to_cover_letter, tag_value},
    list::list_from_remotes,
    login::get_curent_user,
    repo_ref::{self},
    utils::{get_all_proposals, get_open_or_draft_proposals},
};
use repo_ref::RepoRef;

use crate::{fetch::make_commits_for_proposal, git::Repo};

#[allow(clippy::too_many_lines)]
pub async fn run_list(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    for_push: bool,
) -> Result<HashMap<String, (HashMap<String, String>, bool)>> {
    let nostr_state = (get_state_from_cache(Some(git_repo.get_path()?), repo_ref).await).ok();

    let term = console::Term::stderr();

    term.write_line("git servers: listing refs...")?;
    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        &repo_ref.to_nostr_git_url(&None),
        nostr_state.as_ref(),
    )
    .await;

    let mut state = if let Some(nostr_state) = nostr_state {
        nostr_state.state
    } else {
        let (state, _is_grasp_server) = repo_ref
            .git_server
            .iter()
            .filter_map(|server| remote_states.get(server))
            .cloned()
            .collect::<Vec<(HashMap<String, String>, bool)>>()
            .first()
            .context("failed to get refs from git server")?
            .clone();
        state
    };

    state.retain(|k, _| !k.starts_with("refs/heads/pr/"));

    state.extend(
        // get as refs/heads/pr/<branch-name>(<shorthand-event-id>)
        get_open_and_draft_proposals_state(&term, git_repo, repo_ref, &remote_states).await?,
    );

    state.extend(
        // get as refs/pr/<branch-name>(<shorthand-event-id>) and refs/pr/<event-id>/head
        get_all_proposals_state(git_repo, repo_ref).await?,
    );

    // TODO 'for push' should we check with the git servers to see if any of them
    // allow push from the user?
    for (name, value) in state {
        if value.starts_with("ref: ") {
            if !for_push {
                println!("{} {name}", value.replace("ref: ", "@"));
            }
        } else {
            println!("{value} {name}");
        }
    }

    println!();
    Ok(remote_states)
}

/// fetches branches and tags from git servers so patch parent commits can be
/// used to build patches with correct commit ids
async fn get_open_and_draft_proposals_state(
    term: &console::Term,
    git_repo: &Repo,
    repo_ref: &RepoRef,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
) -> Result<HashMap<String, String>> {
    // we cannot use commit_id in the latest patch in a proposal because:
    // 1) the `commit` tag is optional
    // 2) if the commit tag is wrong, it will cause errors which stop clone from
    //    working

    // without trusting commit_id we must apply each patch which requires the oid of
    // the parent so we much do a fetch

    for (git_server_url, (oids_from_git_servers, is_grasp_server)) in remote_states {
        if fetch_from_git_server(
            git_repo,
            &oids_from_git_servers
                .values()
                .filter(|v| !v.starts_with("ref: "))
                .cloned()
                .collect::<Vec<String>>(),
            // TODO we could fetch the oids of Pull Requests and Pull Request Updates to prevent
            // having repeat fetching during the git remote helper fetch phase
            git_server_url,
            &repo_ref.to_nostr_git_url(&None),
            term,
            *is_grasp_server,
        )
        .is_ok()
        {
            break;
        }
    }

    let mut state = HashMap::new();
    let open_and_draft_proposals = get_open_or_draft_proposals(git_repo, repo_ref).await?;
    let current_user = get_curent_user(git_repo)?;
    for (_, (proposal, events_to_apply)) in open_and_draft_proposals {
        if let Ok(cl) = event_to_cover_letter(&proposal) {
            if let Ok(mut branch_name) = cl.get_branch_name_with_pr_prefix_and_shorthand_id() {
                branch_name = if let Some(public_key) = current_user {
                    if proposal.pubkey.eq(&public_key) {
                        format!("pr/{}", cl.branch_name_without_id_or_prefix)
                    } else {
                        branch_name
                    }
                } else {
                    branch_name
                };
                // if events_to_apply contains a PR or PR Update event it should be the only
                // event in the Vec
                if let Some(pr_or_pr_update) = events_to_apply
                    .iter()
                    .find(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
                {
                    match tag_value(pr_or_pr_update, "c") {
                        Ok(tip) => {
                            // only list Pull Requests as refs/heads/pr/* if data is commit is
                            // advertised as tip of a ref on a repo git server or
                            // available locally. Otherwise the standard cmd:
                            // `git clone nostr://` will fail as it assumes all /refs/heads
                            // returned by list are accessable
                            let tip_oid_is_on_a_repo_git_server =
                                remote_states.iter().any(|(_url, (state, _is_grasp))| {
                                    state.iter().any(|(_, oid)| tip == *oid)
                                }) || git_repo.does_commit_exist(&tip).is_ok_and(|r| r);

                            if tip_oid_is_on_a_repo_git_server {
                                state.insert(format!("refs/heads/{branch_name}"), tip);
                            }
                        }
                        Err(_) => {
                            let _ = term.write_line(
                                format!(
                                    "WARNING: failed to fetch branch {branch_name} error: {} event poorly formatted",
                                    if pr_or_pr_update.kind.eq(&KIND_PULL_REQUEST) {
                                        "PR"
                                    } else {
                                        "PR update"
                                    }
                                )
                                .as_str(),
                            );
                        }
                    }
                } else {
                    match make_commits_for_proposal(git_repo, repo_ref, &events_to_apply) {
                        Ok(tip) => {
                            state.insert(format!("refs/heads/{branch_name}"), tip);
                        }
                        Err(error) => {
                            if let Ok(Some(public_key)) = get_curent_user(git_repo) {
                                if repo_ref.maintainers.contains(&public_key)
                                    || events_to_apply.iter().any(|e| e.pubkey.eq(&public_key))
                                {
                                    term.write_line(
                                        format!("WARNING (only shown to maintainers or author): failed to fetch branch {branch_name}, error: {error}",)
                                            .as_str(),
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(state)
}

/// we assume latest default branch oid has been fetched so patch parent commits
/// are present. doesnt report on proposals failed to recreate
async fn get_all_proposals_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<String, String>> {
    let mut state = HashMap::new();
    let all_proposals = get_all_proposals(git_repo, repo_ref).await?;
    let current_user = get_curent_user(git_repo)?;
    for (_, (proposal, events_to_apply)) in all_proposals {
        if let Ok(cl) = event_to_cover_letter(&proposal) {
            if let Ok(mut branch_name) = cl.get_branch_name_with_pr_prefix_and_shorthand_id() {
                branch_name = if let Some(public_key) = current_user {
                    if proposal.pubkey.eq(&public_key) {
                        format!("pr/{}", cl.branch_name_without_id_or_prefix)
                    } else {
                        branch_name
                    }
                } else {
                    branch_name
                };
                if let Some(pr_or_pr_update) = events_to_apply
                    .iter()
                    .find(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
                {
                    if let Ok(tip) = tag_value(pr_or_pr_update, "c") {
                        state.insert(format!("refs/{branch_name}"), tip.clone());
                        state.insert(format!("refs/pr/{}/head", proposal.id), tip);
                    }
                } else if let Ok(tip) =
                    make_commits_for_proposal(git_repo, repo_ref, &events_to_apply)
                {
                    state.insert(format!("refs/{branch_name}"), tip.clone());
                    state.insert(format!("refs/pr/{}/head", proposal.id), tip);
                }
            }
        }
    }
    Ok(state)
}
