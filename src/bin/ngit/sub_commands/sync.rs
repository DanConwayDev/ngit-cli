use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use console::Term;
use git2::Oid;
use ngit::{
    client::{
        Client, Connect, Params, fetching_with_report, get_repo_ref_from_cache,
        get_state_from_cache, send_events,
    },
    fetch::fetch_from_git_server,
    git::{Repo, RepoActions, nostr_url::NostrUrlDecoded},
    list::{get_ahead_behind, list_from_remotes},
    login::existing::load_existing_login,
    push::push_to_remote,
    repo_ref::{
        format_grasp_server_url_as_relay_url, get_repo_coordinates_when_remote_unknown,
        is_grasp_server_clone_url,
    },
    repo_state::RepoState,
    utils::{get_short_git_server_name, join_with_and},
};
use nostr_sdk::RelayUrl;

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    /// optionally just sync a specific reference. eg main or v1.5.2
    #[clap(short, long)]
    pub(crate) ref_name: Option<String>,
    /// force push updates and delete refs from non-grasp git servers
    #[arg(long, action)]
    force: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let full_ref_name = if let Some(ref_name) = &args.ref_name {
        if ref_name.starts_with("refs/") {
            if git_repo.git_repo.find_reference(ref_name).is_ok() {
                Some(ref_name.clone())
            } else {
                bail!("could not find reference {ref_name}");
            }
        } else if git_repo
            .git_repo
            .find_reference(&format!("refs/tags/{ref_name}"))
            .is_ok()
        {
            Some(format!("refs/tags/{ref_name}"))
        } else if git_repo
            .git_repo
            .find_reference(&format!("refs/heads/{ref_name}"))
            .is_ok()
        {
            Some(format!("refs/heads/{ref_name}"))
        } else {
            bail!("could not find reference {ref_name}");
        }
    } else {
        None
    };

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (nostr_remote_name, decoded_nostr_url) = git_repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await.context("failed to list git remotes")?
        .context("no `nostr://` remote detected. `ngit sync` must be run from a repo with a nostr remote")?;

    let repo_coordinate = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    let fetch_report = fetching_with_report(git_repo_path, &client, &repo_coordinate).await?;

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await?;

    let nostr_state = get_state_from_cache(Some(git_repo_path), &repo_ref).await?;

    // When --force is given, rebuild and republish the state event even if
    // nothing has changed.  This lets users repair repos whose state event is
    // missing ^{} peeled refs for annotated tags (or any other corruption)
    // without needing to push a new ref.  A fresh event is signed (new
    // created_at) and broadcast to all repo relays and the user's write relays.
    if args.force {
        let (signer, user_ref, _) = load_existing_login(
            &Some(&git_repo),
            &None,
            &None,
            &None,
            Some(&client),
            false, // not silent — we need the user to authenticate if required
            false, // prompt_for_password
            false, // fetch_profile_updates
        )
        .await
        .context("authentication required to republish state; run 'ngit account login' first")?;
        client.set_signer(signer.clone()).await;
        // Backfill any missing ^{} peeled refs before rebuilding — the existing
        // state event may predate the fix that started storing them.
        let mut state = nostr_state.state.clone();
        let tag_refs: Vec<(String, String)> = state
            .iter()
            .filter(|(k, _)| k.starts_with("refs/tags/") && !k.ends_with("^{}"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (ref_name, tag_oid) in tag_refs {
            let peeled_key = format!("{ref_name}^{{}}");
            if state.contains_key(&peeled_key) {
                continue;
            }
            if let Ok(oid) = git2::Oid::from_str(&tag_oid) {
                if git_repo
                    .git_repo
                    .find_object(oid, Some(git2::ObjectType::Tag))
                    .is_ok()
                {
                    if let Ok(commit_oid) = git_repo.get_commit_or_tip_of_reference(&ref_name) {
                        state.insert(peeled_key, commit_oid.to_string());
                    }
                }
            }
        }
        let new_state = RepoState::build(repo_ref.identifier.clone(), state, &signer).await?;
        send_events(
            &client,
            Some(git_repo_path),
            vec![new_state.event],
            user_ref.relays.write(),
            repo_ref.relays.clone(),
            true,
            false,
        )
        .await?;
        println!("state event republished");
    }

    // Publish the current state event to any grasp server relays that are
    // missing it or have a stale version.  Grasp servers reject git pushes
    // unless the state event is already present on their relay, so we must
    // do this before attempting any git push.
    //
    // We use the per-relay state events captured during the fetch rather than
    // the local database, because the database only stores the canonical latest
    // event and cannot tell us what each individual relay holds.
    let grasp_relays_needing_state: Vec<RelayUrl> = repo_ref
        .git_server
        .iter()
        .filter(|url| is_grasp_server_clone_url(url))
        .filter_map(|url| {
            format_grasp_server_url_as_relay_url(url)
                .ok()
                .and_then(|relay_str| RelayUrl::parse(&relay_str).ok())
        })
        .filter(|relay_url| {
            // Include this relay if it was absent from the fetch results, had
            // no state event, or had a state event older than the canonical one.
            match fetch_report.state_per_relay.get(relay_url) {
                // relay wasn't queried, or returned no state event
                None | Some(None) => true,
                Some(Some(relay_event)) => relay_event.id != nostr_state.event.id,
            }
        })
        .collect();

    // relay URL -> whether the state event was successfully published to it.
    // Only populated for grasp relays that needed the state event; grasp
    // relays that already had the current state event are considered succeeded.
    let mut grasp_relay_publish_results: HashMap<String, bool> = HashMap::new();

    if !grasp_relays_needing_state.is_empty() {
        // Attempt to load an existing login silently so the signer is
        // available for NIP-42 auth if a relay requests it.  We do not
        // prompt the user, do not fetch profile updates, and ignore any
        // failure — the events are already signed so publishing works
        // without a signer.
        if let Ok((signer, _, _)) = load_existing_login(
            &Some(&git_repo),
            &None,
            &None,
            &None,
            Some(&client),
            true,  // silent
            false, // prompt_for_password
            false, // fetch_profile_updates
        )
        .await
        {
            client.set_signer(signer.clone()).await;
        }
        // Send only to the specific grasp relays that are missing or have a
        // stale state event — no user write relays.
        if let Ok(results) = send_events(
            &client,
            Some(git_repo_path),
            vec![nostr_state.event.clone()],
            vec![], // no user write relays
            grasp_relays_needing_state,
            true,
            false,
        )
        .await
        {
            for (relay_url, succeeded) in results {
                grasp_relay_publish_results.insert(relay_url, succeeded);
            }
        }
    }

    let term = console::Term::stderr();

    let remote_states = list_from_remotes(
        &term,
        &git_repo,
        &repo_ref.git_server,
        &decoded_nostr_url,
        Some(&nostr_state),
    )
    .await;

    let missing_refs =
        fetch_missing_refs(&git_repo, &nostr_state, &remote_states, &decoded_nostr_url);

    for (url, (remote_state, is_grasp_server)) in &remote_states {
        let remote_name = get_short_git_server_name(url);
        let mut refspecs = vec![];
        // delete ref from remote
        let mut not_deleted = vec![];
        for remote_ref_name in remote_state.keys() {
            // skip peeled-tag dereference markers — not real refs
            if remote_ref_name.ends_with("^{}") {
                continue;
            }
            // skip unspecified refs
            if let Some(full_ref_name) = &full_ref_name {
                if remote_ref_name != full_ref_name {
                    continue;
                }
            }
            if (!remote_ref_name.starts_with("refs/heads/pr/")
                && (remote_ref_name.starts_with("refs/heads/")
                    || remote_ref_name.starts_with("refs/tags/")))
                && !nostr_state
                    .state
                    .keys()
                    .any(|nostr_ref| nostr_ref.eq(remote_ref_name))
            {
                if *is_grasp_server || args.force {
                    // delete branches / tags not on nostr
                    refspecs.push(format!(":{remote_ref_name}"));
                } else {
                    not_deleted.push(remote_ref_name);
                }
            }
        }
        // add or update ref on remote
        let mut not_updated = vec![];
        for nostr_ref_name in nostr_state.state.keys() {
            // skip peeled-tag dereference markers (e.g. refs/tags/v1.0.0^{})
            // — these are not real git refs and cannot appear in refspecs
            if nostr_ref_name.ends_with("^{}") {
                continue;
            }
            // skip unspecified refs
            if let Some(full_ref_name) = &full_ref_name {
                if nostr_ref_name != full_ref_name {
                    continue;
                }
            }
            // skip refs missing locally
            if missing_refs.contains(nostr_ref_name) {
                continue;
            }
            // strip refs/heads/ or refs/tags/ prefix to get the tracking ref segment
            // e.g. refs/heads/master -> master, refs/tags/v1.0.0 -> v1.0.0
            let tracking_ref_name = nostr_ref_name
                .strip_prefix("refs/heads/")
                .or_else(|| nostr_ref_name.strip_prefix("refs/tags/"))
                .unwrap_or(nostr_ref_name.as_str());
            if invalid_nostr_state_ref(nostr_ref_name) {
                // ensure nostr_state only supports refs/heads and refs/tags/
                // and not refs/heads/prs/*
            } else if let Some(remote_ref_value) = remote_state.get(nostr_ref_name) {
                // update ref
                let force_required = {
                    if let Ok((ahead, _)) =
                        get_ahead_behind(&git_repo, nostr_ref_name, remote_ref_value)
                    {
                        !ahead.is_empty()
                    } else {
                        true
                    }
                };
                if nostr_state
                    .state
                    .get(nostr_ref_name)
                    .is_none_or(|nostr_ref_value| nostr_ref_value.eq(remote_ref_value))
                {
                    // no action if ref in sync
                } else if remote_ref_value.starts_with("ref ") && !(args.force || *is_grasp_server)
                {
                    // dont try and sync push symbolic refs
                } else if !force_required {
                    refspecs.push(format!(
                        "refs/remotes/{nostr_remote_name}/{tracking_ref_name}:{nostr_ref_name}",
                    ));
                } else if *is_grasp_server || args.force {
                    refspecs.push(format!(
                        "+refs/remotes/{nostr_remote_name}/{tracking_ref_name}:{nostr_ref_name}",
                    ));
                } else {
                    not_updated.push(nostr_ref_name);
                }
            } else {
                // add missing refs
                refspecs.push(format!(
                    "refs/remotes/{nostr_remote_name}/{tracking_ref_name}:{nostr_ref_name}",
                ));
            }
        }

        // Skip grasp servers whose relay did not receive the state event —
        // they would reject the git push anyway.
        if (*is_grasp_server || is_grasp_server_clone_url(url))
            && !grasp_relay_publish_results.is_empty()
        {
            if let Ok(relay_url) = format_grasp_server_url_as_relay_url(url) {
                if grasp_relay_publish_results
                    .get(&relay_url)
                    .is_some_and(|succeeded| !succeeded)
                {
                    term.write_line(&format!(
                        "WARNING: skipping {remote_name} - state event failed to reach its relay"
                    ))?;
                    continue;
                }
            }
        }

        if refspecs.is_empty() {
            if !not_updated.is_empty() || !not_deleted.is_empty() {
                term.write_line(&format!("{remote_name} in sync excluding"))?;
            } else {
                term.write_line(&format!("{remote_name} already in sync"))?;
            }
            // report already in sync
        } else {
            match push_to_remote(
                &git_repo,
                url,
                &decoded_nostr_url,
                &refspecs,
                &term,
                *is_grasp_server || is_grasp_server_clone_url(url),
                &[],
            ) {
                Err(error) => {
                    term.write_line(&format!(
                        "error pushing updates to {remote_name}: error: {error}"
                    ))?;
                }
                Ok(updated_refs) => {
                    if updated_refs.values().all(std::option::Option::is_none) {
                        if *is_grasp_server || args.force {
                            term.write_line(&format!("{remote_name} sync completed"))?;
                            // TODO we only know if there was an error but not
                            // if it rejected any
                            // updates
                        } else {
                            // we should report on refs not force pushed
                            term.write_line(&format!("{remote_name} sync completed"))?;
                        }
                    } else {
                        term.write_line(&format!(
                            "{remote_name} sync completed but not all changes were accepted"
                        ))?;
                    }
                    for name in &not_deleted {
                        term.write_line(&format!("  - {name} not deleted"))?;
                    }
                    for name in &not_updated {
                        term.write_line(&format!("  - {name} not updated due to conflicts"))?;
                    }
                    if !not_updated.is_empty() || !not_deleted.is_empty() {
                        term.write_line("run `ngit sync --force` to delete refs or overwrite conflicts and potentially lose work")?;
                    }
                }
            }
        }
    }

    if !missing_refs.is_empty() {
        println!(
            "skipped the following refs as could not find them locally or on any git servers: {}",
            join_with_and(&missing_refs)
        );
    }
    Ok(())
}

fn invalid_nostr_state_ref(ref_name: &str) -> bool {
    ref_name.ends_with("^{}")
        || ref_name.starts_with("refs/heads/pr/")
        || (!ref_name.starts_with("refs/heads/") && !ref_name.starts_with("refs/tags/"))
}

fn identify_missing_refs(git_repo: &Repo, state: &HashMap<String, String>) -> Vec<String> {
    let mut missing_oids = vec![];
    for tip in state.values() {
        if let Ok(exist) = git_repo.does_commit_exist(tip) {
            let oid_exists_as_tag = Oid::from_str(tip).is_ok_and(|tip| {
                git_repo
                    .git_repo
                    .find_object(tip, Some(git2::ObjectType::Tag))
                    .is_ok()
            });

            if !exist && !oid_exists_as_tag {
                missing_oids.push(tip.to_string());
            }
        }
    }
    missing_oids
}

/// returns refs that are still missing
fn fetch_missing_refs(
    git_repo: &Repo,
    nostr_state: &RepoState,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
    nostr_url_decoded: &NostrUrlDecoded,
) -> Vec<String> {
    let mut tried_remotes: Vec<String> = vec![];
    let required_oids = identify_missing_refs(git_repo, &nostr_state.state);
    if !required_oids.is_empty() {
        println!("fetching git data missing locally");
    }
    loop {
        let required_oids = identify_missing_refs(git_repo, &nostr_state.state);
        let mut oids_on_remote: HashMap<String, Vec<String>> = HashMap::new();
        if !required_oids.is_empty() {
            for (url, (state, _)) in remote_states {
                if tried_remotes.contains(url) {
                    continue;
                }
                for oid in &required_oids {
                    if state.values().any(|v| v.eq(oid)) {
                        oids_on_remote
                            .entry(url.to_string())
                            .or_default()
                            .push(oid.clone());
                    }
                }
            }
        }
        if let Some((url, oids)) = oids_on_remote.iter().max_by_key(|(url, vec)| {
            if tried_remotes.contains(url) {
                0
            } else {
                vec.len()
            }
        }) {
            if oids.is_empty() || tried_remotes.contains(url) {
                break;
            }
            tried_remotes.push(url.clone());
            let _ = fetch_from_git_server(
                git_repo,
                oids,
                url,
                nostr_url_decoded,
                &Term::stdout(),
                is_grasp_server_clone_url(url),
            );
        } else {
            break;
        }
    }

    let still_missing_oids = identify_missing_refs(git_repo, &nostr_state.state);
    if still_missing_oids.is_empty() {
        vec![]
    } else {
        let missing_refs: Vec<String> = nostr_state
            .state
            .iter()
            .filter_map(|(key, value)| {
                if still_missing_oids.contains(value) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();
        println!(
            "could not find refs on repo git servers: {}",
            join_with_and(&missing_refs)
        );
        missing_refs
    }
}

#[cfg(test)]
mod tests {
    use super::invalid_nostr_state_ref;

    // Regression test: annotated-tag peeled refs (ending with ^{}) must be
    // treated as invalid nostr state refs so they are never used to build
    // git refspecs.  Before the fix, these passed through and caused git2 to
    // reject the push with "invalid refspec refs/remotes/origin/v1.4.4^{}:…".
    #[test]
    fn annotated_tag_peeled_ref_is_invalid() {
        assert!(
            invalid_nostr_state_ref("refs/tags/v1.4.4^{}"),
            "peeled annotated-tag ref must be invalid"
        );
        assert!(
            invalid_nostr_state_ref("refs/tags/v1.0.0^{}"),
            "peeled annotated-tag ref must be invalid"
        );
    }

    #[test]
    fn pr_ref_is_invalid() {
        assert!(
            invalid_nostr_state_ref("refs/heads/pr/42"),
            "PR branch refs must be invalid"
        );
    }

    #[test]
    fn arbitrary_non_heads_non_tags_ref_is_invalid() {
        assert!(
            invalid_nostr_state_ref("refs/notes/commits"),
            "refs outside heads/tags must be invalid"
        );
        assert!(invalid_nostr_state_ref("HEAD"), "HEAD must be invalid");
    }

    #[test]
    fn normal_branch_and_tag_refs_are_valid() {
        assert!(
            !invalid_nostr_state_ref("refs/heads/main"),
            "normal branch must be valid"
        );
        assert!(
            !invalid_nostr_state_ref("refs/heads/feature/foo"),
            "feature branch must be valid"
        );
        assert!(
            !invalid_nostr_state_ref("refs/tags/v1.4.4"),
            "lightweight tag must be valid"
        );
        assert!(
            !invalid_nostr_state_ref("refs/tags/v2.0.0-rc1"),
            "release-candidate tag must be valid"
        );
    }
}
