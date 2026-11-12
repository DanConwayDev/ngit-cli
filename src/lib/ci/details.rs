//! Typed NIP-C1 execution evidence and manual selection options.
//! References remain signed claims; parsing never downloads or executes them.
use std::collections::{BTreeMap, HashSet};

use nostr::prelude::{Coordinate, Event, EventId, Kind, PublicKey};
use serde::Serialize;

use super::kinds::{Conclusion, ShapeReason};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Restart {
    pub result: EventId,
    pub job: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependency {
    pub reference: String,
    pub relay: Option<String>,
    pub coordinator: PublicKey,
    pub alias: String,
    pub run_id: String,
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobProgressReference {
    pub provider: PublicKey,
    pub allocation: EventId,
    pub job: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct WorkflowDetails {
    pub job_progress: Vec<JobProgressReference>,
    pub restart: Option<Restart>,
    pub dependencies: Vec<Dependency>,
    pub dependency_failures: Vec<String>,
    pub superseded_by: Option<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub id: String,
    pub label: String,
    pub start_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub outcome: String,
    pub artifacts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Artifact {
    pub url: String,
    pub filename: String,
    pub name: String,
    pub step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactManifest {
    pub manifest: String,
    pub archive: Option<String>,
    pub count: Option<u64>,
    pub package: Option<String>,
    pub step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct JobDetails {
    pub allocation: Option<EventId>,
    pub environment: Option<Vec<String>>,
    pub resources: Vec<String>,
    pub steps: Vec<Step>,
    pub artifacts: Vec<Artifact>,
    pub artifact_manifests: Vec<ArtifactManifest>,
    pub outputs: BTreeMap<String, String>,
    pub omitted_outputs: BTreeMap<String, String>,
    pub execution: BTreeMap<String, Vec<String>>,
    pub skip_reason: Option<String>,
    pub superseded_by: Option<EventId>,
    pub exit_code: Option<i64>,
    pub runs_on: Vec<String>,
}

fn invalid(tag: &'static str) -> ShapeReason {
    ShapeReason::InvalidTagValue {
        tag,
        value: "invalid NIP-C1 field".into(),
    }
}
fn tags<'a>(event: &'a Event, name: &str) -> Vec<&'a [String]> {
    event
        .tags
        .iter()
        .map(|t| t.as_slice())
        .filter(|t| t[0] == name)
        .collect()
}
fn one<'a>(event: &'a Event, name: &'static str) -> Result<Option<&'a [String]>, ShapeReason> {
    let all = tags(event, name);
    if all.len() > 1 {
        return Err(ShapeReason::RepeatedTag(name));
    }
    Ok(all.first().copied())
}
fn optional(value: Option<&String>) -> Option<String> {
    value.filter(|s| !s.is_empty()).cloned()
}
fn id(raw: &str, tag: &'static str) -> Result<EventId, ShapeReason> {
    EventId::from_hex(raw).map_err(|_| invalid(tag))
}
fn optional_id(event: &Event, name: &'static str) -> Result<Option<EventId>, ShapeReason> {
    one(event, name)?
        .map(|t| {
            if t.len() != 2 {
                return Err(invalid(name));
            }
            id(&t[1], name)
        })
        .transpose()
}

/// Parse the source of a whole-workflow or selected-job restart.
pub fn restart(event: &Event) -> Result<Option<Restart>, ShapeReason> {
    one(event, "U")?
        .map(|t| {
            if !(t.len() == 2 || t.len() == 3 && !t[2].is_empty()) {
                return Err(invalid("U"));
            }
            Ok(Restart {
                result: id(&t[1], "U")?,
                job: t.get(2).cloned(),
            })
        })
        .transpose()
}

/// Parse aggregate references without substituting newer dependency attempts.
pub fn workflow(event: &Event) -> Result<WorkflowDetails, ShapeReason> {
    let mut result = WorkflowDetails {
        restart: restart(event)?,
        superseded_by: optional_id(event, "superseded-by")?,
        ..Default::default()
    };
    for t in tags(event, "q") {
        if t.get(1).is_some_and(|v| v.starts_with("39846:")) {
            if t.len() != 5 || t[4].is_empty() {
                return Err(invalid("q"));
            }
            let address = Coordinate::from_kpi_format(&t[1]).map_err(|_| invalid("q"))?;
            if PublicKey::from_hex(&t[3]).map_err(|_| invalid("q"))? != address.public_key {
                return Err(invalid("q"));
            }
            result.job_progress.push(JobProgressReference {
                provider: address.public_key,
                allocation: id(&address.identifier, "q")?,
                job: t[4].clone(),
            });
        }
    }
    let mut aliases = HashSet::new();
    for t in tags(event, "D") {
        if t.len() < 5 || t[4].is_empty() || !aliases.insert(t[4].clone()) {
            return Err(invalid("D"));
        }
        let (coordinator, run_id, pending) = if let Ok(address) = Coordinate::from_kpi_format(&t[1])
        {
            if t.len() != 5
                || address.kind != Kind::Custom(39842)
                || address.identifier.is_empty()
                || !t[3].is_empty()
            {
                return Err(invalid("D"));
            }
            (address.public_key, address.identifier, true)
        } else {
            if t.len() != 6 || t[5].is_empty() {
                return Err(invalid("D"));
            }
            id(&t[1], "D")?;
            (
                PublicKey::from_hex(&t[3]).map_err(|_| invalid("D"))?,
                t[5].clone(),
                false,
            )
        };
        result.dependencies.push(Dependency {
            reference: t[1].clone(),
            relay: optional(t.get(2)),
            coordinator,
            run_id,
            alias: t[4].clone(),
            pending,
        });
    }
    for t in tags(event, "dependency-failure") {
        if t.len() != 2 || t[1].is_empty() {
            return Err(invalid("dependency-failure"));
        }
        result.dependency_failures.push(t[1].clone());
    }
    Ok(result)
}

/// Check a manual resource predicate, returning its decoded path and operator.
pub fn requirement(raw: &str) -> Result<(String, String), ShapeReason> {
    let mut fields = raw.splitn(3, ':');
    let encoded = fields.next().unwrap_or_default();
    let operator = fields.next().ok_or_else(|| invalid("f"))?;
    let operand: serde_json::Value =
        serde_json::from_str(fields.next().ok_or_else(|| invalid("f"))?)
            .map_err(|_| invalid("f"))?;
    let mut path = String::new();
    let mut rest = encoded;
    while let Some(pos) = rest.find('%') {
        path.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if tail.starts_with("%25") {
            path.push('%');
        } else if tail.starts_with("%3A") {
            path.push(':');
        } else {
            return Err(invalid("f"));
        }
        rest = &tail[3..];
    }
    path.push_str(rest);
    if path.is_empty() {
        return Err(invalid("f"));
    }
    if let Some(pointer) = path.strip_prefix("env/") {
        if pointer.is_empty() {
            return Err(invalid("f"));
        }
        let bytes = pointer.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'~' && !matches!(bytes.get(i + 1), Some(b'0' | b'1')) {
                return Err(invalid("f"));
            }
        }
    }
    let integer = operand
        .as_i64()
        .is_some_and(|v| (-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&v));
    let scalar = integer || operand.is_string() || operand.is_boolean();
    if !match operator {
        "eq" | "contains" => scalar,
        "gte" | "lte" => integer,
        _ => false,
    } {
        return Err(invalid("f"));
    }
    Ok((path, operator.into()))
}

/// Validate job-level manual environment and resource overrides.
pub fn manual(event: &Event, workflow: &str) -> Result<(), ShapeReason> {
    if !tags(event, "rerun").is_empty() {
        return Err(ShapeReason::ForbiddenTag("rerun"));
    }
    let restart = restart(event)?;
    if (restart.is_some() || !tags(event, "x").is_empty() || !tags(event, "f").is_empty())
        && !workflow.starts_with(".ngit/nix/workflows/")
    {
        return Err(invalid("w"));
    }
    let mut jobs = HashSet::new();
    for t in tags(event, "x") {
        if t.len() != 4 || t[3].is_empty() || !jobs.insert(t[3].clone()) || !hash(&t[1]) {
            return Err(invalid("x"));
        }
        if !t[2].is_empty() {
            environment_url(&t[1], &t[2])?;
        }
    }
    let mut predicates = HashSet::new();
    for t in tags(event, "f") {
        if t.len() != 3 || t[2].is_empty() {
            return Err(invalid("f"));
        }
        let (path, op) = requirement(&t[1])?;
        if !predicates.insert((t[2].clone(), path, op)) {
            return Err(ShapeReason::RepeatedTag("f"));
        }
    }
    Ok(())
}

fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn blossom(raw: &str) -> Result<(), ShapeReason> {
    let url = nostr::prelude::Url::parse(raw).map_err(|_| invalid("artifact"))?;
    let filename = url
        .path_segments()
        .and_then(Iterator::last)
        .unwrap_or_default();
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !hash(filename.split('.').next().unwrap_or_default())
    {
        return Err(invalid("artifact"));
    }
    Ok(())
}
fn environment_url(hash: &str, raw: &str) -> Result<(), ShapeReason> {
    blossom(raw)?;
    let url = nostr::prelude::Url::parse(raw).map_err(|_| invalid("x"))?;
    if url.query().is_some()
        || url.fragment().is_some()
        || url
            .path_segments()
            .and_then(Iterator::last)
            .and_then(|s| s.split('.').next())
            != Some(hash)
    {
        return Err(invalid("x"));
    }
    Ok(())
}

fn timing(raw: &str) -> Result<Option<u64>, ShapeReason> {
    if raw.is_empty() {
        Ok(None)
    } else {
        raw.parse().map(Some).map_err(|_| invalid("step"))
    }
}

/// Parse signed execution summaries. Artifact URLs are references, not verified
/// downloads.
pub fn job(event: &Event, progress: bool) -> Result<JobDetails, ShapeReason> {
    let mut result = JobDetails {
        superseded_by: optional_id(event, "superseded-by")?,
        ..Default::default()
    };
    for t in tags(event, "q") {
        if t.get(4).is_some_and(|m| m == "job-allocation") {
            if t.len() != 5 || result.allocation.is_some() {
                return Err(invalid("q"));
            }
            result.allocation = Some(id(&t[1], "q")?);
            PublicKey::from_hex(&t[3]).map_err(|_| invalid("q"))?;
        }
    }
    if let Some(t) = one(event, "x")? {
        if t.len() != 3 || !hash(&t[1]) {
            return Err(invalid("x"));
        }
        if !t[2].is_empty() {
            environment_url(&t[1], &t[2])?;
        }
        result.environment = Some(t[1..].to_vec());
    }
    let mut step_ids = HashSet::new();
    for t in tags(event, "step") {
        if !(6..=7).contains(&t.len())
            || t[1].is_empty()
            || !step_ids.insert(t[1].clone())
            || (Conclusion::parse(&t[5]).is_none()
                && !matches!(t[5].as_str(), "cached" | "unknown")
                && !(progress && matches!(t[5].as_str(), "pending" | "running")))
        {
            return Err(invalid("step"));
        }
        if let Some(state) = t.get(6) {
            if !matches!(state.as_str(), "artifacts-ready" | "artifacts-failed")
                && !(progress && state == "artifacts-uploading")
            {
                return Err(invalid("step"));
            }
        }
        let start_ms = timing(&t[3])?;
        let duration_ms = timing(&t[4])?;
        if (t[5] == "pending" && (start_ms.is_some() || duration_ms.is_some() || t.len() == 7))
            || (t[5] == "running" && (start_ms.is_none() || duration_ms.is_some() || t.len() == 7))
            || start_ms
                .zip(duration_ms)
                .is_some_and(|(start, duration)| start.checked_add(duration).is_none())
        {
            return Err(invalid("step"));
        }
        result.steps.push(Step {
            id: t[1].clone(),
            label: t[2].clone(),
            start_ms,
            duration_ms,
            outcome: t[5].clone(),
            artifacts: t.get(6).cloned(),
        });
    }
    for t in tags(event, "artifact") {
        if !(4..=5).contains(&t.len()) || t[2].is_empty() || t[3].is_empty() {
            return Err(invalid("artifact"));
        }
        blossom(&t[1])?;
        result.artifacts.push(Artifact {
            url: t[1].clone(),
            filename: t[2].clone(),
            name: t[3].clone(),
            step: optional(t.get(4)),
        });
    }
    for t in tags(event, "artifacts") {
        if !(2..=6).contains(&t.len()) {
            return Err(invalid("artifacts"));
        }
        blossom(&t[1])?;
        let archive = optional(t.get(2));
        if let Some(url) = &archive {
            blossom(url)?;
        }
        let count = optional(t.get(3))
            .map(|v| {
                v.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0 && n.to_string() == v)
                    .ok_or_else(|| invalid("artifacts"))
            })
            .transpose()?;
        result.artifact_manifests.push(ArtifactManifest {
            manifest: t[1].clone(),
            archive,
            count,
            package: optional(t.get(4)),
            step: optional(t.get(5)),
        });
    }
    let mut names = HashSet::new();
    let mut bytes = 0;
    for name in ["output", "output-omitted"] {
        for t in tags(event, name) {
            if t.len() != 3 || t[1].is_empty() || !names.insert(t[1].clone()) {
                return Err(invalid(name));
            }
            if name == "output" {
                bytes += t[2].len();
                if t[2].len() > 8192 || bytes > 65536 {
                    return Err(invalid(name));
                }
                result.outputs.insert(t[1].clone(), t[2].clone());
            } else {
                if progress || !matches!(t[2].as_str(), "missing" | "oversized" | "unresolved") {
                    return Err(invalid(name));
                }
                result.omitted_outputs.insert(t[1].clone(), t[2].clone());
            }
        }
    }
    for name in [
        "exec-termination",
        "exec-cancellation",
        "exec-stop",
        "exec-logs",
        "exec-artifacts",
    ] {
        if let Some(t) = one(event, name)? {
            if progress
                || !(t.len() == 2
                    || name == "exec-stop" && t.len() == 3 && !t[2].is_empty() && t[2].len() <= 256)
            {
                return Err(invalid(name));
            }
            let valid = match name {
                "exec-termination" => matches!(
                    t[1].as_str(),
                    "not_started" | "grace_expired" | "forced" | "timed_out"
                ),
                "exec-cancellation" => matches!(t[1].as_str(), "ordinary" | "forced"),
                "exec-stop" => matches!(t[1].as_str(), "checkpoint" | "stopped" | "rolled_back"),
                _ => matches!(t[1].as_str(), "partial" | "unavailable"),
            };
            if !valid {
                return Err(invalid(name));
            }
            result
                .execution
                .insert(name.trim_start_matches("exec-").into(), t[1..].to_vec());
        }
    }
    let termination = result.execution.get("termination").map(|v| v[0].as_str());
    let cancellation = result.execution.get("cancellation").map(|v| v[0].as_str());
    if ((result.execution.contains_key("stop") || termination == Some("grace_expired"))
        && cancellation.is_none())
        || (termination == Some("forced") && cancellation != Some("forced"))
    {
        return Err(invalid("exec-cancellation"));
    }
    if let Some(t) = one(event, "skip-reason")? {
        if t.len() != 2
            || t[1] != "condition"
            || result.allocation.is_some()
            || progress
            || !result.steps.is_empty()
            || one(event, "conclusion")?
                .and_then(|t| t.get(1))
                .map(String::as_str)
                != Some("skipped")
        {
            return Err(invalid("skip-reason"));
        }
        result.skip_reason = Some(t[1].clone());
    }
    if let Some(t) = one(event, "exit_code")? {
        if t.len() != 2 || progress {
            return Err(invalid("exit_code"));
        }
        result.exit_code = Some(t[1].parse().map_err(|_| invalid("exit_code"))?);
    }
    if let Some(t) = one(event, "runs_on")? {
        if t.len() < 2 || t[1..].iter().any(String::is_empty) {
            return Err(invalid("runs_on"));
        }
        result.runs_on = t[1..].to_vec();
    }
    let mut resources = HashSet::new();
    for t in tags(event, "f") {
        if t.len() != 2 {
            return Err(invalid("f"));
        }
        // Resource declarations permit homogeneous arrays in addition to scalars.
        let mut fields = t[1].splitn(3, ':');
        let key = fields.next().unwrap_or_default();
        if key.is_empty() || !resources.insert(key.to_owned()) || fields.next() != Some("eq") {
            return Err(invalid("f"));
        }
        let value: serde_json::Value =
            serde_json::from_str(fields.next().ok_or_else(|| invalid("f"))?)
                .map_err(|_| invalid("f"))?;
        let values = value.as_array().cloned().unwrap_or_else(|| vec![value]);
        let mut kind = None;
        for v in values {
            requirement(&format!("{key}:eq:{v}"))?;
            let current = if v.is_string() {
                0
            } else if v.is_boolean() {
                1
            } else {
                2
            };
            if kind.is_some_and(|k| k != current) {
                return Err(invalid("f"));
            }
            kind = Some(current);
        }
        result.resources.push(t[1].clone());
    }
    if result.skip_reason.is_some()
        && (!result.artifacts.is_empty()
            || !result.artifact_manifests.is_empty()
            || !result.outputs.is_empty()
            || !result.execution.is_empty()
            || result.environment.is_some()
            || !result.resources.is_empty()
            || result.exit_code.is_some()
            || one(event, "started_at")?.is_some())
    {
        return Err(invalid("skip-reason"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use nostr::prelude::Keys;

    use super::*;
    use crate::ci::kinds::test_events::{event, tag};

    #[test]
    fn explicit_step_states_validate_progress_and_preserve_legacy_outcomes() {
        let keys = Keys::generate();
        let parse = |start, duration, outcome, progress| {
            job(
                &event(
                    &keys,
                    Kind::Custom(39846),
                    100,
                    vec![tag(&["step", "test", "Test", start, duration, outcome])],
                ),
                progress,
            )
        };
        assert!(parse("", "", "pending", true).is_ok());
        assert!(parse("1234", "", "running", true).is_ok());
        for (start, duration, state) in [
            ("0", "", "pending"),
            ("", "1", "pending"),
            ("", "", "running"),
            ("0", "1", "running"),
        ] {
            assert!(parse(start, duration, state, true).is_err());
        }
        assert!(parse("", "", "pending", false).is_err());
        assert!(parse("0", "", "running", false).is_err());
        for state in ["unknown", "skipped"] {
            assert!(parse("", "", state, true).is_ok());
            assert!(parse("", "", state, false).is_ok());
        }
        assert!(parse("18446744073709551615", "1", "success", true).is_err());
    }

    #[test]
    fn execution_evidence_preserves_empty_outputs_and_rejects_conflicts() {
        let keys = Keys::generate();
        let make = |tags| event(&keys, Kind::Custom(9841), 10, tags);
        let good = vec![
            tag(&["output", "empty", ""]),
            tag(&["output-omitted", "missing", "missing"]),
            tag(&[
                "step",
                "build",
                "Build",
                "",
                "12",
                "success",
                "artifacts-ready",
            ]),
            tag(&["exec-cancellation", "forced"]),
            tag(&["exec-termination", "forced"]),
        ];
        let details = job(&make(good.clone()), false).unwrap();
        assert_eq!(details.outputs["empty"], "");
        assert_eq!(details.omitted_outputs["missing"], "missing");
        assert_eq!(details.steps[0].start_ms, None);
        let mut bad = good.clone();
        bad.push(tag(&["output-omitted", "empty", "missing"]));
        assert!(job(&make(bad), false).is_err());
        assert!(job(&make(good), true).is_err());
        assert!(job(&make(vec![tag(&["exec-termination", "forced"])]), false).is_err());
    }

    #[test]
    fn restart_and_selection_validate_exact_shapes() {
        let keys = Keys::generate();
        let make = |tags| event(&keys, Kind::Custom(9840), 10, tags);
        let good = vec![
            tag(&["U", &"ab".repeat(32), "build"]),
            tag(&["x", &"cd".repeat(32), "", "build"]),
            tag(&["f", "memory-mib:gte:2048", "build"]),
        ];
        let evt = make(good.clone());
        manual(&evt, ".ngit/nix/workflows/ci.yaml").unwrap();
        assert_eq!(
            restart(&evt).unwrap().unwrap().job.as_deref(),
            Some("build")
        );
        assert!(manual(&evt, ".ngit/act/workflows/ci.yaml").is_err());
        let mut bad = good;
        bad.push(tag(&["f", "memory-mib:gte:4096", "build"]));
        assert!(manual(&make(bad), ".ngit/nix/workflows/ci.yaml").is_err());
        for expression in [
            "env/a~2b:eq:1",
            "cpu:gte:1.5",
            "cpu:eq:null",
            "cpu:gte:9007199254740992",
            "a%20:eq:1",
        ] {
            assert!(requirement(expression).is_err(), "{expression}");
        }
        assert_eq!(requirement("a%3Ab:eq:\"v:1\"").unwrap().0, "a:b");
    }

    #[test]
    fn dependency_bindings_preserve_exact_attempts() {
        let keys = Keys::generate();
        let pending = format!("39842:{}:run-a", keys.public_key());
        let evt = event(
            &keys,
            Kind::Custom(9842),
            10,
            vec![
                tag(&["D", &pending, "", "", "build"]),
                tag(&[
                    "D",
                    &"ab".repeat(32),
                    "",
                    &keys.public_key().to_hex(),
                    "test",
                    "run-b",
                ]),
            ],
        );
        let details = workflow(&evt).unwrap();
        assert!(details.dependencies[0].pending);
        assert_eq!(details.dependencies[1].run_id, "run-b");
    }
}

#[cfg(test)]
mod binding_tests {
    use nostr::prelude::Keys;

    use super::*;
    use crate::ci::kinds::test_events::{event, tag};

    #[test]
    fn environment_hints_must_name_the_pinned_digest_without_credentials() {
        let keys = Keys::generate();
        let digest = "ab".repeat(32);
        let make = |url: &str| {
            event(
                &keys,
                Kind::Custom(9840),
                100,
                vec![tag(&["x", &digest, url, "build"])],
            )
        };
        manual(
            &make(&format!("https://blossom.example/{digest}.json")),
            ".ngit/nix/workflows/ci.yaml",
        )
        .unwrap();
        for url in [
            format!("https://blossom.example/{}.json", "cd".repeat(32)),
            format!("https://user:password@blossom.example/{digest}"),
            format!("https://blossom.example/{digest}?token=value"),
        ] {
            assert!(manual(&make(&url), ".ngit/nix/workflows/ci.yaml").is_err());
        }
    }

    #[test]
    fn conditional_skip_cannot_also_claim_execution() {
        let keys = Keys::generate();
        let mut tags = vec![
            tag(&["skip-reason", "condition"]),
            tag(&["conclusion", "skipped"]),
        ];
        job(&event(&keys, Kind::Custom(9841), 100, tags.clone()), false).unwrap();
        tags.push(tag(&["started_at", "90"]));
        assert!(job(&event(&keys, Kind::Custom(9841), 100, tags), false).is_err());
    }
}
