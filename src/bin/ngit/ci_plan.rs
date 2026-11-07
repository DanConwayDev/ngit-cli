//! A verified declaration of expected jobs, never a local scheduler.
use std::{collections::HashSet, fmt::Write as _, path::Path};

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
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepPlan {
    pub id: String,
    pub label: String,
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
                        })
                    }
                })
                .collect();
            JobPlan {
                id,
                label,
                steps,
                dynamic,
            }
        })
        .collect()
}

pub fn steps(details: &JobDetails, plan: Option<&JobPlan>, live: bool) -> String {
    let mut output = String::new();
    let mut shown = HashSet::new();
    if let Some(plan) = plan {
        for expected in &plan.steps {
            if let Some(step) = details.steps.iter().find(|s| s.id == expected.id) {
                write_step(&mut output, step, details, live);
                shown.insert(step.id.as_str());
            } else {
                let _ = write!(output, "\n      {}: not reported", expected.label);
            }
        }
    }
    for step in &details.steps {
        if !shown.contains(step.id.as_str()) {
            write_step(&mut output, step, details, live);
        }
    }
    output
}

fn write_step(
    output: &mut String,
    step: &ngit::ci::details::Step,
    details: &JobDetails,
    live: bool,
) {
    let duration = step
        .duration_ms
        .map_or(String::new(), |ms| format!(" · {}", duration(ms)));
    let state = if live
        && step.outcome == "skipped"
        && step.start_ms.is_none()
        && step.duration_ms.is_none()
    {
        "skipped/planned (not timed)"
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
    let _ = write!(output, "\n      {label}: {state}{duration}");
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
            "\n      {} direct artifacts, {} manifests; use ci inspect for all references",
            details.artifacts.len(),
            details.artifact_manifests.len()
        );
    }
    output
}

pub fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{:01}s", ms / 1000, ms % 1000 / 100)
    } else {
        format!("{}m {}s", ms / 60_000, ms / 1000 % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(rendered.contains("1.2s"));
        assert!(rendered.contains("1000 artifacts"));
        assert!(rendered.len() < 1000);
    }
}
