use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, send_events},
    git_events::{KIND_LABEL, get_labels_and_subject},
};
use nostr::prelude::{EventBuilder, Tag, nip10::Nip10Tag};

use crate::{
    cli::SignerParams,
    client::{
        Client, Connect, get_events_from_local_cache, get_repo_ref_from_cache,
        save_event_in_local_cache,
    },
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_for_publishing,
    sub_commands::{
        id_resolver::{load_and_resolve_issue, load_and_resolve_pr_root},
        repository_fetch::fetching_with_account,
    },
};

/// Shared implementation: publish a NIP-32 kind-1985 `#subject` label event
/// for `target`, overriding its displayed title/subject.
///
/// Only the author of the target event or a repository maintainer may set the
/// subject. The subject is not applied to the underlying git commit message —
/// it only affects how the PR/issue title is displayed.
#[allow(clippy::too_many_lines)]
async fn publish_set_subject_event(
    id: &str,
    subject: &str,
    offline: bool,
    target_kind: &str, // "issue" or "PR" — used in error messages
    auth: SignerParams<'_>,
) -> Result<()> {
    let subject = subject.trim();
    if subject.is_empty() {
        bail!("--subject value must not be empty");
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

    // Permission check: only the author or a confirmed member (maintainer or
    // moderator) may set the subject.
    if target.pubkey != user_pubkey && !repo_ref.is_authorized_member(&user_pubkey) {
        bail!(
            "only the {target_kind} author or a repository member (maintainer or moderator) can set the subject of a {target_kind}"
        );
    }

    // Fetch existing label events so we can check whether the subject is
    // already set to the requested value.
    let existing_label_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .event(event_id)
                .kind(KIND_LABEL),
        ],
    )
    .await?;

    let (_, existing_subject) = get_labels_and_subject(&target, &repo_ref, &existing_label_events);

    if existing_subject.as_deref() == Some(subject) {
        if crate::output::is_json() {
            crate::output::set_value(serde_json::json!({
                "command_status": "ok",
                "action": "unchanged",
                "entity": target_kind.to_lowercase(),
                "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
                "subject": subject,
            }));
        }
        println!(
            "{target_kind} {} already has subject: {}",
            &event_id.to_hex()[..8],
            subject,
        );
        return Ok(());
    }

    // Build the kind-1985 subject label event.
    //
    // Structure (NIP-32 §subject namespace):
    //   ["L", "#subject"]                  — namespace declaration
    //   ["l", "<new title>", "#subject"]   — the new subject value
    //   ["e", <target-id>, <relay>]        — reference to the labelled event
    //   ["p", <author-pubkey>]             — notify the author
    let relay_hint = repo_ref.relays.first().cloned();

    let mut tags: Vec<Tag> = vec![
        // Namespace declaration
        Tag::parse(["L", "#subject"])?,
        // Subject value
        Tag::parse(["l", subject, "#subject"])?,
    ];

    // Reference the target event.
    tags.push(Tag::from(Nip10Tag::Event {
        id: target.id,
        relay_hint: relay_hint.clone(),
        marker: None,
        public_key: None,
    }));

    // Notify the target event author.
    tags.push(Tag::public_key(target.pubkey));

    let subject_event = ngit::client::sign_event(
        EventBuilder::new(KIND_LABEL, "").tags(tags),
        &signer,
        format!("set {target_kind} subject"),
    )
    .await?;
    let subject_event_id = subject_event.id;

    // Save to local cache immediately so subsequent reads reflect the new subject.
    save_event_in_local_cache(git_repo_path, &subject_event).await?;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![subject_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "subject-set",
            "entity": target_kind.to_lowercase(),
            "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
            "event": crate::output::event_id_to_nevent(
                subject_event_id,
                repo_ref.relays.first(),
            ),
            "subject": subject,
        }));
    }

    println!(
        "{} {} subject set to: {}",
        target_kind,
        &event_id.to_hex()[..8],
        subject,
    );
    Ok(())
}

pub async fn launch_issue_set_subject(
    id: &str,
    subject: &str,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_set_subject_event(id, subject, offline, "issue", auth).await
}

pub async fn launch_pr_set_subject(
    id: &str,
    subject: &str,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_set_subject_event(id, subject, offline, "PR", auth).await
}
