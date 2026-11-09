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
    println!("CI coordinators · observed runs in the last 30 days");
    for row in &rows {
        print_row(row, now, roots.len() > 1)?;
    }
    if rows.is_empty() {
        println!("No recent runs, service requests or advertised availability observed.");
    }
    if !failures.is_empty() {
        println!("Some relay queries failed; observed activity may be incomplete.");
    }
    crate::output::set_value(
        serde_json::json!({"command_status":"ok", "entity":"ci", "action":"service-status", "services":rows, "relay_failures":failures, "offline":offline, "history_complete":false, "activity_since":now.as_secs().saturating_sub(ngit::ci::service::ACTIVITY_WINDOW_SECS), "observed_at":now.as_secs()}),
    );
    Ok(())
}

fn print_row(
    row: &ngit::ci::service::ServiceRow,
    now: Timestamp,
    multiple_perspectives: bool,
) -> Result<()> {
    let npub = row.coordinator.to_bech32()?;
    println!("\n{}", console::style(&npub).bold());
    for line in relationship_lines(row, now) {
        println!("  {line}");
    }
    if multiple_perspectives {
        println!(
            "  {}",
            console::style(format!("Repository perspective: {}", row.repository)).dim()
        );
    }
    if let Some(latest) = &row.activity.latest {
        let mut counts = vec![format!("{} runs observed", row.activity.runs_observed)];
        if row.activity.maintainer_directed_runs > 0 {
            counts.push(format!(
                "{} maintainer-authorized",
                row.activity.maintainer_directed_runs
            ));
        }
        for (count, label) in [
            (row.activity.running, "active"),
            (row.activity.concluded, "finished"),
            (row.activity.stale, "stale"),
        ] {
            if count > 0 {
                counts.push(format!("{count} {label}"));
            }
        }
        println!("  {}", counts.join(" · "));
        println!(
            "  Latest: {} {} · {}",
            crate::ci_display::outcome(&latest.status),
            latest.workflow,
            crate::ci_history::age(latest.reported_at, now.as_secs())
        );
        println!(
            "  {} {}",
            console::style("Tip:").dim(),
            console::style(format!("`ngit ci inspect {}`", latest.event_id)).cyan()
        );
    } else {
        println!("  No runs observed in the last 30 days");
    }
    Ok(())
}

fn maintainer_history_summary(
    history: &[ngit::ci::service::RequestHistoryEntry],
    now: Timestamp,
) -> Option<(String, bool)> {
    let history: Vec<_> = history.iter().filter(|h| h.maintainer).collect();
    let first = history.first()?;
    if history.len() == 1 && first.action == "start" {
        return Some((
            format!(
                "Maintainer requested {}",
                crate::ci_history::age(first.created_at, now.as_secs())
            ),
            false,
        ));
    }
    let mut changes: Vec<_> = history
        .iter()
        .take(3)
        .map(|h| {
            format!(
                "{} {}",
                h.action,
                crate::ci_history::age(h.created_at, now.as_secs())
            )
        })
        .collect();
    if history.len() > 3 {
        let remaining = history.len() - 3;
        changes.push(format!(
            "{remaining} earlier {}",
            if remaining == 1 { "change" } else { "changes" }
        ));
    }
    Some((
        format!("Maintainer requests: {}", changes.join(" · ")),
        true,
    ))
}

