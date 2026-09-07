//! CI fixture events for the ngit-ci NIP kinds ngit consumes.
//!
//! Building the tags by hand in every test is how event-shape drift creeps
//! in, so the shapes live here once: Workflow Results (9842), Workflow
//! Progress (39842), Job Results (9841), Service Requests/Stops
//! (9843/9844) and Manual Triggers (9840), with the common `a`/`c`/`w`/`o`
//! tags and the NIP-22 pull-request or Git-ref trigger context.
//!
//! The builders are pure: they sign events without touching a relay, so a
//! test can inspect one before publishing it. [`Harness::publish_ci_events`]
//! sends them to a relay listed on the repository announcement, which is
//! where ngit's repository fetch will find them.
//!
//! Every timestamp is explicit. Fixtures that need an expired NIP-40 marker
//! set `created_at` and `expiration` in the past rather than sleeping.

use anyhow::{Context, Result, bail};
use bitcoin_hashes::sha256;
use nostr_sdk::prelude::*;

use crate::{
    harness::Harness,
    scenarios::{PublishedPr, PublishedRepo},
};

/// Kind 9840 — Manual Trigger, signed by a repository maintainer.
pub const KIND_CI_MANUAL_TRIGGER: Kind = Kind::Custom(9840);
/// Kind 9841 — Job Result, signed by the compute provider.
pub const KIND_CI_JOB_RESULT: Kind = Kind::Custom(9841);
/// Kind 9842 — Workflow Result, signed by the coordinator.
pub const KIND_CI_WORKFLOW_RESULT: Kind = Kind::Custom(9842);
/// Kind 9843 — Service Request, signed by the requester.
pub const KIND_CI_SERVICE_REQUEST: Kind = Kind::Custom(9843);
/// Kind 9844 — Service Stop, signed by the requester.
pub const KIND_CI_SERVICE_STOP: Kind = Kind::Custom(9844);
/// Kind 39842 — Workflow Progress, an expiring addressable run marker.
pub const KIND_CI_WORKFLOW_PROGRESS: Kind = Kind::Custom(39842);
/// Kind 30617 — the NIP-34 repository announcement `a` tags point at.
pub const KIND_REPO_ANNOUNCEMENT: u16 = 30617;

/// A relay hint every fixture quote carries. Never dialled: the quoted event
/// is always published alongside the run that quotes it.
const RELAY_HINT: &str = "wss://relay.example";

/// SHA-256 of a workflow file's content, for the `w` tag.
#[must_use]
pub fn workflow_hash(content: &str) -> String {
    sha256::hash(content.as_bytes()).to_string()
}

/// The `a` coordinate CI events use for a published repository.
#[must_use]
pub fn repo_coordinate(repo: &PublishedRepo) -> String {
    format!(
        "{KIND_REPO_ANNOUNCEMENT}:{}:{}",
        repo.maintainer_keys.public_key().to_hex(),
        repo.identifier,
    )
}

/// Why an attempt was run, and the context tags that go with it.
#[derive(Clone, Debug)]
pub enum CiTrigger {
    /// A push/tag run: a Git-ref `r` tag and no NIP-22 tags.
    Push { git_ref: String },
    /// A pull-request run: NIP-22 tags naming the kind-1618 root and the
    /// 1618/1619 event that supplied the commit.
    PullRequest {
        root: EventId,
        root_author: PublicKey,
        supplying: EventId,
        supplying_kind: u16,
        supplying_author: PublicKey,
    },
}

impl CiTrigger {
    /// A run of the PR itself — root and supplying event are the same.
    #[must_use]
    pub fn pull_request(pr: &PublishedPr) -> Self {
        Self::PullRequest {
            root: pr.event_id,
            root_author: pr.author_pubkey,
            supplying: pr.event_id,
            supplying_kind: 1618,
            supplying_author: pr.author_pubkey,
        }
    }

