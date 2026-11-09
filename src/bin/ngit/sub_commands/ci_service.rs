//! Repository-scoped service discovery and evidence.
use anyhow::{Context, Result};
use ngit::{
    client::{Client, Connect, Params, fetch_filters_to_local_cache, get_events_from_local_cache},
    git::{Repo, RepoActions},
    repo_lookup::RepositoryLookup,
};
use nostr::prelude::{Event, Filter, Kind, PublicKey, SingleLetterTag, Timestamp, ToBech32};

use crate::cli::SignerParams;

async fn fetch(
    client: &Client,
    path: &std::path::Path,
    relays: &[nostr::prelude::RelayUrl],
    filters: Vec<Filter>,
    offline: bool,
    failures: &mut Vec<String>,
) -> Result<Vec<Event>> {
    if !offline {
        for (relay, result) in fetch_filters_to_local_cache(client, path, relays, &filters).await? {
            if let Err(error) = result {
                failures.push(format!("{relay}: {error:#}"));
            }
        }
    }
    get_events_from_local_cache(path, filters).await
}

pub async fn launch(offline: bool, auth: SignerParams<'_>) -> Result<()> {
    let repo = Repo::discover().context("failed to find repository")?;
    let path = repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&repo)));
    let selected =
        crate::repo_ref::get_resolved_repo_coordinate_when_remote_unknown(&repo, &mut client)
            .await?;
    let mut lookup = RepositoryLookup::from_resolved(&repo, &selected)?;
    let mut coordinates = selected.coordinate;
    if !offline {
        super::repository_fetch::fetching_with_account(
            &repo,
            path,
            &mut client,
            &mut coordinates,
            &mut lookup,
            auth,
        )
        .await?;
    }
    let reference = lookup.load(path).await?;
    let roots: Vec<_> = reference
        .coordinates()
        .into_iter()
        .map(|r| r.coordinate)
        .collect();
    let mut failures = Vec::new();
    let events =
        collect_evidence(&client, path, &reference, &roots, offline, &mut failures).await?;
    let now = Timestamp::now();
    let rows =
        ngit::ci::service::summarize(&events, &roots, &reference.confirmed_maintainers(), now);
    for row in &rows {
        println!(
            "{}  {}  {}{}\n  {}",
            row.coordinator.to_bech32()?,
            row.service.replace('_', " "),
            row.request.replace('_', " "),
            if row.advertised_ready {
                " · advertises readiness"
            } else {
                ""
            },
            row.repository
        );
    }
    if rows.is_empty() {
        println!("No coordinator service evidence found for this repository.");
    }
    println!(
        "Service is a signed coordinator claim; missing or expired evidence does not prove service stopped."
    );
    crate::output::set_value(
        serde_json::json!({"command_status":"ok", "entity":"ci", "action":"service-status", "services":rows, "relay_failures":failures, "offline":offline, "history_complete":false}),
    );
    Ok(())
}

async fn collect_evidence(
    client: &Client,
    path: &std::path::Path,
    reference: &ngit::repo_ref::RepoRef,
    roots: &[nostr::prelude::Coordinate],
    offline: bool,
    failures: &mut Vec<String>,
) -> Result<Vec<Event>> {
    let addresses: Vec<_> = roots.iter().map(ToString::to_string).collect();
    let owners: Vec<_> = roots.iter().map(|r| r.public_key.to_hex()).collect();
    let mut events = fetch(
        client,
        path,
        &reference.relays,
        vec![
            Filter::new()
                .kinds([9843, 9844, 39844, 19844].map(Kind::Custom))
                .custom_tags(SingleLetterTag::LOWERCASE_A, addresses),
            Filter::new()
                .kind(Kind::Custom(19844))
                .custom_tags(SingleLetterTag::LOWERCASE_P, owners),
        ],
        offline,
        failures,
    )
    .await?;
    let now = Timestamp::now();
    let mut authors: Vec<PublicKey> = events
        .iter()
        .filter(|e| e.verify().is_ok() && e.created_at <= now)
        .filter_map(|e| {
            if matches!(e.kind.as_u16(), 9843 | 9844) {
                ngit::ci::kinds::validate_service_control(e)
                    .ok()
                    .map(|c| c.coordinator)
            } else {
                Some(e.pubkey)
            }
        })
        .collect();
    authors.sort();
    authors.dedup();
    if !authors.is_empty() {
        // Refresh each discovered author's whole replaceable list: a newer list
        // can remove this repository and must supersede an old matching list.
        events.extend(
            fetch(
                client,
                path,
                &reference.relays,
                vec![
                    Filter::new()
                        .authors(authors.clone())
                        .kinds([19843, 19844].map(Kind::Custom)),
                ],
                offline,
                failures,
            )
            .await?,
        );
        let status_addresses: Vec<_> = authors
            .iter()
            .flat_map(|a| roots.iter().map(move |r| format!("39844:{a}:{r}")))
            .collect();
        let ids: Vec<_> = events.iter().map(|e| e.id.to_hex()).collect();
        let mut deleted_addresses = status_addresses;
        for author in &authors {
            deleted_addresses.extend([format!("19843:{author}:"), format!("19844:{author}:")]);
        }
        events.extend(
            fetch(
                client,
                path,
                &reference.relays,
                vec![
                    Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(authors.clone())
                        .custom_tags(SingleLetterTag::LOWERCASE_A, deleted_addresses),
                    Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(authors)
                        .custom_tags(SingleLetterTag::LOWERCASE_E, ids),
                ],
                offline,
                failures,
            )
            .await?,
        );
    }
    events.sort_by_key(|e| e.id);
    events.dedup_by_key(|e| e.id);
    Ok(events)
}
