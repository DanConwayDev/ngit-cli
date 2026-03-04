use anyhow::{Context, Result, bail};
use ngit::{
    client::{
        Params, get_issues_from_cache, get_proposals_and_revisions_from_cache, send_events,
        sign_event,
    },
    git_events::KIND_COMMENT,
};
use nostr::{EventBuilder, Tag, nips::nip19::Nip19};
use nostr_sdk::{EventId, FromBech32, Kind};

use crate::{
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

fn parse_event_id(id: &str) -> Result<EventId> {
    if let Ok(nip19) = Nip19::from_bech32(id) {
        match nip19 {
            nostr::nips::nip19::Nip19::Event(e) => return Ok(e.event_id),
            nostr::nips::nip19::Nip19::EventId(event_id) => return Ok(event_id),
            _ => {}
        }
    }
    if let Ok(event_id) = EventId::from_hex(id) {
        return Ok(event_id);
    }
    bail!("invalid event-id or nevent: {id}")
}

/// Build and publish a NIP-22 kind-1111 comment on any event.
///
/// NIP-22 threading tags:
///   - uppercase `E` — root event id
///   - uppercase `K` — root event kind (as string)
///   - lowercase `e` — parent event id (same as root for top-level comments)
///   - lowercase `k` — parent event kind
async fn publish_comment(
    id: &str,
    body: &str,
    offline: bool,
    root_kind: Kind,
    entity_name: &str,
) -> Result<()> {
    let event_id = parse_event_id(id)?;

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    // Login
    let (signer, user_ref, _) =
        login::login_or_signup(&Some(&git_repo), &None, &None, Some(&client), true).await?;

    let root_kind_str = root_kind.as_u16().to_string();

    // NIP-22: uppercase E = root event, uppercase K = root kind,
    //         lowercase e = parent event (same as root for top-level),
    //         lowercase k = parent kind
    let comment_event = sign_event(
        EventBuilder::new(KIND_COMMENT, body).tags(vec![
            // Root event (uppercase E)
            Tag::parse(vec![
                "E".to_string(),
                event_id.to_hex(),
                repo_ref
                    .relays
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                String::new(), // root marker
            ])?,
            // Root kind (uppercase K)
            Tag::parse(vec!["K".to_string(), root_kind_str.clone()])?,
            // Parent event (lowercase e, same as root for top-level comment)
            Tag::parse(vec![
                "e".to_string(),
                event_id.to_hex(),
                repo_ref
                    .relays
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                "reply".to_string(),
            ])?,
            // Parent kind (lowercase k)
            Tag::parse(vec!["k".to_string(), root_kind_str])?,
        ]),
        &signer,
        format!("comment on {entity_name}"),
    )
    .await?;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![comment_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    println!(
        "comment posted on {entity_name} {}",
        &event_id.to_hex()[..8]
    );
    Ok(())
}

pub async fn launch_pr_comment(id: &str, body: &str, offline: bool) -> Result<()> {
    // Verify the PR exists in cache
    let event_id = parse_event_id(id)?;
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    let proposals =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal = proposals
        .iter()
        .find(|e| e.id == event_id)
        .context(format!(
            "PR with id {} not found in cache",
            event_id.to_hex()
        ))?;

    let root_kind = proposal.kind;

    publish_comment(id, body, true /* already fetched */, root_kind, "PR").await
}

pub async fn launch_issue_comment(id: &str, body: &str, offline: bool) -> Result<()> {
    // Verify the issue exists in cache
    let event_id = parse_event_id(id)?;
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    let issues = get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    issues.iter().find(|e| e.id == event_id).context(format!(
        "issue with id {} not found in cache",
        event_id.to_hex()
    ))?;

    publish_comment(
        id,
        body,
        true, /* already fetched */
        Kind::GitIssue,
        "issue",
    )
    .await
}