    /// A run of a kind-1619 revision of `pr`.
    #[must_use]
    pub fn pull_request_revision(pr: &PublishedPr, revision: &Event) -> Self {
        Self::PullRequest {
            root: pr.event_id,
            root_author: pr.author_pubkey,
            supplying: revision.id,
            supplying_kind: 1619,
            supplying_author: revision.pubkey,
        }
    }

    #[must_use]
    pub fn push(git_ref: impl Into<String>) -> Self {
        Self::Push {
            git_ref: git_ref.into(),
        }
    }

    fn normalized(&self) -> &'static str {
        match self {
            Self::Push { .. } => "push",
            Self::PullRequest { .. } => "pull_request",
        }
    }

    fn context_tags(&self) -> Vec<Tag> {
        match self {
            Self::Push { git_ref } => vec![tag(&["r", git_ref])],
            Self::PullRequest {
                root,
                root_author,
                supplying,
                supplying_kind,
                supplying_author,
            } => vec![
                tag(&["E", &root.to_hex()]),
                tag(&["K", "1618"]),
                tag(&["P", &root_author.to_hex()]),
                tag(&["e", &supplying.to_hex()]),
                tag(&["k", &supplying_kind.to_string()]),
                tag(&["p", &supplying_author.to_hex()]),
            ],
        }
    }
}

/// An expiring Workflow Progress marker.
#[derive(Clone, Debug)]
pub struct CiProgress {
    pub status: String,
    pub created_at: u64,
    pub expiration: u64,
    pub conclusion: Option<String>,
}

impl CiProgress {
    /// An unexpired `in_progress` marker: the run is executing.
    #[must_use]
    pub fn running(now: u64) -> Self {
        Self {
            status: "in_progress".to_owned(),
            created_at: now.saturating_sub(60),
            expiration: now + 600,
            conclusion: None,
        }
    }

    /// A marker whose NIP-40 expiry has already passed: the publisher stopped
    /// renewing it. Published with past timestamps so no test ever sleeps.
    #[must_use]
    pub fn expired(now: u64) -> Self {
        let created_at = now.saturating_sub(3_600);
        Self {
            status: "in_progress".to_owned(),
            created_at,
            expiration: created_at + 600,
            conclusion: None,
        }
    }
}

/// One job within a run.
#[derive(Clone, Debug)]
pub struct CiJob {
    pub job_id: String,
    pub conclusion: String,
    /// The compute provider signing the Job Result. May be the coordinator.
    pub provider: Keys,
    /// The Job Result event content.
    pub log_tail: String,
    /// The provider-published full log URL, when available.
    pub logs: Option<String>,
    /// Whether the Workflow Result quotes this Job Result. Only an accepted
    /// job carries coordinator trust to a separate provider.
    pub accepted: bool,
}

impl CiJob {
    #[must_use]
    pub fn new(job_id: impl Into<String>, conclusion: impl Into<String>, provider: &Keys) -> Self {
        Self {
            job_id: job_id.into(),
            conclusion: conclusion.into(),
            provider: provider.clone(),
            log_tail: String::new(),
            logs: None,
            accepted: true,
        }
    }

    #[must_use]
    pub fn log_tail(mut self, log_tail: impl Into<String>) -> Self {
        self.log_tail = log_tail.into();
        self
    }

    #[must_use]
    pub fn logs(mut self, logs: impl Into<String>) -> Self {
        self.logs = Some(logs.into());
        self
    }

    #[must_use]
    pub fn unaccepted(mut self) -> Self {
        self.accepted = false;
        self
    }
}

/// A run's frozen request-provenance quote.
#[derive(Clone, Debug)]
pub struct CiProvenance {
    pub event_id: EventId,
    pub requester: PublicKey,
    /// `service-request` or `manual-trigger`.
    pub marker: String,
}

impl CiProvenance {
    #[must_use]
    pub fn service_request(request: &Event) -> Self {
        Self {
            event_id: request.id,
            requester: request.pubkey,
            marker: "service-request".to_owned(),
        }
    }

