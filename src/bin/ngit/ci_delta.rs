//! Stream-local identities and semantic changes, separate from signed events.
use std::collections::{BTreeMap, HashMap, HashSet};

use ngit::ci::kinds::{
    WorkflowRunAddress, validate_job_progress, validate_job_result, validate_workflow_progress,
    validate_workflow_result,
};
use nostr::prelude::{Event, EventId, PublicKey, Tag};
use serde_json::{Value, json};

use crate::ci_event_view::{decoded, decoded_tag};

#[derive(Clone, Hash, PartialEq, Eq)]
struct Identity {
    run: WorkflowRunAddress,
    job: Option<(String, PublicKey, Option<EventId>)>,
}

impl Identity {
    fn parse(event: &Event) -> Option<Self> {
        let run = match event.kind.as_u16() {
            39842 => {
                let progress = validate_workflow_progress(event).ok()?;
                WorkflowRunAddress {
                    coordinator: event.pubkey,
                    run_id: progress.run_id,
                }
            }
            9842 => {
                let result = validate_workflow_result(event).ok()?;
                WorkflowRunAddress {
                    coordinator: event.pubkey,
                    run_id: result.run_id,
                }
            }
            39846 => {
                let progress = validate_job_progress(event).ok()?;
                return Some(Self {
                    run: progress.run,
                    job: Some((progress.job_id, event.pubkey, progress.details.allocation)),
                });
            }
            9841 => {
                let result = validate_job_result(event).ok()?;
                return Some(Self {
                    run: result.run,
                    job: Some((result.job_id, event.pubkey, result.details.allocation)),
                });
            }
            _ => return None,
        };
        Some(Self { run, job: None })
    }
}

struct Baseline {
    reference: String,
    event: Event,
}

#[derive(Default)]
pub struct Stream {
    runs: HashMap<WorkflowRunAddress, String>,
    entities: HashMap<Identity, Baseline>,
    references: PrefixReferences,
}

impl Stream {
    /// Call only after verifying the event signature and target filters.
    pub fn update(&mut self, event: &Event) -> Option<Value> {
        let Some(identity) = Identity::parse(event) else {
            // Unknown/malformed shapes must never be guessed into another run.
            return Some(
                json!({"type": "event", "event": decoded(event), "signature_verified": true}),
            );
        };
        if let Some(previous) = self.entities.get_mut(&identity) {
            if !supersedes(event, &previous.event) {
                return None;
            }
            let changes = changes(&previous.event, event);
            previous.event = event.clone();
            return (!changes.as_object()?.is_empty()).then(|| {
                json!({
                    "type": "update", "ref": previous.reference,
                    "timestamp": event.created_at.as_secs(), "changes": changes,
                })
            });
        }
        let run_ref = self
            .runs
            .entry(identity.run.clone())
            .or_insert_with(|| self.references.allocate("", &identity.run.run_id))
            .clone();
        let reference = if let Some((_, _, allocation)) = &identity.job {
            let id = allocation.unwrap_or(event.id).to_hex();
            self.references.allocate(&format!("{run_ref}/"), &id)
        } else {
            run_ref.clone()
        };
        let mut snapshot = json!({
            "type": "snapshot", "ref": reference, "run_ref": run_ref,
            "entity": if identity.job.is_some() { "job" } else { "workflow" },
            "coordinator": identity.run.coordinator.to_hex(), "run_id": identity.run.run_id,
            "workflow": event.tags.iter().map(Tag::as_slice).find(|tag| tag[0] == "w").and_then(|tag| tag.get(1)),
            "timestamp": event.created_at.as_secs(), "event_id": event.id.to_hex(),
            "kind": event.kind.as_u16(), "signature_verified": true,
            "tags": tags(event).iter().map(|t| decoded_tag(event.kind.as_u16(), t)).collect::<Vec<_>>(),
            "steps": steps(event).into_iter().map(|(_, step)| step).collect::<Vec<_>>(),
            "content": event.content,
        });
        if let Some((job, provider, allocation)) = &identity.job {
            snapshot["job"] = json!(job);
            snapshot["provider"] = json!(provider.to_hex());
            snapshot["allocation_id"] = json!(allocation.map(|id| id.to_hex()));
        }
        self.entities.insert(
            identity,
            Baseline {
                reference,
                event: event.clone(),
            },
        );
        Some(snapshot)
    }
}

