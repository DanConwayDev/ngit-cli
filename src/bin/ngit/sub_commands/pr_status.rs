use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, get_proposals_and_revisions_from_cache, send_events, sign_event},
    git_events::{get_status, status_kinds},
};
use nostr::{
    EventBuilder, EventId, FromBech32, Kind, Tag, ToBech32,
    nips::{
        nip01::Nip01Tag,
        nip10::{Marker, Nip10Tag},
        nip19::Nip19,
    },
};

use crate::{
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache,
        get_repo_ref_from_cache, warn_if_invited_as_maintainer,
    },
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

#[allow(clippy::too_many_lines)]
async fn launch_status(
    id: &str,
    offline: bool,
    new_kind: Kind,
    action: &str,
    reason: Option<&str>,
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
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    let proposals_and_revisions =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal = proposals_and_revisions
        .iter()
        .find(|e| e.id == event_id)
        .context(format!(
            "PR with id {} not found in cache",
            event_id.to_hex()
        ))?
        .clone();

    // Login to get signer and user pubkey
    let (signer, user_ref, _) =
        login::login_or_signup(&Some(&git_repo), &None, &None, Some(&client), true).await?;

    let user_pubkey = signer.get_public_key().await?;

    // Only author or maintainer may change status
    if proposal.pubkey != user_pubkey && !repo_ref.maintainers.contains(&user_pubkey) {
        bail!("only the PR author or a repository maintainer can change the status of a PR");
    }

    // Fetch existing statuses to check current state
    let statuses = {
        let mut s = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
                nostr::Filter::default()
                    .custom_tags(
                        nostr::filter::SingleLetterTag::uppercase(nostr::filter::Alphabet::E),
                        proposals_and_revisions.iter().map(|e| e.id),
                    )
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        s.sort_by_key(|e| e.created_at);
        s.reverse();
        s
    };

    let proposals_vec: Vec<nostr::Event> = proposals_and_revisions
        .iter()
        .filter(|e| !ngit::git_events::event_is_revision_root(e))
        .cloned()
        .collect();

    let current_status = get_status(&proposal, &repo_ref, &statuses, &proposals_vec);

    // Guard against no-op transitions
    if current_status == new_kind {
        let status_str = match new_kind {
            Kind::GitStatusOpen => "open",
            Kind::GitStatusClosed => "closed",
            Kind::GitStatusDraft => "draft",
            Kind::GitStatusApplied => "applied",
            _ => "unknown",
        };
        println!("PR is already {status_str}");
        return Ok(());
    }

    let alt_text = match new_kind {
        Kind::GitStatusOpen => "PR reopened",
        Kind::GitStatusClosed => "PR closed",
        Kind::GitStatusDraft => "PR marked as draft",
        Kind::GitStatusApplied => "PR applied/merged",
        _ => "PR status updated",
    };

    // Build status event following the same pattern as push.rs
    let mut public_keys: std::collections::HashSet<nostr::PublicKey> =
        repo_ref.maintainers.iter().copied().collect();
    public_keys.insert(proposal.pubkey);

    let content = reason.unwrap_or("").to_string();

    let alt_tag = Tag::parse(["alt", alt_text])?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let status_event = sign_event(
        EventBuilder::new(new_kind, content).tags(
            [
                vec![
                    alt_tag,
                    Tag::from(Nip10Tag::Event {
                        id: proposal.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        marker: Some(Marker::Root),
                        public_key: None,
                    }),
                ],
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        Tag::from(Nip01Tag::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_hint: c.relays.first().cloned(),
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![r_tag],
            ]
            .concat(),
        ),
        &signer,
        format!("PR {action}"),
    )
    .await?;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![status_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    println!(
        "PR {} {action}: {}",
        &event_id.to_hex()[..8],
        proposal.pubkey.to_bech32().unwrap_or_default()
    );
    Ok(())
}

pub async fn launch_close(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusClosed, "closed", reason).await
}

pub async fn launch_reopen(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusOpen, "reopened", reason).await
}

pub async fn launch_ready(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusOpen, "marked as ready", reason).await
}

pub async fn launch_draft(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(
        id,
        offline,
        Kind::GitStatusDraft,
        "converted to draft",
        reason,
    )
    .await
}