    #[must_use]
    pub fn manual_trigger(trigger: &Event) -> Self {
        Self {
            event_id: trigger.id,
            requester: trigger.pubkey,
            marker: "manual-trigger".to_owned(),
        }
    }
}

/// One workflow run attempt, as a fixture.
#[derive(Clone, Debug)]
pub struct CiRunSpec {
    pub repo_coordinate: String,
    pub run_id: String,
    pub workflow_path: String,
    pub workflow_hash: String,
    /// `c` tags: the peeled commit first, then any annotated-tag object ids.
    pub commits: Vec<String>,
    pub trigger: CiTrigger,
    pub created_at: u64,
    pub started_at: u64,
    /// `Some` publishes a Workflow Result with this conclusion.
    pub conclusion: Option<String>,
    pub progress: Option<CiProgress>,
    pub jobs: Vec<CiJob>,
    pub provenance: Option<CiProvenance>,
}

impl CiRunSpec {
    /// A concluded, successful run against `commit` with no progress marker,
    /// no jobs and no request provenance.
    #[must_use]
    pub fn new(
        repo: &PublishedRepo,
        run_id: impl Into<String>,
        commit: impl Into<String>,
        trigger: CiTrigger,
        now: u64,
    ) -> Self {
        let created_at = now.saturating_sub(60);
        Self {
            repo_coordinate: repo_coordinate(repo),
            run_id: run_id.into(),
            workflow_path: "ci.yml".to_owned(),
            workflow_hash: workflow_hash(""),
            commits: vec![commit.into()],
            trigger,
            created_at,
            started_at: created_at,
            conclusion: Some("success".to_owned()),
            progress: None,
            jobs: Vec::new(),
            provenance: None,
        }
    }

    #[must_use]
    pub fn workflow(mut self, path: impl Into<String>, hash: impl Into<String>) -> Self {
        self.workflow_path = path.into();
        self.workflow_hash = hash.into();
        self
    }

    /// Add a further `c` tag, e.g. the annotated-tag object id a commit peels
    /// from.
    #[must_use]
    pub fn also_commit(mut self, commit: impl Into<String>) -> Self {
        self.commits.push(commit.into());
        self
    }

    #[must_use]
    pub fn conclusion(mut self, conclusion: Option<&str>) -> Self {
        self.conclusion = conclusion.map(ToOwned::to_owned);
        self
    }

