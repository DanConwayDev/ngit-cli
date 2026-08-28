use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use ngit::{
    cli_interactor::cli_error,
    client::{
        Client, Connect, STATE_KIND, get_event_from_global_cache, get_events_from_local_cache,
    },
    event_ordering::latest_event,
    repo_ref::RepoRef,
    repo_state::RepoState,
};
use nostr::prelude::{Event, Filter, Kind, PublicKey, ToBech32};

pub async fn latest_announcement(
    git_repo_path: &Path,
    identifier: &str,
    pubkey: PublicKey,
    discovered: &[Event],
) -> Option<Event> {
    let filter = Filter::new()
        .kind(Kind::GitRepoAnnouncement)
        .author(pubkey)
        .identifier(identifier.to_string());
    let mut candidates = get_event_from_global_cache(Some(git_repo_path), vec![filter.clone()])
        .await
        .unwrap_or_default();
    candidates.extend(
        get_events_from_local_cache(git_repo_path, vec![filter])
            .await
            .unwrap_or_default(),
    );
    candidates.extend(
        discovered
            .iter()
            .filter(|event| event.kind == Kind::GitRepoAnnouncement && event.pubkey == pubkey)
            .cloned(),
    );
    latest_event(&candidates).cloned()
}

pub async fn discover_candidate_events(
    client: &Client,
    repo_ref: &RepoRef,
    pubkey: PublicKey,
) -> Result<Vec<Event>> {
    let mut relays: Vec<String> = repo_ref.relays.iter().map(ToString::to_string).collect();
    if !repo_ref.private {
        relays.extend(client.get_relay_default_set().iter().cloned());
        relays.extend(client.get_announcement_indexer_relays().iter().cloned());
        relays.extend(client.get_fallback_signer_relays().iter().cloned());
    }
    relays.sort();
    relays.dedup();
    if relays.is_empty() {
        return Ok(Vec::new());
    }
    client
        .get_events(
            relays,
            vec![
                Filter::new()
                    .kind(Kind::GitRepoAnnouncement)
                    .author(pubkey)
                    .identifier(repo_ref.identifier.clone()),
                Filter::new()
                    .kind(STATE_KIND)
                    .author(pubkey)
                    .identifier(repo_ref.identifier.clone()),
            ],
        )
        .await
        .context("failed to check the candidate's same-identifier repository")
}

async fn state_events(git_repo_path: &Path, identifier: &str, discovered: &[Event]) -> Vec<Event> {
    let filter = Filter::new()
        .kind(STATE_KIND)
        .identifier(identifier.to_string());
    let mut events = get_event_from_global_cache(Some(git_repo_path), vec![filter.clone()])
        .await
        .unwrap_or_default();
    events.extend(
        get_events_from_local_cache(git_repo_path, vec![filter])
            .await
            .unwrap_or_default(),
    );
    events.extend(
        discovered
            .iter()
            .filter(|event| event.kind == STATE_KIND)
            .cloned(),
    );
    events.sort_by_key(|event| event.id);
    events.dedup_by_key(|event| event.id);
    events
}

async fn state_for_authors(
    git_repo_path: &Path,
    identifier: &str,
    authors: &HashSet<PublicKey>,
    discovered: &[Event],
) -> Option<RepoState> {
    RepoState::try_from(
        state_events(git_repo_path, identifier, discovered)
            .await
            .into_iter()
            .filter(|event| authors.contains(&event.pubkey))
            .collect(),
    )
    .ok()
}

#[derive(Debug, Default, Eq, PartialEq)]
struct StateDifference {
    add: Vec<String>,
    remove: Vec<String>,
    update: Vec<String>,
}

fn state_difference(
    controlled: &std::collections::HashMap<String, String>,
    required: &std::collections::HashMap<String, String>,
) -> StateDifference {
    let mut difference = StateDifference::default();
    for (reference, value) in required {
        match controlled.get(reference) {
            None => difference.add.push(reference.clone()),
            Some(current) if current != value => difference.update.push(reference.clone()),
            Some(_) => {}
        }
    }
    for reference in controlled.keys() {
        if !required.contains_key(reference) {
            difference.remove.push(reference.clone());
        }
    }
    difference.add.sort();
    difference.remove.sort();
    difference.update.sort();
    difference
}

