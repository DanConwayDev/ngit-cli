//! Human CI summaries. Structured evidence stays in the projection's JSON.
use std::fmt::Write as _;

use console::{StyledObject, style};
use ngit::ci::{details::JobDetails, trust::TrustClassification};
use nostr::prelude::Timestamp;

use crate::ci_projection::{CiReport, JobReport, RunReport, Target};

pub fn outcome(value: &str) -> StyledObject<&str> {
    match value {
        "success" => style(value).green(),
        "failure" | "timed_out" | "startup_failure" | "error" => style(value).red().bold(),
        "running" | "in_progress" => style(value).cyan(),
        "queued" | "pending" | "stale" | "cancelled" => style(value).yellow(),
        _ => style(value).dim(),
    }
}

pub fn row(state: &str, name: &str, depth: usize) -> String {
    timed_row(state, name, depth, None)
}

pub fn timed_row(state: &str, name: &str, depth: usize, duration: Option<&str>) -> String {
    let state = if state == "in_progress" {
        "running"
    } else {
        state
    };
    if depth == 0 {
        format!("{: <7} {}", outcome(state).bold(), style(name).bold())
    } else {
        format!(
            "{}{} {: <7} {}",
            "  ".repeat(depth.saturating_sub(1)),
            style(format!("{:>8}", duration.unwrap_or("—"))).dim(),
            outcome(state),
            name
        )
    }
}

