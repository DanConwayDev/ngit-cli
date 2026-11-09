//! A verified declaration of expected jobs, never a local scheduler.
use std::{fmt::Write as _, path::Path};

use ngit::{
    ci::{details::JobDetails, events::WorkflowRun},
    git::Repo,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize)]
pub struct JobPlan {
    pub id: String,
    pub label: String,
    pub steps: Vec<StepPlan>,
    pub dynamic: bool,
    pub needs: Vec<String>,
    pub condition: Option<Value>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepPlan {
    pub id: String,
    pub label: String,
    pub condition: Option<Value>,
}

pub fn load(repo: &Repo, run: &WorkflowRun) -> Vec<JobPlan> {
    let Some(commit) = crate::ci_commit::first_local_commit(repo, &run.commits) else {
        return Vec::new();
    };
    let blob = commit
        .tree()
        .ok()
        .and_then(|t| t.get_path(Path::new(&run.workflow_path)).ok())
        .and_then(|e| e.to_object(&repo.git_repo).ok())
        .and_then(|o| o.into_blob().ok());
    let Some(blob) = blob else {
        return Vec::new();
    };
    if bitcoin_hashes::sha256::Hash::hash(blob.content()).to_string() != run.workflow_hash {
        return Vec::new();
    }
    let Ok(yaml) = std::str::from_utf8(blob.content()) else {
        return Vec::new();
    };
    parse(yaml, run.workflow_path.starts_with(".ngit/nix/"))
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object()?.get(key)
}

fn parse(yaml: &str, native: bool) -> Vec<JobPlan> {
    let Ok(root) = serde_saphyr::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    let Some(jobs) = field(&root, "jobs").and_then(Value::as_object) else {
        return Vec::new();
    };
    jobs.iter()
        .map(|(key, job)| {
            let id = key.to_owned();
            let label = field(job, "name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_owned();
            let dynamic = field(job, "strategy").is_some() || field(job, "uses").is_some();
            let steps = field(job, "steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|step| {
                    if native {
                        let id = step
                            .as_str()
                            .or_else(|| Some(step.as_object()?.keys().next()?.as_str()))?;
                        Some(StepPlan {
                            id: id.to_owned(),
                            label: id.to_owned(),
                            condition: step
                                .as_object()
                                .and_then(|o| o.get(id))
                                .and_then(|v| field(v, "if"))
                                .cloned(),
                        })
                    } else {
                        // act's generated/nested IDs are runtime identities; only an
                        // explicit, non-expression ID is safe to match to an event.
                        let id = field(step, "id")?.as_str()?;
                        if id.contains("${{") {
                            return None;
                        }
                        Some(StepPlan {
                            id: id.to_owned(),
                            label: field(step, "name")
                                .and_then(Value::as_str)
                                .unwrap_or(id)
                                .to_owned(),
                            condition: field(step, "if").cloned(),
                        })
                    }
                })
                .collect();
            JobPlan {
                id,
                label,
                steps,
                dynamic,
                needs: match field(job, "needs") {
                    Some(Value::String(value)) => vec![value.clone()],
                    Some(Value::Array(values)) => values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    _ => Vec::new(),
                },
                condition: field(job, "if").cloned(),
            }
        })
        .collect()
}

/// First signed observations of active jobs and steps.
#[derive(Default)]
pub struct LiveStepClocks(std::collections::HashMap<String, u64>);
impl LiveStepClocks {
    pub(crate) fn elapsed(&mut self, key: String, reported: u64, now: u64) -> u64 {
        self.elapsed_with_offset(key, reported, now, 0)
    }

