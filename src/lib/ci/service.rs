//! Repository coordinator relationships, observed activity and service claims.
use std::collections::{BTreeSet, HashMap};

use nostr::prelude::{Coordinate, Event, Kind, PublicKey, Timestamp};
use serde::Serialize;

use super::{controls::open_requests, kinds::validate_service_control, total_order_position};

#[derive(Debug, Serialize)]
pub struct ServiceRow {
    pub coordinator: PublicKey,
    /// Optional NIP-05 label with a verified key binding; never a profile name.
    pub verified_identity: Option<String>,
    pub repository: String,
    pub service: &'static str,
    pub request: &'static str,
    pub request_event: Option<String>,
    pub request_created_at: Option<u64>,
    pub advertised_ready: bool,
    pub status_event: Option<String>,
    pub status_created_at: Option<u64>,
    pub status_expires_at: Option<u64>,
    pub advertisement_event: Option<String>,
    pub readiness_event: Option<String>,
    pub control_events: Vec<String>,
    /// All observed controls, newest first, with current maintainer
    /// attribution.
    pub request_history: Vec<RequestHistoryEntry>,
    pub activity: ServiceActivity,
}
#[derive(Debug, Serialize)]
pub struct RequestHistoryEntry {
    pub event_id: String,
    pub author: PublicKey,
    pub created_at: u64,
    pub action: &'static str,
    pub maintainer: bool,
}

/// Activity is bounded by publication time, not a claim of complete history.
pub const ACTIVITY_WINDOW_SECS: u64 = 30 * 86400;

#[derive(Debug, Default, Serialize)]
pub struct ServiceActivity {
    pub runs_observed: usize,
    pub maintainer_directed_runs: usize,
    pub running: usize,
    pub concluded: usize,
    pub stale: usize,
    pub latest: Option<RecentRun>,
}

#[derive(Debug, Serialize)]
pub struct RecentRun {
    pub run_id: String,
    pub event_id: String,
    pub workflow: String,
    pub commit: String,
    pub status: String,
    pub reported_at: u64,
    pub classification: Option<&'static str>,
}

fn values<'a>(e: &'a Event, name: &'a str) -> impl Iterator<Item = &'a [String]> {
    e.tags
        .iter()
        .map(nostr::prelude::Tag::as_slice)
        .filter(move |t| t[0] == name)
}
fn one<'a>(e: &'a Event, name: &str) -> Option<&'a str> {
    let mut tags = e.tags.iter().filter(|t| t.as_slice()[0] == name);
    let t = tags.next()?.as_slice();
    (t.len() == 2 && tags.next().is_none()).then(|| t[1].as_str())
}
fn live(e: &Event, now: Timestamp, ttl: u64) -> bool {
    one(e, "expiration")
        .and_then(|x| x.parse::<u64>().ok())
        .is_some_and(|end| {
            e.created_at <= now
                && end > now.as_secs()
                && end > e.created_at.as_secs()
                && end - e.created_at.as_secs() <= ttl
        })
}
fn latest<'a>(events: impl Iterator<Item = &'a Event>) -> Option<&'a Event> {
    events.max_by_key(|e| total_order_position(e.created_at, e.id))
}
fn capabilities(e: &Event) -> bool {
    if values(e, "runs_on").next().is_some() {
        return false;
    }
    let families: BTreeSet<_> = values(e, "W")
        .filter(|t| t.len() == 2 && !t[1].is_empty())
        .map(|t| t[1].to_ascii_lowercase())
        .collect();
    if families.len() != values(e, "W").count() {
        return false;
    }
    let mut selected = BTreeSet::new();
    for t in values(e, "R") {
        if t.len() != 2 {
            return false;
        }
        let Some((family, selector)) = t[1].split_once(':') else {
            return false;
        };
        if selector.is_empty() || !families.contains(&family.to_ascii_lowercase()) {
            return false;
        }
        selected.insert(family.to_ascii_lowercase());
    }
    selected == families
}
fn advertisement(e: &Event, now: Timestamp) -> bool {
    e.content.is_empty()
        && values(e, "d").next().is_none()
        && live(e, now, 1800)
        && one(e, "M").is_some()
        && one(e, "X").is_some()
        && capabilities(e)
}
fn ready(e: &Event, root: &Coordinate, now: Timestamp) -> bool {
    if !e.content.is_empty() || values(e, "d").next().is_some() || !live(e, now, 86400) {
        return false;
    }
    let mut seen = BTreeSet::new();
    let mut matches = false;
    for t in e
        .tags
        .iter()
        .map(nostr::prelude::Tag::as_slice)
        .filter(|t| matches!(t[0].as_str(), "a" | "p"))
    {
        if !(2..=3).contains(&t.len()) || !seen.insert((t[0].clone(), t[1].clone())) {
            return false;
        }
        matches |= (t[0] == "a" && t[1] == root.to_string())
            || (t[0] == "p" && t[1] == root.public_key.to_hex());
    }
    matches
}
fn status_shape(e: &Event, root: &Coordinate) -> bool {
    e.content.is_empty()
        && one(e, "d") == Some(root.to_string().as_str())
        && one(e, "s") == Some("acting")
        && capabilities(e)
        && values(e, "a")
            .next()
            .is_some_and(|t| (2..=3).contains(&t.len()) && t[1] == root.to_string())
}
fn deleted(event: &Event, events: &[&Event], now: Timestamp) -> bool {
    let address = format!(
        "{}:{}:{}",
        event.kind.as_u16(),
        event.pubkey,
        one(event, "d").unwrap_or_default()
    );
    events.iter().any(|d| {
        d.kind == Kind::EventDeletion
            && d.pubkey == event.pubkey
            && d.created_at <= now
            && d.created_at >= event.created_at
            && d.tags.iter().any(|t| {
                let t = t.as_slice();
                t.len() >= 2
                    && ((t[0] == "a" && t[1] == address)
                        || (t[0] == "e" && t[1] == event.id.to_hex()))
            })
    })
}

