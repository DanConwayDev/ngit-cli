//! Human CI summaries. Structured evidence stays in the projection's JSON.
use std::fmt::Write as _;

use console::{StyledObject, style};
use ngit::ci::trust::TrustClassification;
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
    run_metadata_at(run, Timestamp::now())
}

pub(crate) fn run_metadata_at(run: &RunReport, now: Timestamp) -> String {
    let summary = crate::ci_history::AttemptSummary::new(&run.run, now);
    let mut metadata = format!(
        "run {} · {}",
        short_run_id(&run.run.run_id),
        crate::ci_history::age(summary.timestamp, now.as_secs())
    );
    let started = run
        .run
        .result
        .as_ref()
        .and_then(|r| r.started_at)
        .or_else(|| run.run.progress.as_ref().and_then(|p| p.started_at));
    let end =
        run.run.result.as_ref().map(|r| r.created_at).or_else(|| {
            matches!(run.state_label().as_str(), "running" | "in_progress").then_some(now)
        });
    if let Some(ms) = started
        .zip(end)
        .and_then(|(start, end)| elapsed_to_report(Some(start.as_secs()), end.as_secs()))
    {
        let seconds = ms / 1000;
        let duration = if seconds >= 3600 {
            format!(
                "{}h {:02}m {:02}s",
                seconds / 3600,
                seconds % 3600 / 60,
                seconds % 60
            )
        } else {
            crate::ci_plan::duration(ms)
        };
        if run.run.result.is_some() {
            let _ = write!(metadata, " · {duration}");
        } else {
            metadata = format!("run {} · {duration}", short_run_id(&run.run.run_id));
        }
    }
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

/// Lower bound from signed job-relative progress and time observed running.
/// Allocation identity separates provider retries.
pub fn live_job_row(
    run: &RunReport,
    id: &str,
    clocks: &mut crate::ci_plan::LiveStepClocks,
    now: Timestamp,
) -> String {
    let job = run
        .run
        .live_jobs
        .iter()
        .find(|job| job.job_id == id && job.is_live(now));
    if let Some(start) = job.and_then(|job| job.started_at) {
        let duration = crate::ci_plan::duration(
            now.as_secs()
                .saturating_sub(start.as_secs())
                .saturating_mul(1000),
        );
        return timed_row("running", id, 1, Some(&duration));
    }
    let observed = job.map(|job| job.created_at).or_else(|| {
        run.run
            .progress
            .as_ref()
            .filter(|p| p.in_progress_jobs.iter().any(|name| name == id))
            .map(|p| p.created_at)
    });
    let elapsed = observed.map(|observed| {
        let key = format!(
            "job:{}:{}:{}:{:?}",
            run.run.coordinator,
            run.run.run_id,
            id,
            job.map(|job| (job.author, &job.details.allocation))
        );
        // Offsets share the provider's job-relative timeline. Use the furthest
        // reported point, never sum durations (steps can overlap).
        let offset = job.map_or(0, |job| {
            job.details
                .steps
                .iter()
                .filter_map(|step| {
                    step.start_ms
                        .map(|start| start.saturating_add(step.duration_ms.unwrap_or(0)))
                })
                .max()
                .unwrap_or(0)
        });
        clocks.elapsed_with_offset(key, observed.as_secs(), now.as_secs(), offset)
    });
    timed_row(
        "running",
        id,
        1,
        elapsed
            .map(|ms| format!("≥{}", crate::ci_plan::duration(ms)))
            .as_deref(),
    )
}

pub fn print_status(report: &CiReport, target: &Target) {
    print!(
        "{}",
        render_status(
            report,
            target,
            &mut crate::ci_plan::LiveStepClocks::default()
        )
    );
}

pub fn render_status(
    report: &CiReport,
    target: &Target,
    clocks: &mut crate::ci_plan::LiveStepClocks,
) -> String {
    let mut output = String::new();
    let heading = match target {
        Target::Commit { ids, .. } => format!(
            "CI for {} ({})",
            target.describe(),
            ids.first().map_or("unknown commit", String::as_str)
        ),
        Target::PullRequest { .. } => format!("CI for {}", target.describe()),
    };
    let _ = writeln!(output, "{}", style(heading).bold());
    if report.runs.is_empty() {
        let _ = writeln!(output, "  No CI evidence for this commit");
        return output;
    }
    for run in &report.runs {
        let _ = writeln!(
            output,
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
            let _ = writeln!(
                output,
                "    {} · coordinator {}",
                crate::ci_history::context_label(run.run.activity_context()),
                run.run.coordinator
            );
        }
        for warning in warnings(run) {
            let _ = writeln!(output, "    {}", style(warning).yellow());
        }
        for job in &run.jobs {
            let _ = writeln!(
                output,
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
            let _ = writeln!(
                output,
                "{}{}",
                live_job_row(run, &job.job_id, clocks, now),
                crate::ci_plan::live_steps(
                    job,
                    run.plan.iter().find(|p| p.id == job.job_id),
                    clocks,
                    now
                )
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
                let _ = writeln!(output, "{}", live_job_row(run, id, clocks, now));
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
                let _ = writeln!(output, "{}", row("waiting", &job.label, 1));
            }
        }
    }
    if report.is_incomplete() {
        for gap in report.visible_coverage_gaps(ngit::output_mode::is_verbose()) {
            let _ = writeln!(output, "{}", style(gap.message()).dim());
        }
    }
    output.push_str(&inspection_tip(target));
    output
}

fn inspection_tip(target: &Target) -> String {
    let command = match target {
        Target::Commit { described, .. } if described == "HEAD" => "ngit ci inspect".to_owned(),
        Target::Commit { ids, .. } => format!(
            "ngit ci inspect --target {}",
            ids.first().map_or("HEAD", String::as_str)
        ),
        Target::PullRequest { root, .. } => format!("ngit ci inspect --target {root}"),
    };
    // PR targets need their exact ID, not the checked-out branch's HEAD.
    format!(
        "\n{} {} {}\n",
        style("Tip:").dim(),
        style(format!("`{command}`")).cyan().bold(),
        style("for details").dim()
    )
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

#[cfg(test)]
mod tests {
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
}