    pub(crate) fn elapsed_with_offset(
        &mut self,
        key: String,
        reported: u64,
        now: u64,
        offset_ms: u64,
    ) -> u64 {
        let origin = reported.saturating_mul(1000).saturating_sub(offset_ms);
        let since = self.0.entry(key).or_insert(origin);
        *since = (*since).min(origin);
        now.saturating_mul(1000).saturating_sub(*since)
    }
}

pub fn live_steps(
    job: &ngit::ci::kinds::JobProgress,
    plan: Option<&JobPlan>,
    clocks: &mut LiveStepClocks,
    now: nostr::prelude::Timestamp,
) -> String {
    let mut details = job.details.clone();
    let explicit = job.started_at.is_some()
        || details
            .steps
            .iter()
            .any(|s| matches!(s.outcome.as_str(), "pending" | "running"));
    let mut lower_bounds = std::collections::HashSet::new();
    for step in &mut details.steps {
        if (step.outcome == "running"
            || (!explicit && matches!(step.outcome.as_str(), "unknown" | "in_progress")))
            && step.duration_ms.is_none()
        {
            let key = format!(
                "{}:{:?}:{}:{}:{:?}",
                job.author, job.details.allocation, job.job_id, step.id, step.start_ms
            );
            step.outcome = "running".into();
            step.duration_ms = Some(
                if let Some((start, offset)) = job.started_at.zip(step.start_ms) {
                    now.as_secs()
                        .saturating_mul(1000)
                        .saturating_sub(start.as_secs().saturating_mul(1000).saturating_add(offset))
                } else {
                    lower_bounds.insert(step.id.clone());
                    clocks.elapsed(key, job.created_at.as_secs(), now.as_secs())
                },
            );
        }
    }
    render_steps(&details, plan, true, explicit, &lower_bounds)
}

/// Render observed steps and declared work without evaluating conditions.
pub fn steps(details: &JobDetails, plan: Option<&JobPlan>, live: bool) -> String {
    render_steps(
        details,
        plan,
        live,
        false,
        &std::collections::HashSet::new(),
    )
}

fn render_steps(
    details: &JobDetails,
    plan: Option<&JobPlan>,
    live: bool,
    explicit: bool,
    lower_bounds: &std::collections::HashSet<String>,
) -> String {
    let mut output = String::new();
    let mut conditional = 0;
    let mut skipped = 0;
    // Keep the signed execution order, including provider-added preparation
    // and cleanup. YAML declares expected work; it must not reorder evidence.
    for step in &details.steps {
        let expected = plan.and_then(|plan| plan.steps.iter().find(|p| p.id == step.id));
        if live
            && ((untimed_skip(step) && !explicit) || step.outcome == "pending")
            && expected.is_some_and(|p| p.condition.is_some())
        {
            conditional += 1;
        } else if step.outcome == "skipped" && !(live && !explicit && untimed_skip(step)) {
            skipped += 1;
        } else {
            write_step(
                &mut output,
                step,
                details,
                live && !explicit,
                lower_bounds.contains(&step.id),
            );
        }
    }
    if let Some(plan) = plan {
        for expected in &plan.steps {
            if details.steps.iter().any(|step| step.id == expected.id) {
                continue;
            }
            if expected.condition.is_some() {
                conditional += 1;
            } else {
                let _ = write!(
                    output,
                    "\n{}",
                    crate::ci_display::row("waiting", &expected.label, 2)
                );
            }
        }
    }
    if conditional > 0 {
        let _ = write!(
            output,
            "\n    {conditional} conditional steps not yet reported"
        );
    }
    if skipped > 0 {
        let _ = write!(
            output,
            "\n    {skipped} skipped steps (ci inspect --json for details)"
        );
    }
    output
}

fn untimed_skip(step: &ngit::ci::details::Step) -> bool {
    step.outcome == "skipped" && step.start_ms.is_none() && step.duration_ms.is_none()
}

pub fn pending_job(plan: &JobPlan) -> String {
    let mut line = crate::ci_display::row("waiting", &plan.label, 1);
    if !plan.needs.is_empty() {
        let _ = write!(line, " · needs: {}", plan.needs.join(", "));
    }
    if plan.condition.is_some() {
        line.push_str(" · conditional");
    }
    if plan.dynamic {
        line.push_str(" · runtime expansion");
    }
    line
}

fn write_step(
    output: &mut String,
    step: &ngit::ci::details::Step,
    details: &JobDetails,
    live: bool,
    lower_bound: bool,
) {
    let duration = step.duration_ms.map(|ms| {
        if lower_bound {
            format!("≥{}", duration(ms))
        } else {
            duration(ms)
        }
    });
    let state = if live
        && step.outcome == "skipped"
        && step.start_ms.is_none()
        && step.duration_ms.is_none()
    {
        "waiting"
    } else if live
        && step.outcome == "unknown"
        && step.start_ms.is_some()
        && step.duration_ms.is_none()
    {
        // ngit-ci reports a started step with no conclusion until it finishes.
        "running"
    } else {
        &step.outcome
    };
    let label = if step.label.is_empty() || step.label == step.id {
        step.id.clone()
    } else {
        format!("{} ({})", step.label, step.id)
    };
    let direct = details
        .artifacts
        .iter()
        .filter(|a| a.step.as_deref() == Some(&step.id))
        .count();
    let manifests: Vec<_> = details
        .artifact_manifests
        .iter()
        .filter(|a| a.step.as_deref() == Some(&step.id))
        .collect();
    let known = manifests
        .iter()
        .filter_map(|a| a.count)
        .fold(0, u64::saturating_add);
    let count = (direct as u64).saturating_add(known);
    let _ = write!(
        output,
        "\n{}",
        crate::ci_display::timed_row(state, &label, 2, duration.as_deref())
    );
    if count > 0 {
        let _ = write!(output, " · {count} artifacts");
    }
    if manifests.iter().any(|a| a.count.is_none()) {
        let _ = write!(output, " · artifact manifest (count unknown)");
    }
    if let Some(state) = &step.artifacts {
        let _ = write!(output, " · {state}");
    }
}

pub fn artifact_links(details: &JobDetails) -> String {
    let mut output = String::new();
    for artifact in details.artifacts.iter().take(4) {
        let _ = write!(
            output,
            "\n      artifact {}/{}: {}",
            artifact.name, artifact.filename, artifact.url
        );
    }
    for manifest in details.artifact_manifests.iter().take(4) {
        let count = manifest
            .count
            .map_or("count unknown".to_owned(), |n| format!("{n} artifacts"));
        let _ = write!(
            output,
            "\n      artifact manifest ({count}): {}",
            manifest.manifest
        );
    }
    if details.artifacts.len() > 4 || details.artifact_manifests.len() > 4 {
        let _ = write!(
            output,
            "\n      {} direct artifacts, {} manifests; use ci inspect --json for all references",
            details.artifacts.len(),
            details.artifact_manifests.len()
        );
    }
    output
}

pub fn duration(ms: u64) -> String {
    let seconds = ms / 1000 + u64::from(ms % 1000 >= 500);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:2}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations_round_to_seconds_and_carry_into_minutes() {
        assert_eq!(duration(71), "0s");
        assert_eq!(duration(1500), "2s");
        assert_eq!(duration(59_500), "1m 00s");
        assert_eq!(duration(123_000), "2m 03s");
        assert_eq!(duration(3_599_499), "59m 59s");
        assert_eq!(duration(3_599_500), "1h  0m");
        assert_eq!(duration(4_095_000), "1h  8m");
        assert_eq!(duration(7_199_000), "1h 59m");
        assert_eq!(duration(7_200_000), "2h  0m");
        assert!(!duration(u64::MAX).is_empty());
    }

