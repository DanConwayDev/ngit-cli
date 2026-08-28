use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, get_proposals_and_revisions_from_cache, send_events},
    git_events::{get_status, sign_ordered_status_event, status_kinds},
};
use nostr::prelude::{
    EventBuilder, Kind, Tag, ToBech32,
    nip01::Nip01Tag,
    nip10::{Marker, Nip10Tag},
};

use crate::{
    cli::SignerParams,
    client::{Client, Connect, get_events_from_local_cache, get_repo_ref_from_cache},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_for_publishing,
    sub_commands::{
        id_resolver::{pr_description, proposal_roots, resolve_pr_root_or_prefix},
        repository_fetch::fetching_with_account,
    },
};

#[allow(clippy::too_many_lines)]
async fn launch_status(
    id: &str,
    offline: bool,
    new_kind: Kind,
    action: &str,
    reason: Option<&str>,
    auth: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let mut repo_coordinates = get_repo_coordinates_for_publishing(&git_repo, &mut client).await?;

    if !offline {
        fetching_with_account(
            &git_repo,
            git_repo_path,
            &mut client,
            &mut repo_coordinates,
            auth,
        )
        .await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal =
        resolve_pr_root_or_prefix(id, proposals_and_revisions.iter(), pr_description)?.clone();
    let event_id = proposal.id;

    // Login to get signer and user pubkey
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        auth.info,
        auth.password,
        Some(&client),
        true,
    )
    .await?;

    let user_pubkey = signer.get_public_key().await?;

    // Only the author or a confirmed member (maintainer or moderator) may
    // change status
    if proposal.pubkey != user_pubkey && !repo_ref.is_authorized_member(&user_pubkey) {
        bail!(
            "only the PR author or a repository member (maintainer or moderator) can change the status of a PR"
        );
    }

    // Fetch existing statuses to check current state
    let statuses = {
        let mut s = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
                nostr::prelude::Filter::default()
                    .custom_tags(
                        nostr::filter::SingleLetterTag::UPPERCASE_E,
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

    let proposals_vec: Vec<nostr::prelude::Event> =
        proposal_roots(&proposals_and_revisions).cloned().collect();

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
        if crate::output::is_json() {
            crate::output::set_value(serde_json::json!({
                "status": "ok",
                "action": "unchanged",
                "entity": "pr",
                "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
                "pr_status": status_str,
            }));
        }
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
    let mut public_keys: std::collections::HashSet<nostr::prelude::PublicKey> =
        repo_ref.maintainers.iter().copied().collect();
    public_keys.insert(proposal.pubkey);

    let content = reason.unwrap_or("").to_string();

    let alt_tag = Tag::parse(["alt", alt_text])?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let status_event = sign_ordered_status_event(
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
        &statuses,
        proposal.id,
        format!("PR {action}"),
    )
    .await?;
    let status_event_id = status_event.id;

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

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "action": action,
            "entity": "pr",
            "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
            "event": crate::output::event_id_to_nevent(
                status_event_id,
                repo_ref.relays.first(),
            ),
        }));
    }

    println!(
        "PR {} {action}: {}",
        &event_id.to_hex()[..8],
        proposal.pubkey.to_bech32().unwrap_or_default()
    );
    Ok(())
}

pub async fn launch_close(
    id: &str,
    offline: bool,
    reason: Option<&str>,
    auth: SignerParams<'_>,
) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusClosed, "closed", reason, auth).await
}

pub async fn launch_reopen(
    id: &str,
    offline: bool,
    reason: Option<&str>,
    auth: SignerParams<'_>,
) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusOpen, "reopened", reason, auth).await
}

pub async fn launch_ready(
    id: &str,
    offline: bool,
    reason: Option<&str>,
    auth: SignerParams<'_>,
) -> Result<()> {
    launch_status(
        id,
        offline,
        Kind::GitStatusOpen,
        "marked as ready",
        reason,
        auth,
    )
    .await
}

pub async fn launch_draft(
    id: &str,
    offline: bool,
    reason: Option<&str>,
    auth: SignerParams<'_>,
) -> Result<()> {
    launch_status(
        id,
        offline,
        Kind::GitStatusDraft,
        "converted to draft",
        reason,
        auth,
    )
    .await
}