pub fn clean_log(tail: &str) -> String {
    console::strip_ansi_codes(tail)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

pub fn log_tail_preview(tail: &str) -> String {
    let clean = clean_log(tail);
    let mut lines: Vec<_> = clean.trim_end_matches('\n').lines().rev().take(3).collect();
    lines.reverse();
    lines.join("\n")
}

pub fn failure_details(job: &JobReport, plan: Option<&crate::ci_plan::JobPlan>) -> String {
    use ngit::ci::kinds::Conclusion;
    if !matches!(
        job.conclusion,
        Conclusion::Failure | Conclusion::TimedOut | Conclusion::StartupFailure
    ) {
        return String::new();
    }
    let mut output = crate::ci_plan::steps(&job.details, plan, false);
    let tail = log_tail_preview(&job.log_tail);
    if !tail.is_empty() {
        let _ = write!(output, "\n    {}", style("┌─ last log lines ─").dim());
        for line in tail.lines() {
            let _ = write!(output, "\n    {}", style(format!("│ {line}")).dim());
        }
        let _ = write!(output, "\n    {}", style("└─ end log preview ─").dim());
    }
    output
}

fn short_run_id(id: &str) -> String {
    let mut chars = id.chars();
    let mut short: String = chars.by_ref().take(8).collect();
    if chars.next().is_some() {
        short.push('…');
    }
    short
}

pub fn run_metadata(run: &RunReport) -> String {
    let now = Timestamp::now();
    let summary = crate::ci_history::AttemptSummary::new(&run.run, now);
    let mut metadata = format!(
        "run {} · {}",
        short_run_id(&run.run.run_id),
        crate::ci_history::age(summary.timestamp, now.as_secs())
    );
    if run.attempt_of > 1 {
        let _ = write!(metadata, " · attempt {}", run.attempt_of);
    }
    metadata
}

pub fn elapsed_to_report(started: Option<u64>, reported: u64) -> Option<u64> {
    reported
        .checked_sub(started?)
        .and_then(|seconds| seconds.checked_mul(1000))
}

pub fn elapsed_label(ms: Option<u64>) -> Option<String> {
    ms.map(|ms| format!("~{}", crate::ci_plan::duration(ms)))
}

pub fn print_status(report: &CiReport, target: &Target) {
    let heading = match target {
        Target::Commit { ids, .. } => format!(
            "CI for {} ({})",
            target.describe(),
            ids.first().map_or("unknown commit", String::as_str)
        ),
        Target::PullRequest { .. } => format!("CI for {}", target.describe()),
    };
    println!("{}", style(heading).bold());
    if report.runs.is_empty() {
        println!("  No CI evidence for this commit");
        return;
    }
    for run in &report.runs {
        println!(
            "\n{} · {}",
            row(&run.state_label(), &run.run.workflow_path, 0),
            style(run_metadata(run)).dim()
        );
        // Distinguish independent coordinators/contexts without repeating long IDs
        // for the common single-run case. Full identities remain in inspect.
        if report
            .runs
            .iter()
            .filter(|other| other.run.workflow_path == run.run.workflow_path)
            .count()
            > 1
        {
            println!(
                "    {} · coordinator {}",
                crate::ci_history::context_label(run.run.activity_context()),
                run.run.coordinator
            );
        }
        for warning in warnings(run) {
            println!("    {}", style(warning).yellow());
        }
        for job in &run.jobs {
            println!(
                "{}{}",
                timed_row(
                    &job.conclusion.to_string(),
                    job.name.as_deref().unwrap_or(&job.job_id),
                    1,
                    elapsed_label(elapsed_to_report(job.started_at, job.reported_at)).as_deref()
                ),
                failure_details(job, run.plan.iter().find(|p| p.id == job.job_id))
            );
        }
        let now = Timestamp::now();
        for job in run.run.live_jobs.iter().filter(|job| job.is_live(now)) {
            println!(
                "{}{}",
                row("running", &job.job_id, 1),
                step_summary(&job.details, true)
            );
        }
        let running = run.running_jobs();
        for id in &running {
            if !run
                .run
                .live_jobs
                .iter()
                .any(|job| job.job_id == *id && job.is_live(now))
            {
                println!("{}", row("running", id, 1));
            }
        }
        for job in &run.plan {
            if !run.jobs.iter().any(|j| j.job_id == job.id)
                && !running.contains(&job.id.as_str())
                && !run
                    .run
                    .live_jobs
                    .iter()
                    .any(|j| j.job_id == job.id && j.is_live(now))
            {
                println!("{}", row("not reported", &job.label, 1));
            }
        }
    }
    if report.is_incomplete() {
        report.print_coverage();
    }
    print_inspection_tip(target);
}

fn print_inspection_tip(target: &Target) {
    let command = match target {
        Target::Commit { described, .. } if described == "HEAD" => "ngit ci inspect".to_owned(),
        Target::Commit { ids, .. } => format!(
            "ngit ci inspect --target {}",
            ids.first().map_or("HEAD", String::as_str)
        ),
        Target::PullRequest { root, .. } => format!("ngit ci inspect --target {root}"),
    };
    // PR targets need their exact ID, not the checked-out branch's HEAD.
    println!(
        "\n{} {} {}",
        style("Tip:").dim(),
        style(format!("`{command}`")).cyan().bold(),
        style("for details").dim()
    );
}

fn warnings(run: &RunReport) -> Vec<String> {
    let mut warnings = Vec::new();
    match run.resolution.classification() {
        Some(
            TrustClassification::MaintainerDirected | TrustClassification::OperationallyAssociated,
        ) => (),
        Some(_) => warnings.push("No maintainer authorization verified for this run; use `ngit ci inspect` for coordinator evidence.".to_owned()),
        None => warnings.push("Coordinator trust is still being checked".to_owned()),
    }
    if run.integrity.workflow_hash_matches == Some(false) {
        warnings
            .push("Workflow hash MISMATCH — local workflow differs from signed evidence".into());
    }
    warnings
}

/// Only reported failing/active steps are highlighted. Never predict a route
/// through conditional steps or count untimed skipped placeholders as finished.
fn step_summary(details: &JobDetails, live: bool) -> String {
    let relevant = details
        .steps
        .iter()
        .find(|step| {
            matches!(
                step.outcome.as_str(),
                "failure" | "timed_out" | "startup_failure" | "error"
            )
        })
        .or_else(|| {
            if live {
                details
                    .steps
                    .iter()
                    .find(|step| matches!(step.outcome.as_str(), "in_progress" | "running"))
            } else {
                None
            }
        });
    let mut result = String::new();
    if let Some(step) = relevant {
        let action = if matches!(step.outcome.as_str(), "in_progress" | "running") {
            "at"
        } else {
            "failed at"
        };
        let _ = write!(
            result,
            " · {action} {}",
            if step.label.is_empty() {
                &step.id
            } else {
                &step.label
            }
        );
    }
    if live {
        let completed = details
            .steps
            .iter()
            .filter(|step| {
                matches!(
                    step.outcome.as_str(),
                    "success" | "failure" | "cancelled" | "timed_out"
                ) || step.outcome == "skipped" && step.duration_ms.is_some()
            })
            .count();
        if completed > 0 {
            let _ = write!(result, " · {completed} reported steps complete");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use ngit::ci::details::Step;

    use super::*;

    #[test]
    fn failure_preview_keeps_only_the_final_three_lines() {
        assert_eq!(
            log_tail_preview("first\nsecond\nthird\nfourth\n"),
            "second\nthird\nfourth"
        );
        assert_eq!(log_tail_preview("\x1b[2Jone\rtwo"), "onetwo");
        assert!(log_tail_preview("").is_empty());
    }

    #[test]
    fn abbreviated_run_labels_are_visibly_shortened_and_unicode_safe() {
        assert_eq!(short_run_id("76c65676cf620d37"), "76c65676…");
        assert_eq!(short_run_id("run-1"), "run-1");
        assert_eq!(short_run_id("ééééééééé"), "éééééééé…");
    }

    #[test]
    fn duration_columns_align_jobs_and_nest_steps() {
        let workflow = row("success", "ci.yaml", 0);
        let job = timed_row("failure", "test", 1, Some("1s"));
        let workflow = console::strip_ansi_codes(&workflow);
        let job = console::strip_ansi_codes(&job);
        assert_eq!(job.find("failure"), Some(9));
        assert!(workflow.contains("success ci.yaml"));
        let step = timed_row("success", "install", 2, Some("1s"));
        assert_eq!(console::strip_ansi_codes(&step).find("success"), Some(11));
        assert!(workflow.starts_with("success"));
        let short = timed_row("success", "short", 1, Some("1s"));
        let long = timed_row("success", "long", 1, Some("2m 03s"));
        let short = console::strip_ansi_codes(&short);
        let long = console::strip_ansi_codes(&long);
        assert_eq!(short.find("short"), long.find("long"));
        assert_eq!(
            short.find("1s").map(|p| p + 2),
            long.find("2m 03s").map(|p| p + 6)
        );
    }

    #[test]
    fn elapsed_time_requires_ordered_signed_timestamps() {
        assert_eq!(elapsed_to_report(Some(100), 125), Some(25_000));
        assert_eq!(elapsed_to_report(None, 125), None);
        assert_eq!(elapsed_to_report(Some(126), 125), None);
    }

    #[test]
    fn summary_highlights_failure_without_dumping_logs_or_successful_steps() {
        let mut details = JobDetails::default();
        for (id, outcome) in [("install", "success"), ("publish", "failure")] {
            details.steps.push(Step {
                id: id.into(),
                label: id.into(),
                outcome: outcome.into(),
                duration_ms: Some(9500),
                start_ms: None,
                artifacts: None,
            });
        }
        let summary = step_summary(&details, false);
        assert!(summary.contains("failed at publish"));
        assert!(!summary.contains("9.5s"));
        assert!(!summary.contains("install"));
    }

    #[test]
    fn planned_skips_do_not_count_as_completed_progress() {
        let details = JobDetails {
            steps: vec![Step {
                id: "conditional".into(),
                label: String::new(),
                outcome: "skipped".into(),
                duration_ms: None,
                start_ms: None,
                artifacts: None,
            }],
            ..JobDetails::default()
        };
        assert!(step_summary(&details, true).is_empty());
    }
}
