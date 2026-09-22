use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, delete_event_from_local_cache, get_active_label_events, send_events},
    git_events::{KIND_LABEL, get_labels},
    repo_lookup::RepositoryLookup,
};
use nostr::prelude::{EventBuilder, Kind, Tag};

use crate::{
    cli::SignerParams,
    client::{Client, Connect, warn_if_invited_as_maintainer},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_resolved_repo_coordinate_for_publishing,
    sub_commands::{
        id_resolver::{load_and_resolve_issue, load_and_resolve_pr_root},
        label::publish_label_event,
        repository_fetch::fetching_with_account,
    },
};

/// Shared implementation: remove labels from `target` via NIP-09 deletion.
///
/// NIP-09 deletion is whole-event granularity, so every kind-1985 event
/// authored by the caller that carries a requested value is deleted, and
/// the values those events carried minus the removed ones are republished
/// in a single replacement label event. Labels applied by other authors
/// are never touched: only the author of a label event can delete it.
/// Only the target author or a confirmed member (maintainer or moderator)
/// may unlabel.
#[allow(clippy::too_many_lines)]
pub(crate) async fn publish_unlabel_event(
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
    let selected = get_resolved_repo_coordinate_for_publishing(&git_repo, &mut client).await?;
    let mut lookup = RepositoryLookup::from_resolved(&git_repo, &selected)?;
    let mut repo_coordinates = selected.coordinate;

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

    // Permission check: only the author or a confirmed member (maintainer
    // or moderator) may unlabel.
    if target.pubkey != user_pubkey && !repo_ref.is_authorized_member(&user_pubkey) {
        bail!(
            "only the {target_kind} author or a repository member (maintainer or moderator) can remove labels from a {target_kind}"
        );
    }

    // Active label events only: re-removing an already-deleted label is new.
    let active_label_events = get_active_label_events(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .event(event_id)
                .kind(KIND_LABEL),
        ],
    )
    .await?;

    let existing_labels = get_labels(&target, &repo_ref, &active_label_events);

    // Match requested values case-insensitively against what is present.
    let mut to_remove: Vec<String> = vec![];
    for label in labels {
        let trimmed = label.trim();
        if trimmed.is_empty() {
            continue;
        }
        match existing_labels
            .iter()
            .find(|e| e.eq_ignore_ascii_case(trimmed))
        {
            Some(canonical) => {
                if !to_remove.iter().any(|e| e == canonical) {
                    to_remove.push(canonical.clone());
                }
            }
            None => bail!("{target_kind} does not have label #{trimmed}"),
        }
    }
    if to_remove.is_empty() {
        bail!("at least one --label value is required");
    }

    // NIP-09 only deletes whole events, and only the event author can
    // delete: collect the caller's own label events carrying a value.
    let mut doomed: Vec<nostr::prelude::Event> = vec![];
    for label_event in &active_label_events {
        if label_event.pubkey != user_pubkey {
            continue;
        }
        let carries = label_event.tags.iter().any(|t| {
            let s = t.as_slice();
            s.len() >= 3
                && s[0].eq("l")
                && s[2].eq("#t")
                && to_remove.iter().any(|r| r.eq_ignore_ascii_case(&s[1]))
        });
        if carries {
            doomed.push(label_event.clone());
        }
    }
    if doomed.is_empty() {
        bail!(
            "none of the labels to remove were applied by you; only the author of a label event can delete it"
        );
    }

    // Survivors: values the doomed events carried minus the removed ones,
    // republished as one replacement event.
    let mut survivors: Vec<String> = vec![];
    for label_event in &doomed {
        for tag in label_event.tags.iter() {
            let s = tag.as_slice();
            if s.len() >= 3 && s[0].eq("l") && s[2].eq("#t") && !s[1].is_empty() {
                let value = s[1].clone();
                if !to_remove.iter().any(|r| r.eq_ignore_ascii_case(&value))
                    && !survivors.iter().any(|e| e.eq_ignore_ascii_case(&value))
                {
                    survivors.push(value);
                }
            }
        }
    }

    // Build the kind-5 deletion event: one `e` tag per deleted label event
    // plus the `k` tag naming the deleted kind (NIP-09). Tags are set
    // before signing — the event id commits to them.
    let mut tags: Vec<Tag> = vec![Tag::parse(["k", "1985"])?];
    for label_event in &doomed {
        tags.push(Tag::parse(["e", &label_event.id.to_hex()])?);
    }
    let deletion = ngit::client::sign_event(
        EventBuilder::new(Kind::EventDeletion, "").tags(tags),
        &signer,
        format!("unlabel {target_kind}"),
    )
    .await?;

    let deletion_id = deletion.id;

    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![deletion],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    // Evict locally so subsequent reads converge even before relays serve
    // the deletion.
    for label_event in &doomed {
        delete_event_from_local_cache(git_repo_path, label_event.id).await?;
    }

    if !survivors.is_empty() {
        publish_label_event(&event_id.to_hex(), &survivors, offline, target_kind, auth).await?;
    }

    let removed_list = to_remove
        .iter()
        .map(|l| format!("#{l}"))
        .collect::<Vec<_>>()
        .join(", ");
    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "unlabelled",
            "entity": target_kind.to_lowercase(),
            "id": crate::output::event_id_to_nevent(event_id, repo_ref.relays.first()),
            "event": crate::output::event_id_to_nevent(
                deletion_id,
                repo_ref.relays.first(),
            ),
            "labels": to_remove,
        }));
    }
    println!(
        "{} {} unlabelled {}",
        target_kind,
        &event_id.to_hex()[..8],
        removed_list,
    );
    Ok(())
}
