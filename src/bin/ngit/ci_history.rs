//! Concise historical evidence. It never contributes to current CI gates.
use std::path::Path;

use anyhow::Result;
use ngit::{
    ci::{
        events::{RunState, WorkflowRun, group_workflow_runs, latest_attempts},
        kinds::ProgressStatus,
    },
    client::get_events_from_local_cache,
    repo_ref::RepoRef,
};
use nostr::prelude::Timestamp;
use serde::Serialize;

use crate::ci_projection::Target;

#[derive(Debug, Serialize)]
pub struct AttemptSummary {
    pub coordinator: String,
    pub workflow: String,
    pub run_id: String,
    pub commit: String,
    pub context: Option<ngit::ci::kinds::TriggerContext>,
    pub status: String,
    pub event: String,
    pub event_type: &'static str,
    pub time_kind: &'static str,
    pub timestamp: u64,
}
impl AttemptSummary {
    fn new(run: &WorkflowRun, now: Timestamp) -> Self {
        let status = match run.state(now) {
            RunState::Concluded(c) => c.to_string(),
            RunState::Running => run
                .progress
                .as_ref()
                .map_or("running", |p| p.status.as_str())
                .into(),
            RunState::Stale => "stale".into(),
        };
        // Results have no distinct completion timestamp in the current protocol.
        // A publication timestamp is therefore always labelled "reported".
        let (time_kind, timestamp) = if run.result.is_some() {
            ("reported", run.container_created_at())
        } else if let Some(p) = &run.progress {
            if p.status == ProgressStatus::Queued {
                p.queued_at
                    .map(|t| ("queued", t))
                    .unwrap_or(("reported", p.created_at))
            } else {
                p.started_at
                    .map(|t| ("started", t))
                    .unwrap_or(("reported", p.created_at))
            }
        } else {
            ("reported", run.container_created_at())
        };
        Self {
            coordinator: run.coordinator.to_hex(),
            workflow: run.workflow_path.clone(),
            run_id: run.run_id.clone(),
            commit: run.commit().into(),
            context: run.common().context.clone(),
            status,
            event: run.container_event_id().to_hex(),
            event_type: if run.result.is_some() {
                "result"
            } else {
                "progress"
            },
            time_kind,
            timestamp: timestamp.as_secs(),
        }
    }
    pub fn print(&self, inspect: bool, now: Timestamp) {
        println!(
            "  {} · coordinator {} · {} · {} {}",
            self.workflow,
            self.coordinator,
            self.status,
            self.time_kind,
            age(self.timestamp, now.as_secs())
        );
        if inspect {
            println!("    ngit ci inspect {}", self.event);
        }
    }
}

#[derive(Default, Serialize)]
pub struct History {
    pub earlier_attempts: Vec<AttemptSummary>,
}
impl History {
    pub fn print(&self) {
        if !self.earlier_attempts.is_empty() {
            println!("Earlier attempts for this tip — not current results:");
            for attempt in &self.earlier_attempts {
                attempt.print(true, Timestamp::now());
            }
        }
    }
}

pub async fn load(path: &Path, reference: &RepoRef, target: &Target) -> Result<History> {
    let events = get_events_from_local_cache(path, vec![target.ci_filter()]).await?;
    let grouped = group_workflow_runs(&events);
    let roots = reference.coordinates();
    let runs: Vec<_> = grouped
        .runs
        .into_iter()
        .filter(|r| {
            r.common()
                .repositories
                .iter()
                .any(|a| roots.iter().any(|root| root.coordinate == a.coordinate))
        })
        .collect();
    let selected = target.select(&runs);
    Ok(History {
        earlier_attempts: earlier(&selected.current, Timestamp::now()),
    })
}
fn earlier(runs: &[WorkflowRun], now: Timestamp) -> Vec<AttemptSummary> {
    let latest = latest_attempts(runs);
    let mut old: Vec<_> = runs
        .iter()
        .filter(|r| {
            !latest
                .iter()
                .any(|current| current.container_event_id() == r.container_event_id())
        })
        .collect();
    old.sort_by_key(|r| std::cmp::Reverse(r.attempt_position()));
    old.into_iter()
        .map(|r| AttemptSummary::new(r, now))
        .collect()
}
fn age(timestamp: u64, now: u64) -> String {
    if timestamp > now {
        return "at a future timestamp".into();
    }
    let seconds = now - timestamp;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else if seconds < 604800 {
        format!("{}d ago", seconds / 86400)
    } else {
        format!("{}w ago", seconds / 604800)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_times_do_not_turn_future_timestamps_into_recent_history() {
        assert_eq!(age(0, 1200), "20m ago");
        assert_eq!(age(0, 172800), "2d ago");
        assert_eq!(age(0, 1209600), "2w ago");
        assert_eq!(age(200, 100), "at a future timestamp");
    }
}
