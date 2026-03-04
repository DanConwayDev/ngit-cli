use anyhow::{Context, Result, bail};
use ngit::{
    client::{
        Params, get_events_from_local_cache, get_issues_from_cache,
        get_proposals_and_revisions_from_cache, send_events, sign_event,
    },
    content_tags::{dedup_tags, tags_from_content},
    git_events::KIND_COMMENT,
};
use nostr::{EventBuilder, Tag, nips::nip19::Nip19};
use nostr_sdk::{EventId, FromBech32, Kind, PublicKey};

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

struct CommentArgs<'a> {
    root_event_id: EventId,
    root_pubkey: PublicKey,
    root_kind: Kind,
    /// When `None` the comment is top-level (parent == root).
    /// When `Some` the comment replies to that specific comment event.
    reply_to: Option<EventId>,
    git_repo_path: &'a std::path::Path,
    body: &'a str,
    entity_name: &'a str,
    client: Client,
    repo_ref: ngit::repo_ref::RepoRef,
}

/// Build and publish a NIP-22 kind-1111 comment on any event.
///
/// NIP-22 threading tags (<https://nips.nostr.com/22>):
///   - uppercase `E` — root event id + relay hint + root pubkey
///   - uppercase `K` — root event kind (as string)
///   - uppercase `P` — root event author pubkey
///   - lowercase `e` — parent event id + relay hint + parent pubkey
///   - lowercase `k` — parent event kind
///   - lowercase `p` — parent event author pubkey
async fn publish_comment(args: CommentArgs<'_>) -> Result<()> {
    let CommentArgs {
        root_event_id,
        root_pubkey,
        root_kind,
        reply_to,
        git_repo_path,
        body,
        entity_name,
        client,
        repo_ref,
    } = args;

    // Resolve parent: either the specified reply-to comment or the root itself
    let (parent_event_id, parent_pubkey, parent_kind) = if let Some(reply_id) = reply_to {
        // Look up the comment event from local cache
        let events = get_events_from_local_cache(
            git_repo_path,
            vec![nostr::Filter::default().id(reply_id).kind(KIND_COMMENT)],
        )
        .await?;
        let parent = events
            .into_iter()
            .find(|e| e.id == reply_id)
            .with_context(|| {
                format!(
                    "comment with id {} not found in cache; try without --offline",
                    reply_id.to_hex()
                )
            })?;
        (parent.id, parent.pubkey, KIND_COMMENT)
    } else {
        // Top-level comment: parent == root
        (root_event_id, root_pubkey, root_kind)
    };

    // Login
    let (signer, user_ref, _) =
        login::login_or_signup(&None, &None, &None, Some(&client), true).await?;

    let relay_hint = repo_ref
        .relays
        .first()
        .map(ToString::to_string)
        .unwrap_or_default();

    let root_kind_str = root_kind.as_u16().to_string();
    let parent_kind_str = parent_kind.as_u16().to_string();

    // NIP-22 compliant threading tags
    let mut comment_tags: Vec<Tag> = vec![
        // Root scope: uppercase E with root pubkey as 4th element
        Tag::parse(vec![
            "E".to_string(),
            root_event_id.to_hex(),
            relay_hint.clone(),
            root_pubkey.to_hex(),
        ])?,
        // Root kind
        Tag::parse(vec!["K".to_string(), root_kind_str])?,
        // Root author pubkey
        Tag::parse(vec![
            "P".to_string(),
            root_pubkey.to_hex(),
            relay_hint.clone(),
        ])?,
        // Parent item: lowercase e with parent pubkey as 4th element
        Tag::parse(vec![
            "e".to_string(),
            parent_event_id.to_hex(),
            relay_hint.clone(),
            parent_pubkey.to_hex(),
        ])?,
        // Parent kind
        Tag::parse(vec!["k".to_string(), parent_kind_str])?,
        // Parent author pubkey
        Tag::parse(vec!["p".to_string(), parent_pubkey.to_hex(), relay_hint])?,
    ];

    // NIP-21 mention tags: q tags for cited events/addresses, p tags for cited
    // pubkeys
    comment_tags.extend(tags_from_content(body, Some(git_repo_path)).await?);
    let comment_tags = dedup_tags(comment_tags);

    let comment_event = sign_event(
        EventBuilder::new(KIND_COMMENT, body).tags(comment_tags),
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
        &root_event_id.to_hex()[..8]
    );
    Ok(())
}

pub async fn launch_pr_comment(
    id: &str,
    body: &str,
    reply_to: Option<&str>,
    offline: bool,
) -> Result<()> {
    let root_event_id = parse_event_id(id)?;
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
        .find(|e| e.id == root_event_id)
        .context(format!(
            "PR with id {} not found in cache",
            root_event_id.to_hex()
        ))?;

    let root_kind = proposal.kind;
    let root_pubkey = proposal.pubkey;
    let reply_to_id = reply_to.map(parse_event_id).transpose()?;

    publish_comment(CommentArgs {
        root_event_id,
        root_pubkey,
        root_kind,
        reply_to: reply_to_id,
        git_repo_path,
        body,
        entity_name: "PR",
        client,
        repo_ref,
    })
    .await
}

pub async fn launch_issue_comment(
    id: &str,
    body: &str,
    reply_to: Option<&str>,
    offline: bool,
) -> Result<()> {
    let root_event_id = parse_event_id(id)?;
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    let issues = get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let issue = issues
        .iter()
        .find(|e| e.id == root_event_id)
        .context(format!(
            "issue with id {} not found in cache",
            root_event_id.to_hex()
        ))?;

    let root_pubkey = issue.pubkey;
    let reply_to_id = reply_to.map(parse_event_id).transpose()?;

    publish_comment(CommentArgs {
        root_event_id,
        root_pubkey,
        root_kind: Kind::GitIssue,
        reply_to: reply_to_id,
        git_repo_path,
        body,
        entity_name: "issue",
        client,
        repo_ref,
    })
    .await
}
