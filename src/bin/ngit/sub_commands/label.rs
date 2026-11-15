use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, send_events},
    git_events::{KIND_LABEL, get_labels},
    repo_lookup::RepositoryLookup,
};
use nostr::prelude::{Event, EventBuilder, Kind, PublicKey, Tag, nip10::Nip10Tag};

use crate::{
    cli::SignerParams,
    client::{Client, Connect, get_active_label_events, warn_if_invited_as_maintainer},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_resolved_repo_coordinate_for_publishing,
    sub_commands::{
        id_resolver::{load_and_resolve_issue, load_and_resolve_pr_root},
        repository_fetch::fetching_with_account,
    },
};

/// Add labels or remove the signer's own NIP-32 label assignments.
/// Resolve the target and validate the complete change before publishing.
#[allow(clippy::too_many_lines)]
async fn publish_label_event(
    id: &str,
    labels: &[String],
    remove_labels: &[String],
    offline: bool,
    target_kind: &str, // "issue" or "PR" — used in error messages
    auth: SignerParams<'_>,
) -> Result<()> {
    if labels.is_empty() && remove_labels.is_empty() {
        bail!("at least one --label or --remove-label value is required");
    }

    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let selected_repo = get_resolved_repo_coordinate_for_publishing(&git_repo, &mut client).await?;
    let mut lookup = RepositoryLookup::from_resolved(&git_repo, &selected_repo)?;
    let mut repo_coordinates = selected_repo.coordinate;

    if !offline {
        fetching_with_account(
            &git_repo,
            git_repo_path,
            &mut client,
            &mut repo_coordinates,
            &mut lookup,
            auth,
        )
        .await?;
    }

    let repo_ref = lookup.load(git_repo_path).await?;
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

    // Permission check: only the author or a confirmed member (maintainer or
    // moderator) may label.
    if target.pubkey != user_pubkey && !repo_ref.is_authorized_member(&user_pubkey) {
        bail!(
            "only the {target_kind} author or a repository member (maintainer or moderator) can label a {target_kind}"
        );
    }

    // Fetch existing label events so we can warn about duplicates.
    let existing_label_events = get_active_label_events(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .event(event_id)
                .kind(KIND_LABEL),
        ],
    )
    .await?;

    let existing_labels = get_labels(&target, &repo_ref, &existing_label_events);

    if !remove_labels.is_empty() {
        let plans = plan_removal(&existing_label_events, user_pubkey, remove_labels)?;
        let mut replacements = Vec::new();
        let mut deletion_tags = vec![Tag::parse(["k", "1985"])?];
        for RemovalPlan {
            original,
            surviving_tags,
        } in plans
        {
            deletion_tags.push(Tag::event(original.id));
            if let Some(tags) = surviving_tags {
                replacements.push(
                    ngit::client::sign_event(
                        EventBuilder::new(KIND_LABEL, original.content.clone())
                            .tags(tags)
                            .custom_created_at(original.created_at),
                        &signer,
                        format!("preserve labels on {target_kind}"),
                    )
                    .await?,
                );
            }
        }
        let deletion = ngit::client::sign_event(
            EventBuilder::new(Kind::EventDeletion, "").tags(deletion_tags),
            &signer,
            format!("remove labels from {target_kind}"),
        )
        .await?;
        let deletion_id = deletion.id;
        replacements.push(deletion);
        client.set_signer(signer).await;
        // send_events sends this ordered list separately to each relay and
        // stops that route at its first rejection. No relay receives the
        // deletion unless it has accepted every survivor first.
        send_events(
            &client,
            Some(git_repo_path),
            replacements,
            user_ref.relays.write(),
            repo_ref.relays.clone(),
            true,
            false,
        )
        .await?;
        let remaining = get_labels(
            &target,
            &repo_ref,
            &get_active_label_events(
                git_repo_path,
                vec![
                    nostr::prelude::Filter::new()
                        .kind(KIND_LABEL)
                        .event(event_id),
                ],
            )
            .await?,
        );
        if crate::output::is_json() {
            crate::output::set_value(serde_json::json!({
                "command_status": "ok", "action": "unlabelled",
                "entity": target_kind.to_lowercase(),
                "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
                "event": crate::output::event_id_to_nevent(deletion_id, repo_ref.relays.first()),
                "removed_from_own_events": remove_labels, "labels": remaining,
            }));
        }
        println!(
            "{target_kind} {}: removed your label assignments; remaining labels: {}",
            &event_id.to_hex()[..8],
            remaining.join(", ")
        );
        return Ok(());
    }

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
                "command_status": "ok",
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
            "command_status": "ok",
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
    remove_labels: &[String],
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_label_event(id, labels, remove_labels, offline, "issue", auth).await
}

