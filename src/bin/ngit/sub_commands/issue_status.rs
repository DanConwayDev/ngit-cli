use anyhow::{Context, Result, bail};
use ngit::{
    accept_maintainership::{
        build_maintainership_acceptance_with_defaults, finalize_maintainership_acceptance,
    },
    client::{Params, get_issues_from_cache, send_events, sign_event},
    git_events::{get_status, status_kinds},
};
use nostr::{
    EventBuilder, Kind, Tag,
    nips::{
        nip01::Nip01Tag,
        nip10::{Marker, Nip10Tag},
    },
};

use crate::{
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_when_remote_unknown,
    sub_commands::id_resolver::{issue_description, resolve_issue_or_prefix},
};

#[allow(clippy::too_many_lines)]
async fn launch_status(
    id: &str,
    offline: bool,
    new_kind: Kind,
    action: &str,
    reason: Option<&str>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let issues = get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let issue = resolve_issue_or_prefix(id, issues.iter(), issue_description)?.clone();
    let event_id = issue.id;

    // Login to get signer and user pubkey
    let (signer, user_ref, _) =
        login::login_or_signup(&Some(&git_repo), &None, &None, Some(&client), true).await?;

    let user_pubkey = signer.get_public_key().await?;

    // Only author or maintainer may change status
    if issue.pubkey != user_pubkey && !repo_ref.maintainers.contains(&user_pubkey) {
        bail!("only the issue author or a repository maintainer can change the status of an issue");
    }

    // Fetch existing statuses to check current state
    let statuses = {
        let mut s = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(issues.iter().map(|e| e.id)),
                nostr::Filter::default()
                    .custom_tags(
                        nostr::filter::SingleLetterTag::uppercase(nostr::filter::Alphabet::E),
                        issues.iter().map(|e| e.id),
                    )
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        s.sort_by_key(|e| e.created_at);
        s.reverse();
        s
    };

    let empty_proposals: Vec<nostr::Event> = vec![];
    let current_status = get_status(&issue, &repo_ref, &statuses, &empty_proposals);

    if current_status == new_kind {
        let status_str = match new_kind {
            Kind::GitStatusOpen => "open",
            Kind::GitStatusClosed => "closed",
            Kind::GitStatusApplied => "resolved",
            _ => "unknown",
        };
        println!("issue is already {status_str}");
        return Ok(());
    }

    let maintainer_acceptance = if repo_ref
        .maintainers_without_annoucnement
        .as_ref()
        .is_some_and(|ms| ms.contains(&user_pubkey))
    {
        Some(
            build_maintainership_acceptance_with_defaults(
                &git_repo, &repo_ref, &user_ref, &client, &signer,
            )
            .await
            .context("failed to auto-accept co-maintainership")?,
        )
    } else {
        None
    };

    let alt_text = match new_kind {
        Kind::GitStatusOpen => "issue reopened",
        Kind::GitStatusClosed => "issue closed",
        Kind::GitStatusApplied => "issue resolved",
        _ => "issue status updated",
    };

    let mut public_keys: std::collections::HashSet<nostr::PublicKey> =
        repo_ref.maintainers.iter().copied().collect();
    public_keys.insert(issue.pubkey);

    let content = reason.unwrap_or("").to_string();

    let alt_tag = Tag::parse(["alt", alt_text])?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let status_event = sign_event(
        EventBuilder::new(new_kind, content).tags(
            [
                vec![
                    alt_tag,
                    Tag::from(Nip10Tag::Event {
                        id: issue.id,
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
        format!("issue {action}"),
    )
    .await?;

    let mut client = client;
    client.set_signer(signer).await;

    let mut events = maintainer_acceptance
        .as_ref()
        .map(|acceptance| acceptance.event.clone())
        .into_iter()
        .collect::<Vec<_>>();
    events.push(status_event);

    let mut relay_targets = repo_ref.relays.clone();
    if let Some(acceptance) = &maintainer_acceptance {
        for relay in &acceptance.relays {
            if !relay_targets.contains(relay) {
                relay_targets.push(relay.clone());
            }
        }
    }

    send_events(
        &client,
        Some(git_repo_path),
        events,
        user_ref.relays.write(),
        relay_targets,
        true,
        false,
    )
    .await?;

    if let Some(acceptance) = &maintainer_acceptance {
        finalize_maintainership_acceptance(&git_repo, acceptance).await?;
    }

    println!("issue {} {action}", &event_id.to_hex()[..8]);
    Ok(())
}

pub async fn launch_close(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusClosed, "closed", reason).await
}

pub async fn launch_reopen(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusOpen, "reopened", reason).await
}

pub async fn launch_resolved(id: &str, offline: bool, reason: Option<&str>) -> Result<()> {
    launch_status(id, offline, Kind::GitStatusApplied, "resolved", reason).await
}
