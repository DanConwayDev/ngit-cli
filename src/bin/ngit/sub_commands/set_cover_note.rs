use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, get_issues_from_cache, get_proposals_and_revisions_from_cache, send_events},
    content_tags::{dedup_tags, tags_from_content},
    git_events::{KIND_COVER_NOTE, process_cover_note},
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
) -> Result<()> {
    let body = body.trim();
    if body.is_empty() {
        bail!("--body value must not be empty");
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

    // Permission check: only the author or a maintainer may set a cover note.
    if target.pubkey != user_pubkey && !repo_ref.maintainers.contains(&user_pubkey) {
        bail!(
            "only the {target_kind} author or a repository maintainer can set the cover note of a {target_kind}"
        );
    }

    // Fetch existing cover note events so we can check whether the body is
    // already set to the requested value.
    let existing_cover_note_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .event(event_id)
                .kind(KIND_COVER_NOTE),
        ],
    )
    .await?;

    if let Some((existing_cn, _)) =
        process_cover_note(&target, &repo_ref, &existing_cover_note_events)
    {
        if existing_cn.content.trim() == body {
            println!(
                "{target_kind} {} already has this cover note",
                &event_id.to_hex()[..8],
            );
            return Ok(());
        }
    }

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
    tags.push(Tag::custom(
        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
        vec![format!("cover note for {target_kind}")],
    ));

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

    // Save to local cache immediately so subsequent reads reflect the new cover
    // note.
    save_event_in_local_cache(git_repo_path, &cover_note_event).await?;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![cover_note_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    println!("{} {} cover note set", target_kind, &event_id.to_hex()[..8],);
    Ok(())
}

pub async fn launch_issue_set_cover_note(id: &str, body: &str, offline: bool) -> Result<()> {
    publish_set_cover_note_event(id, body, offline, "issue").await
}

pub async fn launch_pr_set_cover_note(id: &str, body: &str, offline: bool) -> Result<()> {
    publish_set_cover_note_event(id, body, offline, "PR").await
}