fn relationship_lines(row: &ngit::ci::service::ServiceRow, now: Timestamp) -> Vec<String> {
    let (mut relationship, timeline) = maintainer_history_summary(&row.request_history, now)
        .unwrap_or_else(|| {
            let relationship = match row.request {
                "maintainer_requested" => "Maintainer requested",
                "previously_requested" => "Previously maintainer-requested",
                "other_request_observed" => "Requested by a non-maintainer",
                _ if row.activity.maintainer_directed_runs > 0 => "Maintainer-authorized runs",
                _ => "No maintainer relationship verified",
            };
            let mut label = relationship.to_owned();
            if let Some(timestamp) = row.request_created_at {
                label.push(' ');
                label.push_str(&crate::ci_history::age(timestamp, now.as_secs()));
            }
            (label, false)
        });
    let mut claims = Vec::new();
    if row.service == "acting" {
        claims.push("Coordinator reports acting".to_owned());
    } else {
        if row.service != "no_status_observed" {
            claims.push(format!("Service claim: {}", row.service.replace('_', " ")));
        }
        if row.advertised_ready {
            claims.push("Advertises availability".to_owned());
        }
    }
    if timeline {
        let mut lines = vec![relationship];
        if !claims.is_empty() {
            lines.push(claims.join(" · "));
        }
        lines
    } else {
        for claim in claims {
            relationship.push_str(" · ");
            relationship.push_str(&claim.to_lowercase());
        }
        vec![relationship]
    }
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
    let now = Timestamp::now();
    let mut events = fetch(
        client,
        path,
        &reference.relays,
        vec![
            Filter::new()
                .kinds([9843, 9844, 39844, 19844].map(Kind::Custom))
                .custom_tags(SingleLetterTag::LOWERCASE_A, addresses.clone()),
            Filter::new()
                .kinds([
                    ngit::ci::kinds::KIND_CI_WORKFLOW_RESULT,
                    ngit::ci::kinds::KIND_CI_WORKFLOW_PROGRESS,
                ])
                .custom_tags(SingleLetterTag::LOWERCASE_A, addresses)
                .since(Timestamp::from_secs(
                    now.as_secs()
                        .saturating_sub(ngit::ci::service::ACTIVITY_WINDOW_SECS),
                )),
            Filter::new()
                .kind(Kind::Custom(19844))
                .custom_tags(SingleLetterTag::LOWERCASE_P, owners),
        ],
        offline,
        failures,
    )
    .await?;
    events.extend(
        fetch_run_requests(client, path, &reference.relays, &events, offline, failures).await?,
    );
    let mut authors: Vec<PublicKey> = events
        .iter()
        .filter(|e| e.verify().is_ok() && e.created_at <= now)
        .filter_map(|e| {
            if matches!(e.kind.as_u16(), 9843 | 9844) {
                ngit::ci::kinds::validate_service_control(e)
                    .ok()
                    .map(|c| c.coordinator)
            } else if matches!(e.kind.as_u16(), 19843 | 19844 | 39844 | 9842 | 39842) {
                Some(e.pubkey)
            } else {
                None
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

async fn fetch_run_requests(
    client: &Client,
    path: &std::path::Path,
    repository_relays: &[nostr::prelude::RelayUrl],
    events: &[Event],
    offline: bool,
    failures: &mut Vec<String>,
) -> Result<Vec<Event>> {
    let runs = ngit::ci::events::group_workflow_runs(events).runs;
    let wanted = ngit::ci::provenance::wanted_quotes(&runs);
    if !wanted.is_empty() {
        // One-shot triggers may predate the activity window. Fetch their exact
        // signed IDs so run authorization is not confused with standing service.
        let mut relays = repository_relays.to_vec();
        for hint in wanted.iter().filter_map(|w| w.relay.as_ref()) {
            if let Ok(relay) = nostr::prelude::RelayUrl::parse(hint) {
                if !relays.contains(&relay) {
                    relays.push(relay);
                }
            }
        }
        return fetch(
            client,
            path,
            &relays,
            vec![Filter::new().ids(wanted.iter().map(|w| w.event_id))],
            offline,
            failures,
        )
        .await;
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_start_stays_simple_but_control_history_is_bounded_and_newest_first() {
        let key = nostr::prelude::Keys::generate().public_key();
        let now = Timestamp::from_secs(100 * 86400);
        let entry = |action: &'static str, days: u64, maintainer: bool| {
            ngit::ci::service::RequestHistoryEntry {
                event_id: format!("{action}-{days}"),
                author: key,
                created_at: now.as_secs() - days * 86400,
                action,
                maintainer,
            }
        };
        let mut history = vec![entry("start", 60, true)];
        let (text, timeline) = maintainer_history_summary(&history, now).unwrap();
        assert!(!timeline);
        assert!(text.contains("Maintainer requested 2mo ago"));
        history = vec![
            entry("start", 2, true),
            entry("stop", 7, true),
            entry("start", 60, true),
            entry("stop", 70, true),
            entry("start", 90, true),
        ];
        let (text, timeline) = maintainer_history_summary(&history, now).unwrap();
        assert!(timeline);
        assert!(text.contains("start 2d ago · stop 1w ago · start 2mo ago"));
        assert!(text.contains("2 earlier changes"));
        assert!(!text.contains("3mo ago"));
        assert!(maintainer_history_summary(&[entry("start", 1, false)], now).is_none());
        let (text, timeline) = maintainer_history_summary(&[entry("stop", 1, true)], now).unwrap();
        assert!(timeline);
        assert!(text.contains("stop 1d ago"));
    }
}