fn format_refs(refs: &[String]) -> String {
    if refs.is_empty() {
        "none".to_string()
    } else {
        refs.join(", ")
    }
}

/// Refuse a relationship that would immediately authorize another state view.
///
/// `controller_is_incoming` is true for acceptance, where the caller controls
/// the state being activated. It is false for an invitation that is already
/// reciprocal, where the caller controls the current repository state.
pub async fn require_equivalent_activating_state(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    incoming_author: PublicKey,
    controller_is_incoming: bool,
    force_requested: bool,
    discovered: &[Event],
) -> Result<()> {
    let incoming_authors = HashSet::from([incoming_author]);
    let Some(incoming) = state_for_authors(
        git_repo_path,
        &repo_ref.identifier,
        &incoming_authors,
        discovered,
    )
    .await
    else {
        return Ok(());
    };

    let existing_authors: HashSet<PublicKey> = repo_ref
        .confirmed_maintainers()
        .into_iter()
        .filter(|author| *author != incoming_author)
        .collect();
    let existing = state_for_authors(
        git_repo_path,
        &repo_ref.identifier,
        &existing_authors,
        discovered,
    )
    .await;
    let (controlled, required) = if controller_is_incoming {
        (&incoming.state, existing.as_ref().map(|state| &state.state))
    } else {
        let Some(existing) = existing.as_ref() else {
            return Err(cli_error(
                "this invitation would immediately activate pre-existing repository state",
                &[(
                    "incoming refs",
                    &format_refs(&incoming.state.keys().cloned().collect::<Vec<_>>()),
                )],
                &["publish an authoritative repository state before retrying"],
            ));
        };
        (&existing.state, Some(&incoming.state))
    };
    let Some(required) = required else {
        return Err(cli_error(
            "accepting would activate pre-existing state where the invited repository has no authoritative state",
            &[(
                "remove",
                &format_refs(&controlled.keys().cloned().collect::<Vec<_>>()),
            )],
            &["remove or move that state to another identifier before accepting"],
        ));
    };
    let difference = state_difference(controlled, required);
    if difference == StateDifference::default() {
        return Ok(());
    }

    let force_note = if force_requested {
        "--force cannot override state collisions in this release; reconcile the refs first"
    } else {
        "reconcile these refs before retrying; --force is reserved but does not override this collision yet"
    };
    Err(cli_error(
        "this membership change would immediately activate divergent repository state",
        &[
            ("add", &format_refs(&difference.add)),
            ("remove", &format_refs(&difference.remove)),
            ("update", &format_refs(&difference.update)),
        ],
        &[force_note],
    ))
}

pub fn require_no_joined_component(
    candidate_ref: &RepoRef,
    current_roster: &[PublicKey],
    publisher: PublicKey,
    candidate: PublicKey,
) -> Result<()> {
    let current: HashSet<PublicKey> = current_roster.iter().copied().collect();
    let mut allowed = current;
    allowed.insert(publisher);
    allowed.insert(candidate);
    let mut extra: Vec<String> = candidate_ref
        .maintainers
        .iter()
        .filter(|pubkey| !allowed.contains(pubkey))
        .map(|pubkey| pubkey.to_bech32().unwrap_or_else(|_| pubkey.to_hex()))
        .collect();
    extra.sort();
    extra.dedup();
    if extra.is_empty() {
        return Ok(());
    }
    Err(cli_error(
        "this membership change would join another same-identifier repository component",
        &[("additional maintainers", &extra.join(", "))],
        &["reconcile the repositories under separate identifiers before retrying"],
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn state_difference_names_each_required_ref_action() {
        let controlled = HashMap::from([
            ("refs/heads/main".to_string(), "old".to_string()),
            ("refs/heads/only-here".to_string(), "one".to_string()),
        ]);
        let required = HashMap::from([
            ("refs/heads/main".to_string(), "new".to_string()),
            ("refs/tags/v1".to_string(), "tag".to_string()),
        ]);

        assert_eq!(
            state_difference(&controlled, &required),
            StateDifference {
                add: vec!["refs/tags/v1".to_string()],
                remove: vec!["refs/heads/only-here".to_string()],
                update: vec!["refs/heads/main".to_string()],
            }
        );
    }
}