    #[must_use]
    pub fn progress(mut self, progress: CiProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    #[must_use]
    pub fn job(mut self, job: CiJob) -> Self {
        self.jobs.push(job);
        self
    }

    #[must_use]
    pub fn provenance(mut self, provenance: CiProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    #[must_use]
    pub fn started_at(mut self, started_at: u64) -> Self {
        self.started_at = started_at;
        self
    }

    fn common_tags(&self) -> Vec<Tag> {
        let mut tags = vec![tag(&["a", &self.repo_coordinate])];
        for commit in &self.commits {
            tags.push(tag(&["c", commit]));
        }
        tags.push(tag(&["w", &self.workflow_path, &self.workflow_hash]));
        tags.push(tag(&["o", self.trigger.normalized()]));
        tags.extend(self.trigger.context_tags());
        tags
    }

    fn provenance_tag(&self) -> Option<Tag> {
        self.provenance.as_ref().map(|provenance| {
            tag(&[
                "q",
                &provenance.event_id.to_hex(),
                RELAY_HINT,
                &provenance.requester.to_hex(),
                &provenance.marker,
            ])
        })
    }
}

/// The signed events one [`CiRunSpec`] produces.
#[derive(Clone, Debug)]
pub struct CiRunEvents {
    pub jobs: Vec<Event>,
    pub progress: Option<Event>,
    pub result: Option<Event>,
}

impl CiRunEvents {
    /// Every event, in publication order: Job Results first, so the Workflow
    /// Result can quote them.
    #[must_use]
    pub fn all(&self) -> Vec<Event> {
        self.jobs
            .iter()
            .chain(self.progress.iter())
            .chain(self.result.iter())
            .cloned()
            .collect()
    }
}

/// Sign the events for one run.
///
/// # Errors
///
/// Returns an error when an event cannot be signed, or when the spec asks for
/// neither a Workflow Result nor a Progress marker — a run with no container
/// is not something a client can attribute to a repository.
pub fn build_ci_run(coordinator: &Keys, spec: &CiRunSpec) -> Result<CiRunEvents> {
    if spec.conclusion.is_none() && spec.progress.is_none() {
        bail!("a CI run fixture needs a Workflow Result, a Progress marker, or both");
    }

    let run_address = format!(
        "{}:{}:{}",
        KIND_CI_WORKFLOW_PROGRESS.as_u16(),
        coordinator.public_key().to_hex(),
        spec.run_id,
    );

    let mut jobs = Vec::with_capacity(spec.jobs.len());
    for job in &spec.jobs {
        let mut tags = spec.common_tags();
        tags.extend([
            tag(&["q", &run_address, RELAY_HINT]),
            tag(&["job", &job.job_id]),
            tag(&["conclusion", &job.conclusion]),
        ]);
        if let Some(logs) = &job.logs {
            tags.push(tag(&["logs", logs]));
        }
        jobs.push(sign_with_content(
            &job.provider,
            KIND_CI_JOB_RESULT,
            &job.log_tail,
            spec.created_at,
            tags,
        )?);
    }

    let job_quotes: Vec<Tag> = spec
        .jobs
        .iter()
        .zip(jobs.iter())
        .filter(|(job, _)| job.accepted)
        .map(|(job, event)| {
            tag(&[
                "q",
                &event.id.to_hex(),
                RELAY_HINT,
                &job.provider.public_key().to_hex(),
                &job.job_id,
            ])
        })
        .collect();

    let progress = spec
        .progress
        .as_ref()
        .map(|progress| {
            let mut tags = spec.common_tags();
            tags.extend([
                tag(&["d", &spec.run_id]),
                tag(&["status", &progress.status]),
                tag(&["expiration", &progress.expiration.to_string()]),
                tag(&["started_at", &spec.started_at.to_string()]),
            ]);
            if let Some(conclusion) = &progress.conclusion {
                tags.push(tag(&["conclusion", conclusion]));
            }
            // A queued marker must omit the frozen service-request quote:
            // final authorization is only selected at runner handoff.
            if progress.status != "queued" {
                tags.extend(spec.provenance_tag());
            }
            tags.extend(job_quotes.clone());
            sign(
                coordinator,
                KIND_CI_WORKFLOW_PROGRESS,
                progress.created_at,
                tags,
            )
        })
        .transpose()?;

    let result = spec
        .conclusion
        .as_ref()
        .map(|conclusion| {
            let mut tags = spec.common_tags();
            tags.extend([
                tag(&["r", &spec.run_id]),
                tag(&["conclusion", conclusion]),
                tag(&["started_at", &spec.started_at.to_string()]),
            ]);
            tags.extend(spec.provenance_tag());
            tags.extend(job_quotes.clone());
            sign(coordinator, KIND_CI_WORKFLOW_RESULT, spec.created_at, tags)
        })
        .transpose()?;

    Ok(CiRunEvents {
        jobs,
        progress,
        result,
    })
}

/// Sign a Service Request (9843) or Service Stop (9844).
///
/// # Errors
///
/// Returns an error when the event cannot be signed.
pub fn build_service_control(
    author: &Keys,
    coordinator: &PublicKey,
    repo_coordinate: &str,
    is_request: bool,
    created_at: u64,
) -> Result<Event> {
    sign(
        author,
        if is_request {
            KIND_CI_SERVICE_REQUEST
        } else {
            KIND_CI_SERVICE_STOP
        },
        created_at,
        vec![
            tag(&["a", repo_coordinate]),
            tag(&["p", &coordinator.to_hex()]),
        ],
    )
}

/// Sign a Manual Trigger (9840) authorizing exactly the run `spec` describes.
///
/// The workflow, commit and trigger context are taken from the run so the
/// trigger validates against it; a fixture that needs a mismatch edits the
/// returned spec before building the run.
///
/// # Errors
///
/// Returns an error when the event cannot be signed.
pub fn build_manual_trigger(
    maintainer: &Keys,
    coordinator: &PublicKey,
    spec: &CiRunSpec,
    created_at: u64,
) -> Result<Event> {
    let mut tags = vec![
        tag(&["p", &coordinator.to_hex()]),
        tag(&["a", &spec.repo_coordinate]),
    ];
    for commit in &spec.commits {
        tags.push(tag(&["c", commit]));
    }
    tags.push(tag(&["w", &spec.workflow_path, &spec.workflow_hash]));
    // A Manual Trigger carries the common tags except `o`; it is always
    // normalized as `manual`. A pull-request context contributes its NIP-22
    // tags except the participant `p`: on a 9840 the `p` slot is the
    // coordinator address, and a coordinator rejects a request naming anyone
    // else.
    tags.extend(
        spec.trigger
            .context_tags()
            .into_iter()
            .filter(|tag| tag.as_slice().first().map(String::as_str) != Some("p")),
    );
    sign(maintainer, KIND_CI_MANUAL_TRIGGER, created_at, tags)
}

impl Harness {
    /// Publish signed CI events to `relay_url`.
    ///
    /// Use a relay the repository announcement lists — e.g. one registered
    /// through [`crate::PublishRepoOpts::extra_repo_relays`] — so ngit's
    /// repository fetch queries it.
    ///
    /// # Errors
    ///
    /// Returns an error when the relay cannot be reached or rejects an event.
    /// A rejection is always a fixture bug worth failing on: the assertions
    /// that follow would otherwise silently describe an empty relay.
    pub async fn publish_ci_events(&self, relay_url: &str, events: &[Event]) -> Result<()> {
        let client = Client::default();
        client
            .add_relay(relay_url)
            .await
            .with_context(|| format!("failed to add relay {relay_url} for CI fixture publish"))?;
        client.connect().await;
        for event in events {
            let output = client
                .send_event(event)
                .to([relay_url])
                .await
                .with_context(|| {
                    format!(
                        "failed to publish kind-{} CI fixture event to {relay_url}",
                        event.kind.as_u16()
                    )
                })?;
            if !output.failed.is_empty() {
                client.disconnect().await;
                bail!(
                    "relay at {relay_url} rejected kind-{} CI fixture event id={}: {:?}",
                    event.kind.as_u16(),
                    event.id,
                    output.failed,
                );
            }
        }
        client.disconnect().await;
        Ok(())
    }

    /// Build and publish one workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error when the events cannot be signed or published.
    pub async fn publish_ci_run(
        &self,
        relay_url: &str,
        coordinator: &Keys,
        spec: &CiRunSpec,
    ) -> Result<CiRunEvents> {
        let events = build_ci_run(coordinator, spec)?;
        self.publish_ci_events(relay_url, &events.all()).await?;
        Ok(events)
    }
}

fn sign(keys: &Keys, kind: Kind, created_at: u64, tags: Vec<Tag>) -> Result<Event> {
    sign_with_content(keys, kind, "", created_at, tags)
}

fn sign_with_content(
    keys: &Keys,
    kind: Kind,
    content: &str,
    created_at: u64,
    tags: Vec<Tag>,
) -> Result<Event> {
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(created_at))
        .finalize(keys)
        .with_context(|| format!("failed to sign kind-{} CI fixture event", kind.as_u16()))
}

fn tag(values: &[&str]) -> Tag {
    Tag::parse(
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
    )
    .expect("CI fixture tags are well formed")
}
