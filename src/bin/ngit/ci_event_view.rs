//! Event-oriented inspection and streaming. Tag order, multiplicity and unknown
//! extensions survive decoding; this is not a current-status reduction.
use nostr::prelude::Event;
use serde_json::{Value, json};

pub fn event_type(kind: u16) -> &'static str {
    match kind {
        9840 => "manual_trigger",
        9841 => "job_result",
        9842 => "workflow_result",
        9843 => "service_request",
        9844 => "service_stop",
        9845 => "job_allocation",
        9846 => "workflow_cancellation",
        9847 => "allocation_cancellation",
        9848 => "forced_workflow_cancellation",
        9849 => "forced_allocation_cancellation",
        19843 => "coordinator_advertisement",
        19844 => "coordinator_readiness",
        29846 => "secret_update",
        39842 => "workflow_progress",
        39844 => "repository_service_status",
        39845 => "provider_advertisement",
        39846 => "job_progress",
        _ => "unknown",
    }
}

fn tag_name(kind: u16, tag: &[String]) -> &str {
    match tag[0].as_str() {
        "a" if tag.get(1).is_some_and(|s| s.starts_with("30063:")) => "release_coordinate",
        "a" => "repository",
        "c" => "commit_or_tag_object",
        "w" => "workflow",
        "j" => "job",
        "o" => "trigger",
        "r" if tag.get(1).is_some_and(|s| s.starts_with("refs/")) => "git_ref",
        "r" => "run_id",
        "d" if kind == 39846 => "allocation_id",
        "d" if kind == 39842 => "run_id",
        "d" => "replacement_identifier",
        "U" => "rerun_source",
        "D" => "dependency",
        "q" => "quoted_reference",
        "E" => "pull_request_root",
        "e" => "event_reference",
        "K" => "root_kind",
        "k" => "referenced_kind",
        "P" => "root_author",
        "p" => "addressed_or_referenced_author",
        "x" => "environment",
        "f" => "resource_requirement",
        "M" => "admission_policy",
        "X" => "manual_trigger_policy",
        "B" => "bootstrap_policy",
        "W" => "workflow_policy",
        "R" => "runner_policy",
        name => name,
    }
}

/// Every value after the tag name is retained, including empty placeholders and
/// future extension slots. Consumers need not reverse a lossy typed projection.
pub fn decoded(event: &Event) -> Value {
    json!({
        "id": event.id, "kind": event.kind.as_u16(),
        "type": event_type(event.kind.as_u16()), "author": event.pubkey,
        "created_at": event.created_at.as_secs(), "content": event.content,
        "tags": event.tags.iter().map(|t| {
            let t = t.as_slice();
            json!({"tag": t[0], "name": tag_name(event.kind.as_u16(), t), "values": t[1..]})
        }).collect::<Vec<_>>()
    })
}

pub fn value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let t = t.as_slice();
        (t[0] == name)
            .then(|| t.get(1).map(String::as_str))
            .flatten()
    })
}

/// One line per observed event, not per artifact or cumulative step entry.
pub fn summary(event: &Event) -> String {
    let steps: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "step")
        .collect();
    let summaries = steps
        .iter()
        .take(8)
        .map(|t| {
            let t = t.as_slice();
            let timing = t
                .get(4)
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(String::new(), |ms| {
                    format!(" {}", crate::ci_plan::duration(ms))
                });
            let outcome = t.get(5).map_or("unknown", String::as_str);
            let state = if event.kind.as_u16() == 39846
                && outcome == "skipped"
                && t.get(3).is_none_or(String::is_empty)
                && t.get(4).is_none_or(String::is_empty)
            {
                "skipped/planned"
            } else {
                outcome
            };
            format!(
                "{}={state}{timing}",
                t.get(1).map_or("step", String::as_str)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let direct = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "artifact")
        .count();
    let manifests: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "artifacts")
        .collect();
    let declared = manifests
        .iter()
        .filter_map(|t| t.as_slice().get(3)?.parse::<u64>().ok())
        .fold(direct as u64, u64::saturating_add);
    format!(
        "{}{} · {}{}{} · {declared} artifacts referenced, {} manifests · {}",
        value(event, "w").unwrap_or("CI"),
        value(event, "job").map_or(String::new(), |j| format!(" / {j}")),
        value(event, "conclusion")
            .or_else(|| value(event, "status"))
            .unwrap_or(event_type(event.kind.as_u16())),
        if summaries.is_empty() {
            String::new()
        } else {
            format!(" · {summaries}")
        },
        if steps.len() > 8 {
            format!(" (+{} steps; inspect for details)", steps.len() - 8)
        } else {
            String::new()
        },
        manifests.len(),
        crate::output::event_id_to_nevent(event.id, None)
    )
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Kind, Tag, event::FinalizeEvent};

    use super::*;

    #[test]
    fn decoded_tags_round_trip_extensions_and_duplicates_without_signature() {
        let event = EventBuilder::new(Kind::Custom(39846), "content")
            .tags([
                Tag::custom("q", ["first", "", "extension"]),
                Tag::custom("q", ["second"]),
                Tag::custom("future", ["", "value"]),
            ])
            .finalize(&Keys::generate())
            .unwrap();
        let decoded = decoded(&event);
        let restored: Vec<Vec<String>> = decoded["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let mut values = vec![t["tag"].as_str().unwrap().to_owned()];
                values.extend(
                    t["values"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned()),
                );
                values
            })
            .collect();
        assert_eq!(
            restored,
            event
                .tags
                .iter()
                .map(|t| t.as_slice().to_vec())
                .collect::<Vec<_>>()
        );
        assert!(decoded.get("sig").is_none());
        assert_eq!(decoded["content"], "content");
    }
}
