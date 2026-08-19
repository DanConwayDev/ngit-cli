use anyhow::{Context, Result, bail};
use ngit::{
    accept_maintainership::{
        build_maintainership_acceptance_with_defaults, finalize_maintainership_acceptance,
    },
    client::{Params, send_events},
    content_tags::{dedup_tags, tags_from_content},
    git_events::{KIND_COVER_NOTE, process_cover_note},
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

/// Shared implementation: publish a kind-1624 cover note event for `target`.
///
/// A cover note is a markdown body that replaces the displayed description of a
/// PR, patch or issue.  Only the author of the target event or a repository
/// maintainer may set it.  The latest authorised event wins (replaceable
/// semantics with hex-id tiebreak).
///
/// The `body` is processed for `nostr:` mentions (NIP-21), which are converted
/// to `q` (event) and `p` (pubkey) tags — the same rules as `--body` in issue
/// creation.
#[allow(clippy::too_many_lines)]
async fn publish_set_cover_note_event(
    id: &str,
    body: &str,
    offline: bool,
    target_kind: &str, // "issue" or "PR" — used in error messages
    auth: SignerParams<'_>,
) -> Result<()> {
    let body = body.trim();
    if body.is_empty() {
        bail!("--body value must not be empty");
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
    // moderator) may set a cover note.
    if target.pubkey != user_pubkey && !repo_ref.is_authorized_member(&user_pubkey) {
        bail!(
            "only the {target_kind} author or a repository member (maintainer or moderator) can set the cover note of a {target_kind}"
        );
    }

    // Fetch existing cover note events so we can check whether the body is
    // already set to the requested value.
    let existing_cover_note_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .event(event_id)
                .kind(KIND_COVER_NOTE),
        ],
    )
    .await?;

    if let Some((existing_cn, _)) =
        process_cover_note(&target, &repo_ref, &existing_cover_note_events)
    {
        if existing_cn.content.trim() == body {
            if crate::output::is_json() {
                crate::output::set_value(serde_json::json!({
                    "status": "ok",
                    "action": "unchanged",
                    "entity": target_kind.to_lowercase(),
                    "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
                    "body": body,
                }));
            }
            println!(
                "{target_kind} {} already has this cover note",
                &event_id.to_hex()[..8],
            );
            return Ok(());
        }
    }

    let maintainer_acceptance = if repo_ref
        .maintainers_without_annoucnement
        .as_ref()
        .is_some_and(|ms| ms.contains(&user_pubkey))
    {
        Some(
            build_maintainership_acceptance_with_defaults(&repo_ref, &user_ref, &client, &signer)
                .await
                .context("failed to auto-accept co-maintainership")?,
        )
    } else {
        None
    };

    // Build the kind-1624 cover note event.
    //
    // Shape:
    //   content: "<markdown>"
    //   tags:
    //     ["e", "<pr-issue-or-patch-id>", "<relay-hint>"]  — reference to target
    //     ["p", "<author-pubkey>"]                          — notify the author
    //     ["q", "<referenced-event>", ...]                  — from body mentions
    //     ["p", "<referenced-pubkey>", ...]                 — from body mentions
    //     ["alt", "cover note for <target_kind>"]
    let relay_hint = repo_ref.relays.first().cloned();

    let mut tags: Vec<Tag> = vec![];

    // Reference the target event (lowercase `e`).
    tags.push(Tag::from(Nip10Tag::Event {
        id: target.id,
        relay_hint: relay_hint.clone(),
        marker: None,
        public_key: None,
    }));

    // Notify the target event author.
    tags.push(Tag::public_key(target.pubkey));

    // Human-readable alt text.
    tags.push(Tag::parse([
        "alt",
        &format!("cover note for {target_kind}"),
    ])?);

    // Process body for nostr: mentions → q and p tags (same as --body in issue
    // creation).
    let mention_tags = tags_from_content(body, Some(git_repo_path)).await?;
    tags.extend(mention_tags);
    let tags = dedup_tags(tags);

    let cover_note_event = ngit::client::sign_event(
        EventBuilder::new(KIND_COVER_NOTE, body).tags(tags),
        &signer,
        format!("set {target_kind} cover note"),
    )
    .await?;
    let cover_note_event_id = cover_note_event.id;

    // Save to local cache immediately so subsequent reads reflect the new cover
    // note.
    save_event_in_local_cache(git_repo_path, &cover_note_event).await?;

    let mut client = client;
    client.set_signer(signer).await;

    let mut events = maintainer_acceptance
        .as_ref()
        .map(|acceptance| acceptance.event.clone())
        .into_iter()
        .collect::<Vec<_>>();
    events.push(cover_note_event);

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

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "action": "cover-note-set",
            "entity": target_kind.to_lowercase(),
            "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
            "event": crate::output::event_id_to_nevent(
                cover_note_event_id,
                repo_ref.relays.first(),
            ),
            "body": body,
        }));
    }

    println!("{} {} cover note set", target_kind, &event_id.to_hex()[..8]);
    Ok(())
}

pub async fn launch_issue_set_cover_note(
    id: &str,
    body: &str,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_set_cover_note_event(id, body, offline, "issue", auth).await
}

pub async fn launch_pr_set_cover_note(
    id: &str,
    body: &str,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_set_cover_note_event(id, body, offline, "PR", auth).await
}
