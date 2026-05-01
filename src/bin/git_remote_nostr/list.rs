use std::collections::HashMap;

use anyhow::{Context, Result};
use client::get_state_from_cache;
use git::RepoActions;
use ngit::{
    client::{self, FetchReport, is_verbose},
    fetch::fetch_from_git_server,
    git::{self},
    git_events::{KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, event_to_cover_letter, tag_value},
    list::list_from_remotes,
    login::get_curent_user,
    repo_ref::{self},
    repo_state::RepoState,
    utils::{get_all_proposals, get_open_or_draft_proposals},
};
use repo_ref::RepoRef;

use crate::{fetch::make_commits_for_proposal, git::Repo};

#[allow(clippy::too_many_lines)]
pub async fn run_list(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    for_push: bool,
    fetch_report: &FetchReport,
) -> Result<HashMap<String, (HashMap<String, String>, bool)>> {
    let nostr_state = (get_state_from_cache(Some(git_repo.get_path()?), repo_ref).await).ok();

    let term = console::Term::stderr();

    if is_verbose() {
        term.write_line("git servers: listing refs...")?;
    }
    let nostr_git_url = repo_ref.to_nostr_git_url(&None);
    // nostr_state is passed to list_from_remotes only for the sync-status
    // display; the actual ref state we advertise is determined below.
    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        &nostr_git_url,
        nostr_state.as_ref(),
    )
    .await;

    // Collect all OIDs confirmed present on at least one git server.
    let git_server_oids: std::collections::HashSet<String> = remote_states
        .values()
        .flat_map(|(state, _)| state.values())
        .filter(|v| !v.starts_with("ref: "))
        .cloned()
        .collect();

    // From the per-relay state events captured during the nostr fetch, find
    // the newest state event whose every OID is either:
    //   (a) confirmed present on at least one git server, or
    //   (b) already available locally.
    // This prevents advertising refs whose git objects haven't been pushed to
    // any server yet, which would cause `git clone` / `git fetch` to fail.
    //
    // filter by maintainers to avoid state events from other remotes with the
    // same identifier being selected when they have a newer created_at
    let mut candidates: Vec<&nostr::Event> = fetch_report
        .state_per_relay
        .values()
        .filter_map(|maybe| maybe.as_ref())
        .filter(|event| repo_ref.maintainers.contains(&event.pubkey))
        .collect();
    // Sort newest-first (by created_at, then by id for tie-breaking).
    candidates.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    // Deduplicate by event id so we don't check the same event twice.
    candidates.dedup_by_key(|e| e.id);

    let best_state: Option<HashMap<String, String>> = candidates.into_iter().find_map(|event| {
        if let Ok(rs) = RepoState::try_from(vec![event.clone()]) {
            let all_resolvable = rs.state.values().all(|v| {
                v.starts_with("ref: ")
                    || git_server_oids.contains(v)
                    || git_repo.does_commit_exist(v).is_ok_and(|exists| exists)
            });
            if all_resolvable { Some(rs.state) } else { None }
        } else {
            None
        }
    });

    let mut state = if let Some(state) = best_state {
        state
    } else {
        // No relay returned a state event whose OIDs are all resolvable
        // (either no state events were seen on any relay, or every candidate
        // references git objects not yet on any server).  Fall back to
        // whatever the git servers actually report so we never advertise OIDs
        // that cannot be fetched.
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
#[allow(clippy::too_many_lines)]
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

    let open_and_draft_proposals = get_open_or_draft_proposals(git_repo, repo_ref).await?;

    // Collect PR/PR-update tip OIDs that are still missing after the bulk prefetch.
    // We borrow proposals here so we can move them in the state-building loop
    // below.
    let mut missing_pr_oids: Vec<String> = open_and_draft_proposals
        .values()
        .filter_map(|(_, events)| {
            events
                .iter()
                .find(|e| e.kind.eq(&KIND_PULL_REQUEST) || e.kind.eq(&KIND_PULL_REQUEST_UPDATE))
                .and_then(|e| tag_value(e, "c").ok())
        })
        .filter(|tip| !git_repo.does_commit_exist(tip).unwrap_or(false))
        .collect();

    // For each repo git server, batch-fetch the PR tip OIDs it carries that are
    // still missing locally. Only OIDs the server has advertised are included in
    // each batch (avoids all-or-nothing batch-poisoning). We mop up across servers
    // until all missing OIDs are satisfied or all servers are exhausted.
    //
    // NOTE: we intentionally restrict mop-up to the repo's declared git servers
    // (remote_states) and do NOT try the git-server URL carried in the PR event's
    // `clone` tag. A PR submitter could include an arbitrary server URL there;
    // fetching from it unconditionally would let a malicious or slow server
    // delay every clone/fetch. If we later want to support PR-supplied servers,
    // it should be opt-in (e.g. an explicit `--include-pr-servers` flag) so
    // users consciously accept the trust/performance trade-off. PRs whose tip
    // OID isn't carried by any repo git server will simply not be advertised as
    // `refs/heads/pr/*` refs; they are still accessible via their patch events.
    if !missing_pr_oids.is_empty() {
        for (server_url, (server_state, is_grasp)) in remote_states {
            let batch: Vec<String> = missing_pr_oids
                .iter()
                .filter(|oid| server_state.values().any(|v| v == *oid))
                .cloned()
                .collect();
            if batch.is_empty() {
                continue;
            }
            let _ = fetch_from_git_server(
                git_repo,
                &batch,
                server_url,
                &repo_ref.to_nostr_git_url(&None),
                term,
                *is_grasp,
            );
            missing_pr_oids.retain(|oid| !git_repo.does_commit_exist(oid).unwrap_or(false));
            if missing_pr_oids.is_empty() {
                break;
            }
        }
    }

    let mut state = HashMap::new();
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
                            // Only advertise once confirmed locally available — this
                            // guarantees the subsequent fetch phase can serve the object.
                            if git_repo.does_commit_exist(&tip).is_ok_and(|r| r) {
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
    for (proposal, events_to_apply) in all_proposals.values() {
        if let Ok(cl) = event_to_cover_letter(proposal) {
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
                    make_commits_for_proposal(git_repo, repo_ref, events_to_apply)
                {
                    state.insert(format!("refs/{branch_name}"), tip.clone());
                    state.insert(format!("refs/pr/{}/head", proposal.id), tip);
                }
            }
        }
    }
    Ok(state)
}