pub async fn launch_pr_label(
    id: &str,
    labels: &[String],
    remove_labels: &[String],
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    publish_label_event(id, labels, remove_labels, offline, "PR", auth).await
}

/// Retain every unrelated tag and label namespace. Refuse multi-target events:
/// deleting one would also alter another entity's labels.
struct RemovalPlan<'a> {
    original: &'a Event,
    surviving_tags: Option<Vec<Tag>>,
}

fn plan_removal<'a>(
    events: &'a [Event],
    author: PublicKey,
    requested: &[String],
) -> Result<Vec<RemovalPlan<'a>>> {
    let values: Vec<&str> = requested.iter().map(|s| s.trim()).collect();
    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
        bail!("at least one non-empty --remove-label value is required");
    }
    let matches = |tag: &Tag| {
        let s = tag.as_slice();
        s.len() >= 3
            && s[0] == "l"
            && s[2] == "#t"
            && values.iter().any(|v| v.eq_ignore_ascii_case(&s[1]))
    };
    let mut found = Vec::new();
    let mut plans = Vec::new();
    for event in events {
        if event.kind != KIND_LABEL
            || event.pubkey != author
            || !event.tags.iter().any(|t| {
                t.as_slice().first().is_some_and(|s| s == "L")
                    && t.as_slice().get(1).is_some_and(|s| s == "#t")
            })
            || !event.tags.iter().any(&matches)
        {
            continue;
        }
        if event.tags.event_ids().count() != 1 {
            bail!("cannot remove labels from a label event referencing multiple targets");
        }
        for tag in event.tags.iter().filter(|t| matches(t)) {
            found.push(tag.as_slice()[1].as_str());
        }
        let tags: Vec<Tag> = event.tags.iter().filter(|t| !matches(t)).cloned().collect();
        let has_survivor = tags
            .iter()
            .any(|t| t.as_slice().first().is_some_and(|s| s == "l"));
        plans.push(RemovalPlan {
            original: event,
            surviving_tags: has_survivor.then_some(tags),
        });
    }
    for value in values {
        if !found.iter().any(|f| f.eq_ignore_ascii_case(value)) {
            bail!(
                "you have no removable label assignment for #{value}; labels on the original event or applied by other authors cannot be removed this way"
            );
        }
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventId, Keys, event::FinalizeEvent};

    use super::*;

    #[test]
    fn removal_preserves_unrelated_namespaces_and_ignores_other_authors() -> Result<()> {
        let keys = Keys::generate();
        let target = EventId::from_byte_array([0; 32]);
        let event = EventBuilder::new(KIND_LABEL, "metadata")
            .tags([
                Tag::event(target),
                Tag::parse(["L", "#t"])?,
                Tag::parse(["l", "bug", "#t"])?,
                Tag::parse(["l", "help-wanted", "#t"])?,
                Tag::parse(["L", "other"])?,
                Tag::parse(["l", "keep", "other"])?,
                Tag::parse(["custom", "preserved"])?,
            ])
            .finalize(&keys)?;
        let foreign = EventBuilder::new(KIND_LABEL, "")
            .tags(event.tags.clone())
            .finalize(&Keys::generate())?;
        let events = vec![event.clone(), foreign];
        let plans = plan_removal(&events, keys.public_key(), &[" BUG ".into()])?;
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].original.id, event.id);
        let tags = plans[0].surviving_tags.as_ref().unwrap();
        assert!(!tags.contains(&Tag::parse(["l", "bug", "#t"])?));
        for tag in [["l", "help-wanted", "#t"], ["l", "keep", "other"]] {
            assert!(tags.contains(&Tag::parse(tag)?));
        }
        assert!(tags.contains(&Tag::parse(["custom", "preserved"])?));
        assert!(plan_removal(&events, Keys::generate().public_key(), &["bug".into()]).is_err());
        assert!(plan_removal(&events, keys.public_key(), &["bug".into(), " ".into()]).is_err());
        assert!(
            plan_removal(
                &events,
                keys.public_key(),
                &["bug".into(), "missing".into()]
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn removal_refuses_to_change_other_targets() -> Result<()> {
        let keys = Keys::generate();
        let event = EventBuilder::new(KIND_LABEL, "")
            .tags([
                Tag::event(EventId::from_byte_array([0; 32])),
                Tag::event(EventId::from_byte_array([1; 32])),
                Tag::parse(["L", "#t"])?,
                Tag::parse(["l", "bug", "#t"])?,
            ])
            .finalize(&keys)?;
        assert!(plan_removal(&[event], keys.public_key(), &["bug".into()]).is_err());
        Ok(())
    }
}
