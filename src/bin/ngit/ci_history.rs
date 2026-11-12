//! Concise historical evidence. It never contributes to current CI gates.
use std::{fmt::Write as _, path::Path};

use anyhow::Result;
use console::style;
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
    pub(crate) fn new(run: &WorkflowRun, now: Timestamp) -> Self {
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
        let (time_kind, timestamp) = if matches!(run.state(now), RunState::Concluded(_)) {
            ("reported", run.container_created_at())
        } else if let Some(p) = &run.progress {
            if p.status == ProgressStatus::Queued {
                p.queued_at
                    .map_or(("reported", p.created_at), |t| ("queued", t))
            } else {
                p.started_at
                    .map_or(("reported", p.created_at), |t| ("started", t))
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
    fn render(&self, inspect: bool, now: Timestamp) -> String {
        let mut output = String::new();
        let _ = writeln!(
            output,
            "{}",
            style(format!(
                "  {} · {} · {}",
                self.workflow,
                self.status,
                age(self.timestamp, now.as_secs())
            ))
            .dim()
        );
        if inspect {
            let _ = writeln!(
                output,
                "{}",
                style(format!("    `ngit ci inspect {}`", self.event)).dim()
            );
        }
        output
    }
}

pub struct Scope {
    pub automatic: bool,
    pub selected_remote: Option<String>,
}

#[derive(Default, Serialize)]
pub struct History {
    pub earlier_attempts: Vec<AttemptSummary>,
    pub previous_tip: Option<PreviousTip>,
    pub notice: Option<String>,
    #[serde(rename = "history_complete")]
    pub complete: bool,
}
#[derive(Serialize)]
pub struct PreviousTip {
    pub commit: String,
    pub source: &'static str,
    pub workflows: Vec<AttemptSummary>,
    pub command: String,
}
impl History {
    pub fn print(&self) {
        print!("{}", self.render());
    }
    pub fn render(&self) -> String {
        let mut output = String::new();
        if !self.earlier_attempts.is_empty() {
            let _ = writeln!(
                output,
                "{}",
                style("Earlier attempts for this tip — not current results:").dim()
            );
            for attempt in &self.earlier_attempts {
                output.push_str(&attempt.render(true, Timestamp::now()));
            }
        }
        if let Some(previous) = &self.previous_tip {
            for run in &previous.workflows {
                let name = run.workflow.rsplit('/').next().unwrap_or(&run.workflow);
                let label = if previous.workflows.iter().any(|other| {
                    other.workflow != run.workflow
                        && other.workflow.rsplit('/').next() == Some(name)
                }) {
                    &run.workflow
                } else {
                    name
                };
                let _ = writeln!(
                    output,
                    "{}",
                    style(format!(
                        "Previous tip {} · {label}: {} · {}",
                        crate::ci_projection::short(&previous.commit),
                        run.status,
                        age(run.timestamp, Timestamp::now().as_secs())
                    ))
                    .dim()
                );
            }
        }
        if let Some(notice) = &self.notice {
            if self
                .previous_tip
                .as_ref()
                .is_none_or(|tip| tip.source != "repository_branch_history")
            {
                let _ = writeln!(output, "{}", style(notice).dim());
            }
        }
        output
    }
}

pub async fn load(
    repo: &ngit::git::Repo,
    scope: &Scope,
    path: &Path,
    reference: &RepoRef,
    target: &Target,
) -> Result<History> {
    let events = get_events_from_local_cache(path, vec![target.ci_filter()]).await?;
    let grouped = group_workflow_runs(events.iter().filter(|e| e.verify().is_ok()));
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
    let mut history = History {
        earlier_attempts: earlier(&selected.current, Timestamp::now()),
        ..History::default()
    };
    if !selected.current.is_empty() {
        return Ok(history);
    }
    if !scope.automatic && matches!(target, Target::Commit { .. }) {
        return Ok(history);
    }
    let filters = vec![
        nostr::prelude::Filter::new()
            .kinds(ngit::ci::kinds::CONSUMED_CI_KINDS.to_vec())
            .custom_tags(
                nostr::prelude::SingleLetterTag::LOWERCASE_A,
                roots
                    .iter()
                    .map(|r| r.coordinate.to_string())
                    .collect::<Vec<_>>(),
            ),
    ];
    let events = get_events_from_local_cache(path, filters).await?;
    let all = group_workflow_runs(events.iter().filter(|e| e.verify().is_ok()));
    let candidates = crate::ci_previous::resolve(repo, reference, target, scope, all.runs).await?;
    history.notice = candidates.notice;
    for commit in candidates.commits {
        let at_tip: Vec<_> = candidates
            .runs
            .iter()
            .filter(|r| r.commit() == commit)
            .cloned()
            .collect();
        if at_tip.is_empty() {
            continue;
        }
        history.previous_tip = Some(PreviousTip {
            command: format!("ngit ci status {commit}"),
            commit,
            source: candidates.source,
            workflows: latest_attempts(&at_tip)
                .into_iter()
                .map(|r| AttemptSummary::new(r, Timestamp::now()))
                .collect(),
        });
        break;
    }
    Ok(history)
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
pub(crate) fn age(timestamp: u64, now: u64) -> String {
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
    } else if seconds < 604_800 {
        format!("{}d ago", seconds / 86400)
    } else if seconds < 30 * 86400 {
        format!("{}w ago", seconds / 604_800)
    } else if seconds < 365 * 86400 {
        format!("{}mo ago", seconds / (30 * 86400))
    } else {
        format!("{}y ago", seconds / (365 * 86400))
    }
}
pub fn context_label(context: Option<&ngit::ci::kinds::TriggerContext>) -> String {
    use ngit::ci::kinds::TriggerContext;
    match context {
        Some(TriggerContext::Push { git_ref }) => git_ref.clone(),
        Some(TriggerContext::PullRequest(pr)) => {
            format!("PR {}", crate::ci_projection::short(&pr.root.to_hex()))
        }
        Some(TriggerContext::Issue {
            root,
            activity,
            action,
            ..
        }) => format!(
            "issue {} {action} ({})",
            crate::ci_projection::short(&root.to_hex()),
            crate::ci_projection::short(&activity.to_hex())
        ),
        Some(TriggerContext::Release {
            version,
            channel,
            revision,
            ..
        }) => format!(
            "release {version} {channel} ({})",
            crate::ci_projection::short(&revision.to_hex())
        ),
        None => "unspecified trigger context".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_times_do_not_turn_future_timestamps_into_recent_history() {
        assert_eq!(age(0, 1200), "20m ago");
        assert_eq!(age(0, 172_800), "2d ago");
        assert_eq!(age(0, 1_209_600), "2w ago");
        assert_eq!(age(0, 90 * 86400), "3mo ago");
        assert_eq!(age(0, 365 * 86400), "1y ago");
        assert_eq!(age(200, 100), "at a future timestamp");
    }
}