/// Current claims and request history for each exact repository perspective.
/// Missing observations never prove operator configuration or stopped service.
#[must_use]
pub fn summarize(
    events: &[Event],
    roots: &[Coordinate],
    maintainers: &[PublicKey],
    now: Timestamp,
) -> Vec<ServiceRow> {
    let events: Vec<_> = events
        .iter()
        .filter(|e| e.verify().is_ok() && e.created_at <= now)
        .collect();
    let controls: Vec<_> = events
        .iter()
        .filter_map(|e| validate_service_control(e).ok())
        .collect();
    let mut runs = super::events::group_workflow_runs(events.iter().copied()).runs;
    runs.retain(|r| {
        r.container_created_at().as_secs() >= now.as_secs().saturating_sub(ACTIVITY_WINDOW_SECS)
            && r.repositories.iter().any(|r| roots.contains(&r.coordinate))
    });
    let repository = super::resolve::RepositoryContext {
        confirmed_maintainers: maintainers.to_vec(),
        coordinates: roots.to_vec(),
        grasp_domains: Vec::new(),
    };
    let quoted = events.iter().map(|e| (e.id, (*e).clone())).collect();
    let profiles = HashMap::new();
    let context = super::resolve::resolve_cache_tier(&super::resolve::CiInputs::new(
        &repository,
        &runs,
        &controls,
        &quoted,
        &profiles,
        now,
    ));
    let mut rows = Vec::new();
    for root in roots {
        let authors: BTreeSet<_> = events
            .iter()
            .filter(|e| matches!(e.kind.as_u16(), 19844 | 39844))
            .map(|e| e.pubkey)
            .chain(
                controls
                    .iter()
                    .filter(|c| c.repository.coordinate == *root)
                    .map(|c| c.coordinator),
            )
            .chain(
                runs.iter()
                    .filter(|r| r.repositories.iter().any(|r| r.coordinate == *root))
                    .map(|r| r.coordinator),
            )
            .collect();
        for author in authors {
            let ad = latest(
                events
                    .iter()
                    .copied()
                    .filter(|e| e.pubkey == author && e.kind.as_u16() == 19843),
            );
            let ad_live = ad.is_some_and(|e| advertisement(e, now) && !deleted(e, &events, now));
            let readiness = latest(
                events
                    .iter()
                    .copied()
                    .filter(|e| e.pubkey == author && e.kind.as_u16() == 19844),
            );
            let advertised_ready = ad_live
                && readiness.is_some_and(|e| ready(e, root, now) && !deleted(e, &events, now));
            let status = latest(events.iter().copied().filter(|e| {
                e.pubkey == author
                    && e.kind.as_u16() == 39844
                    && one(e, "d") == Some(root.to_string().as_str())
            }));
            let relevant: Vec<_> = controls
                .iter()
                .filter(|c| c.coordinator == author && c.repository.coordinate == *root)
                .collect();
            let open = open_requests(&controls, &author, root, maintainers, now);
            let requested = open.iter().any(|r| maintainers.contains(&r.author));
            let historical_request = relevant
                .iter()
                .any(|c| c.is_request && maintainers.contains(&c.author));
            let request = if requested {
                "maintainer_requested"
            } else if historical_request {
                "previously_requested"
            } else if relevant.iter().any(|c| c.is_request) {
                "other_request_observed"
            } else {
                "none_observed"
            };
            let mut history = relevant.clone();
            history
                .sort_by_key(|c| std::cmp::Reverse(total_order_position(c.created_at, c.event_id)));
            history.dedup_by_key(|c| c.event_id);
            let request_history = history
                .into_iter()
                .map(|c| RequestHistoryEntry {
                    event_id: c.event_id.to_hex(),
                    author: c.author,
                    created_at: c.created_at.as_secs(),
                    action: if c.is_request { "start" } else { "stop" },
                    maintainer: maintainers.contains(&c.author),
                })
                .collect();
            // Date the latest still-open maintainer request, not a request
            // superseded by a stop. Historical rows date their last request.
            let dated_request = if requested { &open } else { &relevant }
                .iter()
                .copied()
                .filter(|c| {
                    c.is_request
                        && (!(requested || historical_request) || maintainers.contains(&c.author))
                })
                .max_by_key(|c| total_order_position(c.created_at, c.event_id));
            let service = match status {
                Some(e) if deleted(e, &events, now) => "withdrawn",
                Some(e) if !status_shape(e, root) => "invalid_status",
                Some(e) if !live(e, now, 86400) => "status_expired_or_invalid",
                Some(_) if !ad_live => "advertisement_unavailable",
                Some(_) => "acting",
                None => "no_status_observed",
            };
            let mut activity = ServiceActivity::default();
            let mut observed: Vec<_> = runs
                .iter()
                .filter(|r| {
                    r.coordinator == author && r.repositories.iter().any(|r| r.coordinate == *root)
                })
                .collect();
            observed.sort_by_key(|r| {
                total_order_position(r.container_created_at(), r.container_event_id())
            });
            for run in &observed {
                activity.runs_observed += 1;
                if context.run_resolution(run).classification()
                    == Some(super::trust::TrustClassification::MaintainerDirected)
                {
                    activity.maintainer_directed_runs += 1;
                }
                match run.state(now) {
                    super::events::RunState::Running => activity.running += 1,
                    super::events::RunState::Concluded(_) => activity.concluded += 1,
                    super::events::RunState::Stale => activity.stale += 1,
                }
            }
            activity.latest = observed.last().map(|run| RecentRun {
                run_id: run.run_id.clone(),
                event_id: run.container_event_id().to_hex(),
                workflow: run.workflow_path.clone(),
                commit: run.commit().to_owned(),
                status: match run.state(now) {
                    super::events::RunState::Running => run
                        .progress
                        .as_ref()
                        .map_or("running", |p| p.status.as_str())
                        .to_owned(),
                    super::events::RunState::Concluded(c) => c.to_string(),
                    super::events::RunState::Stale => "stale".to_owned(),
                },
                reported_at: run.container_created_at().as_secs(),
                classification: context
                    .run_resolution(run)
                    .classification()
                    .map(|c| c.as_str()),
            });
            if status.is_none()
                && relevant.is_empty()
                && !advertised_ready
                && activity.runs_observed == 0
            {
                continue;
            }
            rows.push(ServiceRow {
                activity,
                coordinator: author,
                verified_identity: None,
                repository: root.to_string(),
                service,
                request,
                request_event: dated_request.map(|c| c.event_id.to_hex()),
                request_created_at: dated_request.map(|c| c.created_at.as_secs()),
                advertised_ready,
                status_event: status.map(|e| e.id.to_hex()),
                status_created_at: status.map(|e| e.created_at.as_secs()),
                status_expires_at: status
                    .and_then(|e| one(e, "expiration").and_then(|v| v.parse().ok())),
                advertisement_event: ad.map(|e| e.id.to_hex()),
                readiness_event: readiness
                    .filter(|e| ready(e, root, now))
                    .map(|e| e.id.to_hex()),
                control_events: relevant.iter().map(|c| c.event_id.to_hex()).collect(),
                request_history,
            });
        }
    }
    rows.sort_by(|a, b| {
        b.activity
            .latest
            .as_ref()
            .map(|r| r.reported_at)
            .cmp(&a.activity.latest.as_ref().map(|r| r.reported_at))
            .then_with(|| a.coordinator.cmp(&b.coordinator))
            .then_with(|| a.repository.cmp(&b.repository))
    });
    rows
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Tag, event::FinalizeEvent};

    use super::*;
    fn event(keys: &Keys, kind: u16, time: u64, tags: Vec<Vec<String>>) -> Event {
        EventBuilder::new(Kind::Custom(kind), "")
            .custom_created_at(Timestamp::from_secs(time))
            .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
            .finalize(keys)
            .unwrap()
    }
    fn t(a: &str, b: impl ToString) -> Vec<String> {
        vec![a.into(), b.to_string()]
    }
    #[test]
    fn activity_discovers_coordinators_counts_attempts_and_keeps_idle_relationships() {
        use super::super::kinds::{
            ProgressStatus,
            test_events::{service_control, workflow_progress, workflow_result},
        };
        let owner = Keys::generate();
        let foreign = Keys::generate();
        let active = Keys::generate();
        let idle = Keys::generate();
        let unrelated = Keys::generate();
        let now = ACTIVITY_WINDOW_SECS + 1000;
        let root =
            Coordinate::new(Kind::GitRepoAnnouncement, owner.public_key()).identifier("ngit");
        let result = workflow_result(&active, &owner, "recent", now - 10);
        let events = vec![
            result.clone(),
            workflow_progress(
                &active,
                &owner,
                "recent",
                ProgressStatus::InProgress,
                now - 20,
                now + 100,
            ),
            workflow_progress(
                &active,
                &owner,
                "live",
                ProgressStatus::Queued,
                now - 5,
                now + 100,
            ),
            workflow_result(&active, &owner, "old", 999),
            workflow_result(&unrelated, &foreign, "foreign", now - 1),
            workflow_result(&unrelated, &owner, "future", now + 1),
            service_control(&owner, &idle, &owner, true, 500),
        ];
        let rows = summarize(
            &events,
            &[root],
            &[owner.public_key()],
            Timestamp::from_secs(now),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].coordinator, active.public_key());
        assert_eq!(rows[0].activity.runs_observed, 2);
        assert_eq!(rows[0].activity.running, 1);
        assert_eq!(rows[0].activity.concluded, 1);
        assert_eq!(rows[0].activity.latest.as_ref().unwrap().status, "queued");
        assert_eq!(rows[0].request, "none_observed");
        assert_eq!(rows[1].coordinator, idle.public_key());
        assert_eq!(rows[1].request, "maintainer_requested");
        assert_eq!(rows[1].activity.runs_observed, 0);
    }

    #[test]
    fn request_age_tracks_open_requests_and_does_not_reuse_stopped_history() {
        use super::super::kinds::test_events::service_control;
        let owner = Keys::generate();
        let coordinator = Keys::generate();
        let root =
            Coordinate::new(Kind::GitRepoAnnouncement, owner.public_key()).identifier("ngit");
        let first = service_control(&owner, &coordinator, &owner, true, 100);
        let stop = service_control(&owner, &coordinator, &owner, false, 200);
        let later = service_control(&owner, &coordinator, &owner, true, 300);
        let mut events = vec![first.clone(), stop];
        let rows = summarize(
            &events,
            std::slice::from_ref(&root),
            &[owner.public_key()],
            Timestamp::from_secs(250),
        );
        assert_eq!(rows[0].request, "previously_requested");
        assert_eq!(rows[0].request_created_at, Some(100));
        events.push(later.clone());
        let rows = summarize(
            &events,
            &[root],
            &[owner.public_key()],
            Timestamp::from_secs(400),
        );
        assert_eq!(rows[0].request, "maintainer_requested");
        assert_eq!(rows[0].request_created_at, Some(300));
        assert_eq!(rows[0].request_event, Some(later.id.to_hex()));
        assert_eq!(
            rows[0]
                .request_history
                .iter()
                .map(|h| (h.action, h.created_at))
                .collect::<Vec<_>>(),
            vec![("start", 300), ("stop", 200), ("start", 100)]
        );
        assert!(rows[0].request_history.iter().all(|h| h.maintainer));
    }

    #[test]
    fn requests_readiness_and_acting_are_independent_and_expire() {
        let owner = Keys::generate();
        let coordinator = Keys::generate();
        let stranger = Keys::generate();
        let root = Coordinate::new(Kind::Custom(30617), owner.public_key()).identifier("repo");
        let now = Timestamp::from_secs(200);
        let ad = event(
            &coordinator,
            19843,
            100,
            vec![
                t("M", "maintainer-request"),
                t("X", "request-required"),
                t("expiration", 1000),
            ],
        );
        let ready = event(
            &coordinator,
            19844,
            100,
            vec![t("p", owner.public_key()), t("expiration", 1000)],
        );
        let unrelated = event(
            &stranger,
            19843,
            100,
            vec![t("M", "open"), t("X", "automatic"), t("expiration", 1000)],
        );
        let mut events = vec![ad, ready, unrelated];
        let rows = summarize(
            &events,
            std::slice::from_ref(&root),
            &[owner.public_key()],
            now,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].advertised_ready);
        assert_eq!(rows[0].service, "no_status_observed");
        assert_eq!(rows[0].request, "none_observed");
        events.push(event(
            &owner,
            9843,
            110,
            vec![t("a", &root), t("p", coordinator.public_key())],
        ));
        let row = summarize(
            &events,
            std::slice::from_ref(&root),
            &[owner.public_key()],
            now,
        )
        .remove(0);
        assert_eq!(row.request, "maintainer_requested");
        assert_eq!(row.service, "no_status_observed");
        events.push(event(
            &coordinator,
            39844,
            120,
            vec![
                t("d", &root),
                t("a", &root),
                t("s", "acting"),
                t("expiration", 1000),
            ],
        ));
        let row = summarize(
            &events,
            std::slice::from_ref(&root),
            &[owner.public_key()],
            now,
        )
        .remove(0);
        assert_eq!(row.service, "acting");
        events.push(event(
            &owner,
            9844,
            130,
            vec![t("a", &root), t("p", coordinator.public_key())],
        ));
        let row = summarize(
            &events,
            std::slice::from_ref(&root),
            &[owner.public_key()],
            now,
        )
        .remove(0);
        assert_eq!(row.service, "acting");
        assert_eq!(row.request, "previously_requested");
        let row = summarize(
            &events,
            std::slice::from_ref(&root),
            &[owner.public_key()],
            Timestamp::from_secs(1100),
        )
        .remove(0);
        assert_eq!(row.service, "status_expired_or_invalid");
        assert!(!row.advertised_ready);
        events.push(event(
            &stranger,
            5,
            140,
            vec![t("a", format!("39844:{}:{root}", coordinator.public_key()))],
        ));
        assert_eq!(
            summarize(
                &events,
                std::slice::from_ref(&root),
                &[owner.public_key()],
                now
            )[0]
            .service,
            "acting"
        );
        events.push(event(
            &coordinator,
            5,
            150,
            vec![t("a", format!("39844:{}:{root}", coordinator.public_key()))],
        ));
        assert_eq!(
            summarize(
                &events,
                std::slice::from_ref(&root),
                &[owner.public_key()],
                now
            )[0]
            .service,
            "withdrawn"
        );
    }
    #[test]
    fn newer_readiness_removal_and_future_claims_do_not_offer_service() {
        let owner = Keys::generate();
        let c = Keys::generate();
        let root = Coordinate::new(Kind::Custom(30617), owner.public_key()).identifier("repo");
        let mut events = vec![
            event(
                &c,
                19843,
                100,
                vec![t("M", "open"), t("X", "automatic"), t("expiration", 1000)],
            ),
            event(&c, 19844, 100, vec![t("a", &root), t("expiration", 1000)]),
        ];
        events.push(event(&c, 19844, 110, vec![t("expiration", 1000)]));
        events.push(event(
            &c,
            39844,
            300,
            vec![
                t("d", &root),
                t("a", &root),
                t("s", "acting"),
                t("expiration", 1000),
            ],
        ));
        assert!(
            summarize(
                &events,
                &[root],
                &[owner.public_key()],
                Timestamp::from_secs(200)
            )
            .is_empty()
        );
    }
}
