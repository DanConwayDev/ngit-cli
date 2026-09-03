use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use ngit::{
    cli_interactor::cli_error,
    client::{
        Params, get_event_from_global_cache, get_events_from_local_cache, get_repo_ref_from_cache,
        get_repo_ref_from_cache_for_lead_recovery, get_state_from_cache, send_events,
    },
    event_ordering::latest_event,
    git::nostr_url::NostrUrlDecoded,
    repo_ref::{LeadSource, RepoRef, ResolvedRepoCoordinate},
};
use nostr::prelude::{Event, Filter, Kind, PublicKey, ToBech32};

use crate::{
    cli::SignerParams,
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::{print_selected_repo, try_resolve_repo_coordinate},
    sub_commands::repository_fetch::prepare_account_for_repo_fetch,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {}

fn announcement_by(repo_ref: &RepoRef, pubkey: PublicKey) -> Option<RepoRef> {
    repo_ref
        .events
        .values()
        .find(|event| event.pubkey == pubkey)
        .cloned()
        .and_then(|event| RepoRef::try_from((event, None)).ok())
}

async fn latest_own_announcement(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    pubkey: PublicKey,
) -> Option<Event> {
    if let Some(event) = repo_ref
        .events
        .values()
        .find(|event| event.pubkey == pubkey)
    {
        return Some(event.clone());
    }
    let filter = Filter::new()
        .kind(Kind::GitRepoAnnouncement)
        .author(pubkey)
        .identifier(repo_ref.identifier.clone());
    let mut candidates = get_event_from_global_cache(Some(git_repo_path), vec![filter.clone()])
        .await
        .unwrap_or_default();
    candidates.extend(
        get_events_from_local_cache(git_repo_path, vec![filter])
            .await
            .unwrap_or_default(),
    );
    latest_event(&candidates).cloned()
}

fn role_history_names(repo_ref: &RepoRef, pubkey: PublicKey) -> bool {
    let pubkey = pubkey.to_string();
    repo_ref.role_tags.iter().any(|tag| {
        let tag = tag.as_slice();
        matches!(tag.first().map(String::as_str), Some("M" | "m")) && tag.get(1) == Some(&pubkey)
    })
}

async fn require_equivalent_state(
    git_repo: &Repo,
    selected: &RepoRef,
    lead: &RepoRef,
) -> Result<()> {
    let selected_state = get_state_from_cache(Some(git_repo.get_path()?), selected).await;
    let lead_state = get_state_from_cache(Some(git_repo.get_path()?), lead).await;
    match (selected_state, lead_state) {
        (Ok(selected), Ok(lead)) if selected.state == lead.state => Ok(()),
        (Err(_), Err(_))
            if git_repo
                .get_git_config_item("nostr.nostate", None)?
                .as_deref()
                == Some("true") =>
        {
            Ok(())
        }
        (Ok(_), Ok(_)) => Err(cli_error(
            "the selected and lead coordinates publish different Git state",
            &[],
            &["reconcile their branches and tags before following the lead"],
        )),
        _ => Err(cli_error(
            "repository state is incomplete on one lead path",
            &[],
            &["fetch again after both coordinates publish equivalent state"],
        )),
    }
}

async fn matching_remote_urls(
    git_repo: &Repo,
    resolved: &ResolvedRepoCoordinate,
) -> Result<Vec<(String, String)>> {
    let mut configured = Vec::new();
    for name in git_repo
        .git_repo
        .remotes()?
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
    {
        let remote = git_repo.git_repo.find_remote(&name)?;
        if let Ok(url) = remote.url() {
            configured.push((name, url.to_string()));
        }
    }

    let selected_name = resolved.remote.as_ref().map(|remote| remote.name.as_str());
    let mut matching = Vec::new();
    for (name, url) in configured {
        let selected = selected_name == Some(name.as_str())
            || NostrUrlDecoded::parse_and_resolve(&url, &Some(git_repo))
                .await
                .is_ok_and(|decoded| {
                    decoded.coordinate.coordinate == resolved.coordinate.coordinate
                });
        if selected {
            matching.push((name, url));
        }
    }
    Ok(matching)
}

fn rollback_remote_urls(git_repo: &Repo, remotes: &[(String, String)]) {
    for (name, url) in remotes {
        let _ = git_repo.git_repo.remote_set_url(name, url);
    }
}

fn switch_local_coordinate(
    git_repo: &Repo,
    remotes: &[(String, String)],
    target_url: &str,
    target_coordinate: &str,
) -> Result<()> {
    let prior_config = git_repo.get_git_config_item("nostr.repo", None)?;
    let mut changed_remotes = Vec::new();
    for (name, old_url) in remotes {
        if let Err(error) = git_repo.git_repo.remote_set_url(name, target_url) {
            rollback_remote_urls(git_repo, &changed_remotes);
            return Err(error).context(format!("failed to update Git remote {name}"));
        }
        changed_remotes.push((name.clone(), old_url.clone()));
    }
    if let Err(error) = git_repo.save_git_config_item("nostr.repo", target_coordinate, false) {
        rollback_remote_urls(git_repo, &changed_remotes);
        if let Some(prior) = prior_config {
            let _ = git_repo.save_git_config_item("nostr.repo", &prior, false);
        } else {
            let _ = git_repo.remove_git_config_item("nostr.repo", false);
        }
        return Err(error).context("failed to update nostr.repo");
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn launch(_args: &SubCommandArgs, signer_params: SignerParams<'_>) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer_params.info,
        signer_params.password,
        Some(&client),
        false,
    )
    .await?;
    let resolved = try_resolve_repo_coordinate(&git_repo)
        .await?
        .context("no nostr repository found")?;
    print_selected_repo(&resolved);
    let mut selected_coordinate = resolved.coordinate.clone();
    let private_discovery = prepare_account_for_repo_fetch(
        &git_repo,
        &mut client,
        &selected_coordinate,
        &signer,
        &user_ref,
    )
    .await;
    ngit::client::fetching_with_private_discovery(
        git_repo_path,
        &client,
        &mut selected_coordinate,
        &private_discovery,
    )
    .await?;
    let selected_ref =
        get_repo_ref_from_cache_for_lead_recovery(Some(git_repo_path), &selected_coordinate)
            .await?;
    let resolution = selected_ref.lead_resolution();
    if resolution.source != LeadSource::Explicit {
        return Err(cli_error(
            "the selected repository has no complete explicit lead path",
            &[],
            &["resolve the pending, conflicting, or legacy lead decision first"],
        ));
    }
    let lead = resolution.lead.context("the resolved lead is missing")?;
    if !resolution.path.iter().all(|pubkey| {
        selected_ref
            .events
            .values()
            .any(|event| event.pubkey == *pubkey)
    }) {
        return Err(cli_error(
            "the explicit lead path was not completely discovered",
            &[],
            &["fetch again after every lead-path announcement is available"],
        ));
    }

    let canonical_lead = announcement_by(&selected_ref, lead)
        .context("the resolved lead announcement is missing")?;
    let mut lead_coordinate = canonical_lead.coordinate_with_hint();
    lead_coordinate.relays = canonical_lead.relays.clone();
    let lead_ref = get_repo_ref_from_cache(Some(git_repo_path), &lead_coordinate).await?;
    let lead_resolution = lead_ref.lead_resolution();
    if lead_resolution.source != LeadSource::Explicit || lead_resolution.lead != Some(lead) {
        return Err(cli_error(
            "the lead coordinate does not resolve to its own prepared roster",
            &[],
            &["ask the lead to repair their announcement before following it"],
        ));
    }
    let selected_members: HashSet<PublicKey> =
        selected_ref.confirmed_maintainers().into_iter().collect();
    let lead_members: HashSet<PublicKey> = lead_ref.confirmed_maintainers().into_iter().collect();
    if selected_members != lead_members {
        return Err(cli_error(
            "the selected and lead coordinates resolve different maintainer rosters",
            &[],
            &["reconcile the roster before following the lead"],
        ));
    }
    require_equivalent_state(&git_repo, &selected_ref, &lead_ref).await?;

    let my_pubkey = user_ref.public_key;
    let own_event = latest_own_announcement(git_repo_path, &selected_ref, my_pubkey).await;
    let is_confirmed = selected_ref.is_authorized_maintainer(&my_pubkey);
    let was_removed = !is_confirmed && role_history_names(&canonical_lead, my_pubkey);
    if lead == selected_coordinate.public_key
        && (my_pubkey == lead || (!is_confirmed && !was_removed))
    {
        return Err(cli_error(
            "this checkout already selects the resolved lead",
            &[],
            &[],
        ));
    }
    if is_confirmed || was_removed {
        let own_event = own_event.context("your maintainer announcement is missing")?;
        let mut own_ref = RepoRef::try_from((own_event, None))?;
        let old_lead = own_ref.lead.unwrap_or_else(|| {
            resolution
                .path
                .iter()
                .rev()
                .nth(1)
                .copied()
                .unwrap_or(selected_ref.selected_maintainer)
        });
        own_ref.role_tags = own_ref.role_history_for_follow_lead(
            &canonical_lead,
            my_pubkey,
            old_lead,
            lead,
            is_confirmed,
        )?;
        own_ref.maintainers = if is_confirmed {
            vec![my_pubkey, lead]
        } else {
            vec![lead]
        };
        own_ref.lead = Some(lead);
        for (coordinate, event) in &selected_ref.events {
            own_ref.events.insert(coordinate.clone(), event.clone());
        }
        let event = own_ref.to_event(&signer).await?;
        client.set_signer(signer.clone()).await;
        if selected_ref.private {
            client.nip42_register_private_repo_relays(selected_ref.relays.clone());
        }
        send_events(
            &client,
            Some(git_repo_path),
            vec![event],
            user_ref.relays.write(),
            lead_ref.relays.clone(),
            true,
            false,
        )
        .await
        .context("failed to publish the announcement following the lead")?;
    }

    let matching_remotes = matching_remote_urls(&git_repo, &resolved).await?;
    let target_url = canonical_lead
        .to_nostr_git_url(&Some(&git_repo))
        .to_string();
    let target_coordinate = lead_coordinate.to_bech32()?;
    switch_local_coordinate(
        &git_repo,
        &matching_remotes,
        &target_url,
        &target_coordinate,
    )?;

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "followed_lead",
            "lead": lead.to_string(),
            "coordinate": target_coordinate,
            "updated_remotes": matching_remotes.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        }));
    }
    println!("now following lead maintainer {lead}.");
    Ok(())
}
