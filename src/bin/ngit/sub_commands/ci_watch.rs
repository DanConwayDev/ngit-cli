//! Open-ended observation of distinct workflow/job receipts. No scheduler or
//! synthetic lifecycle events: JSON streams receipts, humans see current
//! status.
use std::{
    collections::HashSet,
    io::{IsTerminal, Write},
};

use anyhow::Result;
use ngit::{
    ci::watch::{Subscription, Update},
    client::{Client, get_events_from_local_cache, save_event_in_local_cache},
    git::{Repo, RepoActions},
    repo_ref::RepoRef,
};
use nostr::prelude::{Event, EventId, Kind, MatchEventOptions};

use crate::ci_projection::{ProjectionRequest, Target, Tier, build_report};

#[derive(Clone, Copy)]
pub struct Mode {
    pub json: bool,
    pub follow: bool,
}

pub async fn launch(
    repo: &Repo,
    reference: &RepoRef,
    client: &Client,
    target: &Target,
    mode: Mode,
    history_scope: &crate::ci_history::Scope,
) -> Result<()> {
    if mode.json {
        crate::output::start_stream();
    }
    let filter = target
        .ci_filter()
        .kinds([9841, 9842, 39842, 39846].map(Kind::Custom));
    let mut subscription = client.watch_ci(&reference.relays, filter).await?;
    // Keep one interrupt listener alive, including during cache and output I/O.
    let result = tokio::select! {
        signal = tokio::signal::ctrl_c() => signal.map_err(anyhow::Error::from),
        result = observe(repo, reference, client, target, mode, history_scope, &mut subscription) => result,
    };
    subscription.close().await;
    result
}

async fn observe(
    repo: &Repo,
    reference: &RepoRef,
    client: &Client,
    target: &Target,
    mode: Mode,
    history_scope: &crate::ci_history::Scope,
    subscription: &mut Subscription,
) -> Result<()> {
    let json = mode.json;
    let mut completion = Completion::default();
    let path = repo.get_path()?;
    let filter = target
        .ci_filter()
        .kinds([9841, 9842, 39842, 39846].map(Kind::Custom));
    let mut initial: Vec<_> = get_events_from_local_cache(path, vec![filter.clone()])
        .await?
        .into_iter()
        .collect();
    initial.sort_by_key(|event| (event.created_at, event.id));
    let mut screen = InlineReport::default();
    let mut clocks = crate::ci_plan::LiveStepClocks::default();
    let mut snapshot = if json {
        None
    } else {
        Some(load_snapshot(repo, reference, client, target, history_scope).await?)
    };
    if !mode.follow {
        completion.update(
            settled(&initial, target, reference),
            tokio::time::Instant::now(),
        );
    }
    if let Some((report, history)) = &snapshot {
        screen.draw(&snapshot_text(
            report,
            history,
            target,
            &mut clocks,
            completion.remaining(tokio::time::Instant::now()),
        ))?;
    }
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut seen = emit_initial(initial, target, reference, json).await?;
    loop {
        let update = tokio::select! {
            biased;
            update = subscription.next() => update?,
            _ = ticker.tick() => {
                let remaining = completion.remaining(tokio::time::Instant::now());
                if remaining == Some(0) {
                    if let Some((report, history)) = &snapshot {
                        screen.draw(&snapshot_text(report, history, target, &mut clocks, remaining))?;
                    }
                    return Ok(());
                }
                if std::io::stdout().is_terminal() {
                    if let Some((report, history)) = &snapshot {
                        screen.draw(&snapshot_text(report, history, target, &mut clocks, remaining))?;
                    }
                }
                continue;
            }
        };
        match update {
            Update::Notice(message) => {
                screen.clear()?;
                completion.update(false, tokio::time::Instant::now());
                eprintln!("CI watch: {message}");
            }
            Update::Event { relay: _, event } => {
                if filter.match_event(&event, MatchEventOptions::default())
                    && matches_target(target, reference, &event)
                    && event.verify().is_ok()
                    && seen.insert(event.id)
                {
                    if json {
                        emit(&event).await?;
                    }
                    save_event_in_local_cache(path, &event).await?;
                    if !mode.follow {
                        let cached = get_events_from_local_cache(path, vec![filter.clone()])
                            .await?
                            .into_iter()
                            .collect::<Vec<_>>();
                        completion.update(
                            settled(&cached, target, reference),
                            tokio::time::Instant::now(),
                        );
                    }
                    if !json {
                        let updated =
                            load_snapshot(repo, reference, client, target, history_scope).await?;
                        screen.draw(&snapshot_text(
                            &updated.0,
                            &updated.1,
                            target,
                            &mut clocks,
                            completion.remaining(tokio::time::Instant::now()),
                        ))?;
                        snapshot = Some(updated);
                    }
                }
            }
        }
    }
}

