//! Open-ended observation of distinct workflow/job receipts. No scheduler or
//! synthetic lifecycle events: the signed event remains the unit of output.
use std::collections::HashSet;

use anyhow::Result;
use ngit::{
    ci::watch::{Subscription, Update},
    client::{Client, get_events_from_local_cache, save_event_in_local_cache},
    git::{Repo, RepoActions},
    repo_ref::RepoRef,
};
use nostr::prelude::{Event, EventId, Kind, MatchEventOptions};

use crate::ci_projection::{ProjectionRequest, Target, Tier, build_report};

pub async fn launch(
    repo: &Repo,
    reference: &RepoRef,
    client: &Client,
    target: &Target,
    json: bool,
    history_scope: &crate::ci_history::Scope,
) -> Result<()> {
    if json {
        crate::output::start_stream();
    }
    let filter = target
        .ci_filter()
        .kinds([9841, 9842, 39842, 39846].map(Kind::Custom));
    let mut subscription = client.watch_ci(&reference.relays, filter).await?;
    // Keep one interrupt listener alive, including during cache and output I/O.
    let result = tokio::select! {
        signal = tokio::signal::ctrl_c() => signal.map_err(anyhow::Error::from),
        result = observe(repo, reference, client, target, json, history_scope, &mut subscription) => result,
    };
    subscription.close().await;
    result
}

async fn observe(
    repo: &Repo,
    reference: &RepoRef,
    client: &Client,
    target: &Target,
    json: bool,
    history_scope: &crate::ci_history::Scope,
    subscription: &mut Subscription,
) -> Result<()> {
    let path = repo.get_path()?;
    let filter = target
        .ci_filter()
        .kinds([9841, 9842, 39842, 39846].map(Kind::Custom));
    let mut seen = HashSet::new();
    let mut initial: Vec<_> = get_events_from_local_cache(path, vec![filter.clone()])
        .await?
        .into_iter()
        .collect();
    initial.sort_by_key(|event| (event.created_at, event.id));
    if !json {
        let report = build_report(
            repo,
            path,
            reference,
            client,
            &ProjectionRequest {
                target,
                tier: Tier::Cache,
                include_outdated: false,
                input_coverage: ngit::ci::trust::Coverage::Partial,
            },
        )
        .await?;
        crate::ci_display::print_status(&report, target);
        crate::ci_history::load(repo, history_scope, path, reference, target)
            .await?
            .print();
    }
    for event in initial {
        if matches_target(target, reference, &event)
            && event.verify().is_ok()
            && seen.insert(event.id)
            && json
        {
            emit(&event, true).await?;
        }
    }
    eprintln!("Watching CI for {target:?}; Ctrl-C stops observation without cancelling execution.");
    loop {
        match subscription.next().await? {
            Update::Notice(message) => eprintln!("CI watch: {message}"),
            Update::Event { relay: _, event } => {
                if filter.match_event(&event, MatchEventOptions::default())
                    && matches_target(target, reference, &event)
                    && event.verify().is_ok()
                    && seen.insert(event.id)
                {
                    emit(&event, json).await?;
                    save_event_in_local_cache(path, &event).await?;
                }
            }
        }
    }
}

fn matches_target(target: &Target, reference: &RepoRef, event: &Event) -> bool {
    let repositories = reference.coordinates();
    if !event.tags.iter().any(|t| {
        t.as_slice()[0] == "a"
            && t.as_slice()
                .get(1)
                .is_some_and(|v| repositories.iter().any(|r| r.coordinate.to_string() == *v))
    }) {
        return false;
    }
    match target {
        Target::Commit { .. } => true,
        Target::PullRequest {
            anchor, revision, ..
        } => {
            let supplying = event
                .tags
                .iter()
                .find(|t| t.as_slice()[0] == "e")
                .and_then(|t| t.as_slice().get(1))
                .and_then(|s| EventId::from_hex(s).ok());
            supplying.map_or(anchor == revision, |id| id == *revision)
        }
    }
}

async fn emit(event: &Event, json: bool) -> Result<()> {
    if json {
        crate::output::write_json_line(&serde_json::json!({"event": crate::ci_event_view::decoded(event), "signature_verified": true})).await
    } else {
        use tokio::io::AsyncWriteExt;
        let line = format!("{}\n", crate::ci_event_view::summary(event));
        let mut out = tokio::io::stdout();
        out.write_all(line.as_bytes()).await?;
        out.flush().await?;
        Ok(())
    }
}