    #[test]
    fn yaml_does_not_move_runtime_preparation_or_cleanup_around_failure() {
        let plan = parse("jobs:\n  test:\n    steps: [publish]\n", true);
        let details = JobDetails {
            steps: [
                ("prepare", "success"),
                ("publish", "failure"),
                ("cleanup", "success"),
            ]
            .into_iter()
            .map(|(id, outcome)| ngit::ci::details::Step {
                id: id.into(),
                label: id.into(),
                outcome: outcome.into(),
                start_ms: None,
                duration_ms: Some(1000),
                artifacts: None,
            })
            .collect(),
            ..JobDetails::default()
        };
        let rendered = steps(&details, Some(&plan[0]), false);
        assert!(rendered.find("prepare") < rendered.find("publish"));
        assert!(rendered.find("publish") < rendered.find("cleanup"));
    }

    #[test]
    fn plans_only_match_stable_step_ids_and_never_evaluate_conditions() {
        let native = parse(
            "jobs:\n  build:\n    if: false\n    steps:\n      - compile\n      - verify: {run: tests}\n",
            true,
        );
        assert_eq!(
            native[0]
                .steps
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["compile", "verify"]
        );
        let act = parse(
            "jobs:\n  build:\n    strategy: {matrix: {os: [linux, mac]}}\n    steps:\n      - run: echo unnamed\n      - id: test\n        name: Unit tests\n        run: tests\n",
            false,
        );
        assert!(act[0].dynamic);
        assert_eq!(act[0].steps.len(), 1);
        assert_eq!(act[0].steps[0].label, "Unit tests");
    }
    #[test]
    fn summary_keeps_timing_and_bounds_artifacts() {
        use ngit::ci::details::{Artifact, Step};
        let mut details = JobDetails::default();
        details.steps.push(Step {
            id: "build".into(),
            label: "Build".into(),
            start_ms: Some(0),
            duration_ms: Some(1234),
            outcome: "success".into(),
            artifacts: None,
        });
        for n in 0..1000 {
            details.artifacts.push(Artifact {
                url: format!("https://example/{n}"),
                filename: n.to_string(),
                name: "out".into(),
                step: Some("build".into()),
            });
        }
        let rendered = format!(
            "{}{}",
            steps(&details, None, false),
            artifact_links(&details)
        );
        assert!(rendered.contains("1s"));
        assert!(rendered.contains("1000 artifacts"));
        assert!(rendered.len() < 1000);
    }
    #[test]
    fn pending_jobs_are_compact_and_conditional_paths_are_not_predicted() {
        let plans = parse(
            "jobs:\n  publish:\n    needs: [build, test]\n    if: {result: test, in: [success]}\n    steps:\n      - upload\n      - diagnose:\n          if: {result: upload, in: [failure]}\n",
            true,
        );
        let plan = &plans[0];
        assert_eq!(plan.needs, ["build", "test"]);
        assert!(plan.condition.is_some());
        assert!(plan.steps[1].condition.is_some());
        let row = pending_job(plan);
        assert!(row.contains("needs: build, test"));
        assert!(row.contains("conditional"));
        assert!(!row.contains("upload"));
        let details = JobDetails::default();
        let rendered = steps(&details, Some(plan), true);
        assert!(rendered.contains("waiting") && rendered.contains("upload"));
        assert!(rendered.contains("1 conditional steps not yet reported"));
        assert!(!rendered.contains("diagnose"));
    }