/// Labels stay fixed after publication. A newly colliding ID gets a longer
/// prefix; an identical ID in another identity scope gets a numeric suffix.
#[derive(Default)]
struct PrefixReferences {
    used: HashSet<String>,
    sources: HashSet<(String, String)>,
}
impl PrefixReferences {
    fn allocate(&mut self, parent: &str, id: &str) -> String {
        let chars: Vec<_> = id.chars().collect();
        let shortest = chars.len().min(8);
        if self.sources.insert((parent.into(), id.into())) {
            for length in shortest..=chars.len() {
                let reference = format!("{parent}{}", chars[..length].iter().collect::<String>());
                if self.used.insert(reference.clone()) {
                    return reference;
                }
            }
        }
        let prefix: String = chars[..shortest].iter().collect();
        for suffix in 2_u64.. {
            let reference = format!("{parent}{prefix}-{suffix}");
            if self.used.insert(reference.clone()) {
                return reference;
            }
        }
        unreachable!("reference suffix space exhausted")
    }
}

fn terminal(event: &Event) -> bool {
    matches!(event.kind.as_u16(), 9841 | 9842)
}

fn supersedes(event: &Event, previous: &Event) -> bool {
    if terminal(event) != terminal(previous) {
        return terminal(event);
    }
    ngit::ci::total_order_position(event.created_at, event.id)
        > ngit::ci::total_order_position(previous.created_at, previous.id)
}

fn tags(event: &Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|tag| {
            tag[0] != "expiration"
                && !(tag[0] == "step" && matches!(event.kind.as_u16(), 9841 | 39846))
        })
        .map(<[String]>::to_vec)
        .collect()
}

/// A multiset difference: each removal/addition represents one occurrence.
fn difference(left: &[Vec<String>], right: &[Vec<String>], kind: u16) -> Vec<Value> {
    let mut counts = BTreeMap::<&Vec<String>, usize>::new();
    for tag in right {
        *counts.entry(tag).or_default() += 1;
    }
    left.iter()
        .filter_map(|tag| {
            if let Some(count) = counts.get_mut(tag) {
                if *count > 0 {
                    *count -= 1;
                    return None;
                }
            }
            Some(decoded_tag(kind, tag))
        })
        .collect()
}

fn steps(event: &Event) -> Vec<(String, Value)> {
    if !matches!(event.kind.as_u16(), 9841 | 39846) {
        return Vec::new();
    }
    event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|tag| tag[0] == "step")
        .map(|tag| {
            let running = event.kind.as_u16() == 39846 && tag[5] == "unknown" && tag[4].is_empty();
            let mut step =
                json!({"id": tag[1], "status": if running { "running" } else { &tag[5] }});
            if running {
                step["outcome"] = json!("unknown");
            }
            if !tag[2].is_empty() && tag[2] != tag[1] {
                step["label"] = json!(tag[2]);
            }
            for (name, index) in [("start_ms", 3), ("duration_ms", 4)] {
                if let Ok(value) = tag[index].parse::<u64>() {
                    step[name] = json!(value);
                }
            }
            if tag.len() > 6 {
                step["extra"] = json!(&tag[6..]);
            }
            (tag[1].clone(), step)
        })
        .collect()
}

