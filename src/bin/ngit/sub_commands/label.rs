use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, send_events},
    git_events::{KIND_LABEL, get_labels},
};
use nostr::prelude::{EventBuilder, Tag, nip10::Nip10Tag};

use crate::{
    cli::SignerParams,
    client::{
        Client, Connect, get_events_from_local_cache, get_repo_ref_from_cache,
        save_event_in_local_cache, warn_if_invited_as_maintainer,
    },
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_for_publishing,
    sub_commands::{
        id_resolver::{load_and_resolve_issue, load_and_resolve_pr_root},
        repository_fetch::fetching_with_account,
    },
};

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
    auth: SignerParams<'_>,
) -> Result<()> {
    if labels.is_empty() {
        bail!("at least one --label value is required");
    }

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
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    // Resolve the target event from cache.
    let target = if target_kind == "issue" {
        load_and_resolve_issue(git_repo_path, &repo_ref, id).await?
    } else {
        load_and_resolve_pr_root(git_repo_path, &repo_ref, id).await?
    };
    let event_id = target.id;

    // Login — we need the signer and user pubkey.
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        auth.info,
        auth.password,
        Some(&client),
        true,
    )
    .await?;

    let user_pubkey = signer.get_public_key().await?;

    // Permission check: only the author or a maintainer may label.
    if target.pubkey != user_pubkey && !repo_ref.maintainers.contains(&user_pubkey) {
        bail!("only the {target_kind} author or a repository maintainer can label a {target_kind}");
    }

    // Fetch existing label events so we can warn about duplicates.
    let existing_label_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .event(event_id)
                .kind(KIND_LABEL),
        ],
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
        if crate::output::is_json() {
            crate::output::set_value(serde_json::json!({
                "status": "ok",
                "action": "unchanged",
                "entity": target_kind.to_lowercase(),
                "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
                "labels": existing_labels,
            }));
        }
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
        Tag::parse(["L", "#t"])?,
    ];

    // One ["l", value, "#t"] tag per label.
    for label in &new_labels {
        tags.push(Tag::parse(["l", label.as_str(), "#t"])?);
    }

    // Reference the target event.
    tags.push(Tag::from(Nip10Tag::Event {
        id: target.id,
        relay_hint: relay_hint.clone(),
        marker: None,
        public_key: None,
    }));

    // Notify the target event author.
    tags.push(Tag::public_key(target.pubkey));

    // Human-readable alt text.
    let label_list = new_labels
        .iter()
        .map(|l| format!("#{l}"))
        .collect::<Vec<_>>()
        .join(", ");
    tags.push(Tag::parse([
        "alt",
        &format!("labelled {target_kind} with {label_list}"),
    ])?);

    let label_event = ngit::client::sign_event(
        EventBuilder::new(KIND_LABEL, "").tags(tags),
        &signer,
        format!("label {target_kind}"),
    )
    .await?;
    let label_event_id = label_event.id;

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

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "action": "labelled",
            "entity": target_kind.to_lowercase(),
            "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
            "event": crate::output::event_id_to_nevent(
                label_event_id,
                repo_ref.relays.first(),
            ),
            "labels": new_labels,
        }));
    }

    println!(
        "{} {} labelled with {}",
        target_kind,
        &event_id.to_hex()[..8],
        label_list,
    );
    Ok(())
}

pub async fn launch_issue_label(
    id: &str,
    labels: &[String],
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_label_event(id, labels, offline, "issue", auth).await
}

pub async fn launch_pr_label(
    id: &str,
    labels: &[String],
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_label_event(id, labels, offline, "PR", auth).await
}