    #[test]
    fn reported_conditional_execution_wins_and_final_skips_are_compact() {
        use ngit::ci::details::Step;
        let plans = parse(
            "jobs:\n  test:\n    steps:\n      - diagnose:\n          if: {result: test, in: [failure]}\n",
            true,
        );
        let mut details = JobDetails::default();
        details.steps.push(Step {
            id: "diagnose".into(),
            label: "diagnose".into(),
            start_ms: None,
            duration_ms: None,
            outcome: "skipped".into(),
            artifacts: None,
        });
        assert!(
            steps(&details, Some(&plans[0]), true).contains("conditional steps not yet reported")
        );
        assert!(steps(&details, Some(&plans[0]), false).contains("1 skipped steps"));
        // Without a condition, the live placeholder is visible but has no
        // inferred outcome. The final snapshot still establishes a skip.
        let live = steps(&details, None, true);
        assert!(live.contains("waiting") && live.contains("diagnose"));
        assert!(!live.contains("skipped") && !live.contains("planned"));
        assert!(steps(&details, None, false).contains("1 skipped steps"));
        details.steps[0].start_ms = Some(10);
        details.steps[0].duration_ms = Some(1200);
        details.steps[0].outcome = "success".into();
        let rendered = steps(&details, Some(&plans[0]), true);
        assert!(rendered.contains("1s success diagnose"));
        assert!(!rendered.contains("conditional steps"));
    }
    #[test]
    fn started_unfinished_step_is_running_only_in_live_evidence() {
        let mut details = JobDetails::default();
        details.steps.push(ngit::ci::details::Step {
            id: "compile".into(),
            label: String::new(),
            start_ms: Some(0),
            duration_ms: None,
            outcome: "unknown".into(),
            artifacts: None,
        });
        assert!(steps(&details, None, true).contains("running"));
        assert!(steps(&details, None, false).contains("unknown"));
        details.steps[0].start_ms = None;
        assert!(steps(&details, None, true).contains("unknown"));
        details.steps[0].start_ms = Some(0);
        details.steps[0].duration_ms = Some(10);
        assert!(steps(&details, None, true).contains("unknown"));
    }
    #[test]
    fn live_clock_ticks_without_updates_and_survives_renewals() {
        let mut clocks = LiveStepClocks::default();
        assert_eq!(clocks.elapsed("allocation:step".into(), 100, 100), 0);
        assert_eq!(clocks.elapsed("allocation:step".into(), 100, 105), 5000);
        assert_eq!(clocks.elapsed("allocation:step".into(), 104, 106), 6000);
        assert_eq!(clocks.elapsed("other-allocation:step".into(), 106, 106), 0);
        assert_eq!(clocks.elapsed("allocation:next".into(), 107, 106), 0);
    }
}