fn changes(previous: &Event, event: &Event) -> Value {
    let mut result = json!({});
    if previous.kind != event.kind {
        result["kind"] = json!(event.kind.as_u16());
    }
    if previous.content != event.content {
        result["content"] = json!(event.content);
    }
    let old_tags = tags(previous);
    let new_tags = tags(event);
    for (key, values) in [
        (
            "tags_added",
            difference(&new_tags, &old_tags, event.kind.as_u16()),
        ),
        (
            "tags_removed",
            difference(&old_tags, &new_tags, previous.kind.as_u16()),
        ),
    ] {
        if !values.is_empty() {
            result[key] = json!(values);
        }
    }
    let old_steps = steps(previous);
    let new_steps = steps(event);
    let updated = new_steps
        .iter()
        .filter(|(id, step)| {
            !old_steps
                .iter()
                .any(|(old_id, old_step)| id == old_id && step == old_step)
        })
        .map(|(_, step)| step)
        .collect::<Vec<_>>();
    let removed = old_steps
        .iter()
        .filter(|(id, _)| !new_steps.iter().any(|(new_id, _)| id == new_id))
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    if !updated.is_empty() {
        result["steps"] = json!(updated);
    }
    if !removed.is_empty() {
        result["removed_steps"] = json!(removed);
    }
    let old_order = old_steps.iter().map(|(id, _)| id).collect::<Vec<_>>();
    let new_order = new_steps.iter().map(|(id, _)| id).collect::<Vec<_>>();
    if old_order != new_order {
        result["step_order"] = json!(new_order);
    }
    result
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Kind, Timestamp, event::FinalizeEvent};

    use super::*;

    fn common(keys: &Keys) -> Vec<Tag> {
        vec![
            Tag::custom("a", [format!("30617:{}:repo", keys.public_key())]),
            Tag::custom("c", ["ab".repeat(20)]),
            Tag::custom("w", ["ci.yml".into(), "cd".repeat(32)]),
            Tag::custom("o", ["push"]),
            Tag::custom("r", ["refs/heads/main"]),
        ]
    }
    fn event(keys: &Keys, kind: u16, time: u64, tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::Custom(kind), "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(time))
            .finalize(keys)
            .unwrap()
    }
    fn workflow(keys: &Keys, run: &str, kind: u16, time: u64, extra: Vec<Tag>) -> Event {
        let mut tags = common(keys);
        if kind == 39842 {
            tags.extend([
                Tag::identifier(run),
                Tag::custom("status", ["in_progress"]),
                Tag::custom("expiration", [(time + 100).to_string()]),
            ]);
        } else {
            tags.extend([
                Tag::custom("r", [run]),
                Tag::custom("conclusion", ["success"]),
            ]);
        }
        tags.extend(extra);
        let event = event(keys, kind, time, tags);
        assert!(Identity::parse(&event).is_some());
        event
    }
    fn job(
        keys: &Keys,
        run: &str,
        allocation: &str,
        kind: u16,
        time: u64,
        extra: Vec<Tag>,
    ) -> Event {
        let mut tags = common(keys);
        tags.extend([
            Tag::custom(
                "q",
                [
                    format!("39842:{}:{run}", keys.public_key()),
                    "wss://relay.example".into(),
                ],
            ),
            Tag::custom(
                "q",
                [
                    allocation.into(),
                    "wss://relay.example".into(),
                    keys.public_key().to_hex(),
                    "job-allocation".into(),
                ],
            ),
            Tag::custom("job", ["rust"]),
        ]);
        if kind == 39846 {
            tags.extend([
                Tag::identifier(allocation),
                Tag::custom("expiration", [(time + 100).to_string()]),
            ]);
        } else {
            tags.push(Tag::custom("conclusion", ["success"]));
        }
        tags.extend(extra);
        let event = event(keys, kind, time, tags);
        assert!(Identity::parse(&event).is_some());
        event
    }
    fn step(id: &str, duration: &str, status: &str) -> Tag {
        Tag::custom("step", [id, id, "", duration, status])
    }

    #[test]
    fn aliases_isolate_runs_coordinators_and_allocations_even_when_jobs_arrive_first() {
        let keys = Keys::generate();
        let other = Keys::generate();
        let mut stream = Stream::default();
        let first = stream
            .update(&job(&keys, "one", &"aa".repeat(32), 39846, 10, vec![]))
            .unwrap();
        assert_eq!(first["ref"], "one/aaaaaaaa");
        assert_eq!(first["run_id"], "one");
        assert_eq!(
            stream
                .update(&workflow(&keys, "one", 39842, 10, vec![]))
                .unwrap()["ref"],
            "one"
        );
        assert_eq!(
            stream
                .update(&job(&keys, "one", &"bb".repeat(32), 39846, 10, vec![]))
                .unwrap()["ref"],
            "one/bbbbbbbb"
        );
        let same_allocation = job(&keys, "one", &"aa".repeat(32), 39846, 10, vec![]);
        let other_provider = event(
            &other,
            39846,
            10,
            same_allocation.tags.iter().cloned().collect(),
        );
        assert_eq!(
            stream.update(&other_provider).unwrap()["ref"],
            "one/aaaaaaaa-2"
        );
        assert_eq!(
            stream
                .update(&workflow(&keys, "two", 39842, 10, vec![]))
                .unwrap()["ref"],
            "two"
        );
        assert_eq!(
            stream
                .update(&workflow(&other, "one", 39842, 10, vec![]))
                .unwrap()["ref"],
            "one-2"
        );
    }

    #[test]
    fn step_updates_replace_whole_steps_and_explicitly_remove_missing_steps() {
        let keys = Keys::generate();
        let allocation = "aa".repeat(32);
        let mut stream = Stream::default();
        let initial = stream
            .update(&job(
                &keys,
                "one",
                &allocation,
                39846,
                10,
                vec![
                    step("compile", "1000", "success"),
                    step("tests", "", "unknown"),
                ],
            ))
            .unwrap();
        assert_eq!(initial["steps"][1]["status"], "running");
        let update = stream
            .update(&job(
                &keys,
                "one",
                &allocation,
                39846,
                11,
                vec![
                    step("compile", "1000", "success"),
                    step("tests", "2000", "success"),
                ],
            ))
            .unwrap();
        assert_eq!(update["timestamp"], 11);
        assert_eq!(update["changes"]["steps"].as_array().unwrap().len(), 1);
        assert_eq!(update["changes"]["steps"][0]["id"], "tests");
        assert!(update["changes"]["steps"][0].get("outcome").is_none());
        let removed = stream
            .update(&job(
                &keys,
                "one",
                &allocation,
                39846,
                12,
                vec![step("tests", "2000", "success")],
            ))
            .unwrap();
        assert_eq!(removed["changes"]["removed_steps"], json!(["compile"]));
        assert_eq!(removed["changes"]["step_order"], json!(["tests"]));
        let result = stream
            .update(&job(
                &keys,
                "one",
                &allocation,
                9841,
                13,
                vec![step("tests", "2000", "success")],
            ))
            .unwrap();
        assert_eq!(result["ref"], "one/aaaaaaaa");
        assert_eq!(result["changes"]["kind"], 9841);
        assert!(
            stream
                .update(&job(&keys, "one", &allocation, 39846, 14, vec![]))
                .is_none()
        );
    }

    #[test]
    fn renewals_and_late_events_are_suppressed_but_duplicate_tags_are_counted() {
        let keys = Keys::generate();
        let future = Tag::custom("future", ["", "extension"]);
        let mut stream = Stream::default();
        let first = workflow(
            &keys,
            "one",
            39842,
            10,
            vec![future.clone(), future.clone()],
        );
        stream.update(&first).unwrap();
        assert!(stream.update(&first).is_none());
        assert!(
            stream
                .update(&workflow(
                    &keys,
                    "one",
                    39842,
                    11,
                    vec![future.clone(), future.clone()]
                ))
                .is_none()
        );
        assert!(
            stream
                .update(&workflow(&keys, "one", 39842, 9, vec![]))
                .is_none()
        );
        let update = stream
            .update(&workflow(&keys, "one", 39842, 12, vec![future]))
            .unwrap();
        assert_eq!(
            update["changes"]["tags_removed"],
            json!([{"tag":"future", "name":"future", "values":["", "extension"]}])
        );
        // A malformed unknown extension named step on a workflow remains a tag.
        let update = stream
            .update(&workflow(
                &keys,
                "one",
                9842,
                13,
                vec![Tag::custom("step", ["future"])],
            ))
            .unwrap();
        assert_eq!(update["changes"]["kind"], 9842);
        assert!(
            stream
                .update(&workflow(&keys, "one", 39842, 14, vec![]))
                .is_none()
        );
    }

    #[test]
    fn content_clear_and_tag_order_changes_have_defined_semantics() {
        let keys = Keys::generate();
        let first = workflow(
            &keys,
            "one",
            39842,
            10,
            vec![Tag::custom("future", ["a"]), Tag::custom("future", ["b"])],
        );
        let mut next = workflow(
            &keys,
            "one",
            39842,
            11,
            vec![Tag::custom("future", ["b"]), Tag::custom("future", ["a"])],
        );
        assert_eq!(changes(&first, &next), json!({}));
        next.content = "log tail".into();
        assert_eq!(changes(&next, &first)["content"], "");
    }
    #[test]
    fn reference_prefixes_extend_on_collisions_without_renaming_prior_labels() {
        let mut references = PrefixReferences::default();
        assert_eq!(references.allocate("", "12345678aaaa"), "12345678");
        assert_eq!(references.allocate("", "12345678bbbb"), "12345678b");
        assert_eq!(references.allocate("", "12345678aaaa"), "12345678-2");
        assert_eq!(references.allocate("run/", "12345678aaaa"), "run/12345678");
        assert_eq!(references.allocate("", "短い"), "短い");
    }

    #[test]
    fn legacy_job_references_use_the_initial_result_event_prefix() {
        let keys = Keys::generate();
        let allocation = "aa".repeat(32);
        let original = job(&keys, "one", &allocation, 9841, 10, vec![]);
        let tags = original
            .tags
            .iter()
            .filter(|tag| {
                tag.as_slice()
                    .get(4)
                    .is_none_or(|marker| marker != "job-allocation")
            })
            .cloned()
            .collect();
        let legacy = event(&keys, 9841, 10, tags);
        let snapshot = Stream::default().update(&legacy).unwrap();
        assert_eq!(snapshot["ref"], format!("one/{}", &legacy.id.to_hex()[..8]));
        assert!(snapshot["allocation_id"].is_null());
        assert_eq!(snapshot["workflow"], "ci.yml");
    }
}
