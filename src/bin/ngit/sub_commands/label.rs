use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, get_issues_from_cache, get_proposals_and_revisions_from_cache, send_events},
    git_events::{KIND_LABEL, get_labels},
};
use nostr::{EventBuilder, Tag, TagStandard, nips::nip19::Nip19};
use nostr_sdk::{EventId, FromBech32};

use crate::{
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache,
        get_repo_ref_from_cache, save_event_in_local_cache,
    },
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

fn parse_event_id(id: &str) -> Result<EventId> {
    if let Ok(nip19) = Nip19::from_bech32(id) {
        match nip19 {
            Nip19::Event(e) => return Ok(e.event_id),
            Nip19::EventId(event_id) => return Ok(event_id),
            _ => {}
        }
    }
    if let Ok(event_id) = EventId::from_hex(id) {
        return Ok(event_id);
    }
    bail!("invalid event-id or nevent: {id}")
}

/// Shared implementation: publish a NIP-32 kind-1985 label event for `target`.
///
/// `labels` must be non-empty. The caller is responsible for ensuring
/// `target` is a valid issue or PR event that belongs to the current repo.
#[allow(clippy::too_many_lines)]
async fn publish_label_event(
    id: &str,
    labels: &[String],
    offline: bool,
    target_kind: &str, // "issue" or "PR" — used in error messages
) -> Result<()> {
    if labels.is_empty() {
        bail!("at least one --label value is required");
    }

    let event_id = parse_event_id(id)?;

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    // Resolve the target event from cache.
    let target = if target_kind == "issue" {
        let issues = get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;
        issues
            .into_iter()
            .find(|e| e.id == event_id)
            .context(format!(
                "issue with id {} not found in cache",
                event_id.to_hex()
            ))?
    } else {
        let proposals =
            get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;
        proposals
            .into_iter()
            .find(|e| e.id == event_id)
            .context(format!(
                "PR with id {} not found in cache",
                event_id.to_hex()
            ))?
    };

    // Login — we need the signer and user pubkey.
    let (signer, user_ref, _) =
        login::login_or_signup(&Some(&git_repo), &None, &None, Some(&client), true).await?;

    let user_pubkey = signer.get_public_key().await?;

    // Permission check: only the author or a maintainer may label.
    if target.pubkey != user_pubkey && !repo_ref.maintainers.contains(&user_pubkey) {
        bail!("only the {target_kind} author or a repository maintainer can label a {target_kind}");
    }

    // Fetch existing label events so we can warn about duplicates.
    let existing_label_events = get_events_from_local_cache(
        git_repo_path,
        vec![nostr::Filter::default().event(event_id).kind(KIND_LABEL)],
    )
    .await?;

    let existing_labels = get_labels(&target, &repo_ref, &existing_label_events);

    // Deduplicate: only add labels not already present.
    let new_labels: Vec<String> = labels
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .filter(|l| !existing_labels.iter().any(|e| e.eq_ignore_ascii_case(l)))
        .collect();

    if new_labels.is_empty() {
        let already: Vec<String> = labels.iter().map(|l| format!("#{}", l.trim())).collect();
        println!(
            "{target_kind} already has label{}: {}",
            if already.len() == 1 { "" } else { "s" },
            already.join(", ")
        );
        return Ok(());
    }

    // Build the kind-1985 label event.
    //
    // Structure (NIP-32 §hashtag namespace):
    //   ["L", "#t"]                    — namespace declaration
    //   ["l", "<value>", "#t"]         — one tag per label
    //   ["e", <target-id>, <relay>]    — reference to the labelled event
    //   ["p", <author-pubkey>]         — notify the author
    let relay_hint = repo_ref.relays.first().cloned();

    let mut tags: Vec<Tag> = vec![
        // Namespace declaration
        Tag::custom(
            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("L")),
            vec!["#t".to_string()],
        ),
    ];

    // One ["l", value, "#t"] tag per label.
    for label in &new_labels {
        tags.push(Tag::custom(
            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("l")),
            vec![label.clone(), "#t".to_string()],
        ));
    }

    // Reference the target event.
    tags.push(Tag::from_standardized(TagStandard::Event {
        event_id: target.id,
        relay_url: relay_hint.clone(),
        marker: None,
        public_key: None,
        uppercase: false,
    }));

    // Notify the target event author.
    tags.push(Tag::public_key(target.pubkey));

    // Human-readable alt text.
    let label_list = new_labels
        .iter()
        .map(|l| format!("#{l}"))
        .collect::<Vec<_>>()
        .join(", ");
    tags.push(Tag::custom(
        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
        vec![format!("labelled {target_kind} with {label_list}")],
    ));

    let label_event = ngit::client::sign_event(
        EventBuilder::new(KIND_LABEL, "").tags(tags),
        &signer,
        format!("label {target_kind}"),
    )
    .await?;

    // Save to local cache immediately so subsequent reads reflect the new labels.
    save_event_in_local_cache(git_repo_path, &label_event).await?;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![label_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    println!(
        "{} {} labelled with {}",
        target_kind,
        &event_id.to_hex()[..8],
        label_list,
    );
    Ok(())
}

pub async fn launch_issue_label(id: &str, labels: &[String], offline: bool) -> Result<()> {
    publish_label_event(id, labels, offline, "issue").await
}

pub async fn launch_pr_label(id: &str, labels: &[String], offline: bool) -> Result<()> {
    publish_label_event(id, labels, offline, "PR").await
}