async fn emit_initial(
    initial: Vec<Event>,
    target: &Target,
    reference: &RepoRef,
    json: bool,
) -> Result<HashSet<EventId>> {
    let mut seen = HashSet::new();
    for event in initial {
        if matches_target(target, reference, &event)
            && event.verify().is_ok()
            && seen.insert(event.id)
            && json
        {
            emit(&event).await?;
        }
    }
    if json {
        eprintln!(
            "{}",
            console::style(format!(
                "Watching CI for {}; Ctrl-C stops observation without cancelling execution.",
                target.describe()
            ))
            .for_stderr()
            .dim()
        );
    }
    Ok(seen)
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

async fn emit(event: &Event) -> Result<()> {
    crate::output::write_json_line(&serde_json::json!({"event": crate::ci_event_view::decoded(event), "signature_verified": true})).await
}

async fn load_snapshot(
    repo: &Repo,
    reference: &RepoRef,
    client: &Client,
    target: &Target,
    history_scope: &crate::ci_history::Scope,
) -> Result<(crate::ci_projection::CiReport, crate::ci_history::History)> {
    let path = repo.get_path()?;
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
    let history = crate::ci_history::load(repo, history_scope, path, reference, target).await?;
    Ok((report, history))
}

fn snapshot_text(
    report: &crate::ci_projection::CiReport,
    history: &crate::ci_history::History,
    target: &Target,
    clocks: &mut crate::ci_plan::LiveStepClocks,
    remaining: Option<u64>,
) -> String {
    let footer = match remaining {
        Some(0) => "All observed workflows finished".into(),
        Some(seconds) => format!("All observed workflows finished · stopping in {seconds}s"),
        None => "Watching for CI updates · Ctrl-C stops watching, not CI".into(),
    };
    format!(
        "{}{}\n{}\n",
        crate::ci_display::render_status(report, target, clocks),
        history.render(),
        console::style(footer).dim()
    )
}

/// Repaint only our own previous rows, leaving the terminal and scrollback
/// alone.
#[derive(Default)]
struct InlineReport {
    previous: String,
}
impl InlineReport {
    fn clear(&mut self) -> Result<()> {
        if std::io::stdout().is_terminal() && !self.previous.is_empty() {
            let term = console::Term::stdout();
            let (height, width) = term.size();
            let rows = display_rows(&self.previous, usize::from(width));
            // A report taller than the viewport has already scrolled. Append a
            // new snapshot rather than erasing unrelated history above it.
            if rows < usize::from(height) {
                term.clear_last_lines(rows)?;
            }
        }
        self.previous.clear();
        Ok(())
    }
    fn draw(&mut self, text: &str) -> Result<()> {
        let fitted;
        let text = if std::io::stdout().is_terminal() {
            let (height, width) = console::Term::stdout().size();
            fitted = fit_report(text, usize::from(height), usize::from(width));
            fitted.as_str()
        } else {
            text
        };
        if self.previous == text {
            return Ok(());
        }
        self.clear()?;
        std::io::stdout().write_all(text.as_bytes())?;
        std::io::stdout().flush()?;
        self.previous = text.into();
        Ok(())
    }
}

fn fit_report(text: &str, height: usize, width: usize) -> String {
    let budget = height.saturating_sub(1).max(1);
    if display_rows(text, width) <= budget {
        return text.into();
    }
    let mut fitted = String::new();
    let mut used = 0;
    for line in text.lines() {
        let rows = display_rows(line, width).max(1);
        if used + rows >= budget {
            break;
        }
        fitted.push_str(line);
        fitted.push('\n');
        used += rows;
    }
    fitted.push_str(
        &console::style(console::truncate_str(
            "… more details: ngit ci inspect",
            width.max(1),
            "…",
        ))
        .dim()
        .to_string(),
    );
    fitted.push('\n');
    fitted
}

fn display_rows(text: &str, width: usize) -> usize {
    text.lines()
        .map(|line| {
            console::measure_text_width(line)
                .max(1)
                .div_ceil(width.max(1))
        })
        .sum()
}

fn settled(events: &[Event], target: &Target, reference: &RepoRef) -> bool {
    let grouped = ngit::ci::events::group_workflow_runs(
        events
            .iter()
            .filter(|event| matches_target(target, reference, event) && event.verify().is_ok()),
    );
    let selected = target.select(&grouped.runs);
    finished_states(
        &selected
            .current
            .iter()
            .map(|run| run.state(nostr::prelude::Timestamp::now()))
            .collect::<Vec<_>>(),
        !grouped.skipped.is_empty(),
    )
}

fn finished_states(states: &[ngit::ci::events::RunState], incomplete: bool) -> bool {
    !incomplete
        && !states.is_empty()
        && states
            .iter()
            .all(|state| matches!(state, ngit::ci::events::RunState::Concluded(_)))
}

#[derive(Default)]
struct Completion {
    since: Option<tokio::time::Instant>,
}
impl Completion {
    fn update(&mut self, settled: bool, now: tokio::time::Instant) {
        self.since = settled.then_some(now);
    }
    fn remaining(&self, now: tokio::time::Instant) -> Option<u64> {
        self.since
            .map(|since| 10_u64.saturating_sub(now.duration_since(since).as_secs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_rows_account_for_wrapping_blank_lines_and_colour() {
        assert_eq!(display_rows("\x1b[32msuccess\x1b[0m\n\n123456789\n", 8), 4);
        assert_eq!(display_rows("12345678\n", 8), 1);
        assert_eq!(display_rows("", 80), 0);
    }
    #[test]
    fn oversized_reports_fit_without_scrolling_on_each_tick() {
        let text = "first\nsecond\nthird\nfourth\n";
        assert!(display_rows(&fit_report(text, 4, 80), 80) < 4);
        assert!(fit_report(text, 4, 80).contains("ngit ci inspect"));
        assert_eq!(fit_report(text, 20, 80), text);
    }
    #[test]
    fn queued_running_stale_missing_and_incomplete_work_never_finish() {
        use ngit::ci::{events::RunState, kinds::Conclusion};
        let done = RunState::Concluded(Conclusion::Success);
        assert!(finished_states(&[done], false));
        assert!(!finished_states(&[], false));
        assert!(!finished_states(&[done], true));
        assert!(!finished_states(&[done, RunState::Running], false));
        assert!(!finished_states(&[done, RunState::Stale], false));
    }
    #[tokio::test(start_paused = true)]
    async fn grace_resets_on_updates_and_is_cancelled_by_queued_work() {
        use tokio::time::{Instant, advance};
        let mut completion = Completion::default();
        assert_eq!(completion.remaining(Instant::now()), None);
        completion.update(true, Instant::now());
        advance(std::time::Duration::from_secs(9)).await;
        assert_eq!(completion.remaining(Instant::now()), Some(1));
        completion.update(false, Instant::now());
        advance(std::time::Duration::from_secs(20)).await;
        assert_eq!(completion.remaining(Instant::now()), None);
        completion.update(true, Instant::now());
        advance(std::time::Duration::from_secs(9)).await;
        completion.update(true, Instant::now());
        assert_eq!(completion.remaining(Instant::now()), Some(10));
        advance(std::time::Duration::from_secs(10)).await;
        assert_eq!(completion.remaining(Instant::now()), Some(0));
    }
}
