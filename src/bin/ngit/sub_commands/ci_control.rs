//! `ngit ci request|stop|trigger|cancel` — the maintainer controls that publish
//! the Level 1 evidence `ngit ci status` consumes.
//!
//! A Service Request (9843) and a Service Stop (9844) are standing controls
//! over one repository perspective; a Manual Trigger (9840) is a one-shot
//! authorization for exactly one workflow file at one commit. Every event
//! built here is checked against [`ngit::ci::kinds`] before it is published,
//! so ngit can never publish a control its own reader would skip as
//! malformed.
//!
//! Nothing here decides whether a coordinator will act on the control: the
//! acceptance policy is the coordinator's. ngit warns when the signer is not
//! a confirmed maintainer — the default policy accepts only those — but the
//! NIP lets an operator accept other requesters, so it is a warning and never
//! a refusal for those controls. Exact-run cancellation requires confirmed
//! maintainership and is refused locally when that authority is unavailable.

use std::path::Path;

use anyhow::{Context, Result, bail};
use bitcoin_hashes::sha256;
use ngit::{
    ci::kinds::{
        KIND_CI_MANUAL_TRIGGER, KIND_CI_SERVICE_REQUEST, KIND_CI_SERVICE_STOP,
        KIND_CI_WORKFLOW_CANCELLATION, KIND_CI_WORKFLOW_FORCED_CANCELLATION,
        validate_manual_trigger, validate_service_control, validate_workflow_cancellation,
    },
    client::{Params, get_state_from_cache, send_events_with_results, sign_event},
    login::user::get_user_details,
    repo_lookup::RepositoryLookup,
    repo_ref::RepoRef,
};
use nostr::prelude::{
    Coordinate, Event, EventBuilder, Kind, PublicKey, RelayUrl, Tag, ToBech32,
    nip19::Nip19Coordinate,
};

use crate::{
    ci_commit::{ResolvedCommit, resolve_commit_ish_strict},
    cli::SignerParams,
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_resolved_repo_coordinate_for_publishing,
    sub_commands::repository_fetch::fetching_with_account,
};

/// Publish a kind-9843 Service Request.
///
/// # Errors
///
/// Returns an error when the repository or the coordinator cannot be
/// resolved, when login fails, or when publishing fails.
pub async fn launch_request(
    coordinator: &str,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    launch_service_control(coordinator, true, offline, auth).await
}

/// Publish a kind-9844 Service Stop.
///
/// # Errors
///
/// As [`launch_request`].
pub async fn launch_stop(coordinator: &str, offline: bool, auth: SignerParams<'_>) -> Result<()> {
    launch_service_control(coordinator, false, offline, auth).await
}

async fn launch_service_control(
    coordinator: &str,
    is_request: bool,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let coordinator = parse_coordinator(coordinator)?;
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let mut prepared = Prepared::new(git_repo, offline, auth).await?;

    // The NIP allows exactly one `a` and one `p`: the perspective the signer
    // is asking about, and the coordinator being asked.
    let perspective = prepared.perspective();
    let event = prepared
        .sign(
            EventBuilder::new(
                if is_request {
                    KIND_CI_SERVICE_REQUEST
                } else {
                    KIND_CI_SERVICE_STOP
                },
                "",
            )
            .tags([
                Tag::coordinate(perspective.clone(), prepared.relay_hint()),
                Tag::public_key(coordinator),
            ]),
            if is_request {
                "CI service request"
            } else {
                "CI service stop"
            },
        )
        .await?;
    validate_service_control(&event)
        .map_err(|reason| malformed(KIND_CI_SERVICE_REQUEST.as_u16(), &reason.to_string()))?;

    let warnings: Vec<String> = prepared.maintainer_warning().into_iter().collect();
    let warning = join_warnings(&warnings);
    let action = if is_request {
        "service-requested"
    } else {
        "service-stopped"
    };
    let event_id = event.id;
    prepared.publish(vec![event], coordinator, offline).await?;

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": action,
            "entity": "ci",
            "event": crate::output::event_id_to_nevent(event_id, prepared.repo_ref.relays.first()),
            "coordinator": coordinator.to_bech32().unwrap_or_else(|_| coordinator.to_hex()),
            "repository": naddr(&perspective, prepared.relay_hint()),
            "warning": warning,
        }));
    }
    for warning in &warnings {
        println!("{warning}");
    }
    println!(
        "{} coordinator {}",
        if is_request {
            "CI service requested from"
        } else {
            "CI service stopped for"
        },
        coordinator
            .to_bech32()
            .unwrap_or_else(|_| coordinator.to_hex()),
    );
    Ok(())
}

/// Publish an exact-run cancellation from a confirmed repository maintainer.
///
/// # Errors
/// Returns an error for malformed identity, unavailable repository, unconfirmed
/// maintainer, signing failure or unsuccessful publication.
pub async fn launch_cancel(
    coordinator: &str,
    run_id: &str,
    force: bool,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let coordinator = parse_coordinator(coordinator)?;
    if run_id.len() != 64
        || !run_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!(
            "invalid workflow run id; pass the exact 64-character lowercase hexadecimal run id from `ngit ci status`"
        );
    }
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let mut prepared = Prepared::new(git_repo, offline, auth).await?;
    if !prepared
        .repo_ref
        .confirmed_maintainers()
        .contains(&prepared.user_pubkey)
    {
        bail!(
            "workflow cancellation requires a confirmed repository maintainer; select an authorized signer with --signer"
        );
    }
    // A run belongs to an exact perspective. Unlike standing service requests,
    // cancellation must not substitute the signer's own announcement.
    let perspective = Coordinate {
        kind: Kind::GitRepoAnnouncement,
        public_key: prepared.repo_ref.selected_maintainer,
        identifier: prepared.repo_ref.identifier.clone(),
    };
    let kind = if force {
        KIND_CI_WORKFLOW_FORCED_CANCELLATION
    } else {
        KIND_CI_WORKFLOW_CANCELLATION
    };
    let event = prepared
        .sign(
            EventBuilder::new(kind, "").tags([
                Tag::coordinate(perspective.clone(), None),
                Tag::public_key(coordinator),
                Tag::custom("r", [run_id]),
            ]),
            "CI workflow cancellation",
        )
        .await?;
    validate_workflow_cancellation(&event)
        .map_err(|reason| malformed(kind.as_u16(), &reason.to_string()))?;
    let event_id = event.id;
    prepared.publish(vec![event], coordinator, offline).await?;
    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok", "action": "cancellation-requested", "entity": "ci",
            "event": crate::output::event_id_to_nevent(event_id, prepared.repo_ref.relays.first()),
            "coordinator": coordinator.to_bech32().unwrap_or_else(|_| coordinator.to_hex()),
            "repository": naddr(&perspective, prepared.relay_hint()), "run_id": run_id,
            "forced": force,
        }));
    }
    println!(
        "Cancellation requested for run {run_id}; use `ngit ci status` to observe confirmed completion."
    );
    Ok(())
}

/// Publish a kind-9840 Manual Trigger.
///
/// # Errors
///
/// Returns an error when the commit-ish does not resolve, when a `c` object
/// does not peel to the resolved commit, when the workflow file does not
/// exist at that commit, or when login or publishing fails. Nothing is
/// published unless every check passed.
pub async fn launch_trigger(
    coordinator: &str,
    commit_ish: Option<&str>,
    workflow: &str,
    git_ref: Option<&str>,
    selection: &crate::cli::CiExecutionSelection,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    if selection.rerun_dependencies && !workflow.starts_with(".ngit/nix/workflows/") {
        bail!(
            "--rerun-dependencies requires a native Nix workflow under .ngit/nix/workflows/; omit the flag for other workflows"
        );
    }
    let coordinator = parse_coordinator(coordinator)?;
    if let Some(git_ref) = git_ref {
        if !git_ref.starts_with("refs/") {
            bail!(
                "`--ref {git_ref}` is not a Git ref; pass the full ref, e.g. `refs/heads/{git_ref}`"
            );
        }
    }
    // Resolved before anything touches the network: a commit-ish that does
    // not exist or a workflow path that is not in its tree fails without a
    // fetch, a login, or a published event.
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let commit_ish = commit_ish.unwrap_or("HEAD");
    let resolved = resolve_commit_ish_strict(&git_repo, commit_ish)?;
    let workflow_hash = workflow_blob_sha256(&git_repo, &resolved, workflow)?;

    let mut prepared = Prepared::new(git_repo, offline, auth).await?;

    // The perspective goes first; the other announcements ngit knows for this
    // repository follow as further candidates, which the NIP allows and a
    // coordinator resolves independently.
    let perspective = prepared.perspective();
    let mut tags = vec![Tag::public_key(coordinator)];
    for coordinate in prepared.announcement_coordinates(&perspective) {
        // Emit minimal addresses; current coordinators also accept relay hints.
        tags.push(Tag::coordinate(coordinate, None));
    }
    for commit in &resolved.ids {
        tags.push(Tag::parse(["c", commit])?);
    }
    tags.push(Tag::parse(["w", workflow, &workflow_hash])?);
    if let Some(git_ref) = git_ref {
        tags.push(Tag::parse(["r", git_ref])?);
    }

    tags.extend(selection_tags(
        &prepared.git_repo,
        &resolved,
        workflow,
        selection,
    )?);

    let event = prepared
        .sign(
            EventBuilder::new(KIND_CI_MANUAL_TRIGGER, "").tags(tags),
            "CI manual trigger",
        )
        .await?;
    validate_manual_trigger(&event)
        .map_err(|reason| malformed(KIND_CI_MANUAL_TRIGGER.as_u16(), &reason.to_string()))?;

    let mut warnings: Vec<String> = prepared.maintainer_warning().into_iter().collect();
    warnings.extend(prepared.unreachable_commit_warning(&resolved).await);
    let warning = join_warnings(&warnings);
    let event_id = event.id;
    prepared.publish(vec![event], coordinator, offline).await?;

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "triggered",
            "entity": "ci",
            "event": crate::output::event_id_to_nevent(event_id, prepared.repo_ref.relays.first()),
            "coordinator": coordinator.to_bech32().unwrap_or_else(|_| coordinator.to_hex()),
            "repository": naddr(&perspective, prepared.relay_hint()),
            "commit": resolved.commit,
            "commits": resolved.ids,
            "workflow": workflow,
            "workflow_hash": workflow_hash,
            "ref": git_ref,
            "rerun_dependencies": selection.rerun_dependencies,
            "environments": selection.environment,
            "requirements": selection.requirements,
            "warning": warning,
        }));
    }
    for warning in &warnings {
        println!("{warning}");
    }
    println!(
        "requested a manual run of {workflow} at {} from coordinator {}",
        short(&resolved.commit),
        coordinator
            .to_bech32()
            .unwrap_or_else(|_| coordinator.to_hex()),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared publishing context
// ---------------------------------------------------------------------------

/// The repository, the signer and the relays every CI control needs.
struct Prepared {
    git_repo: Repo,
    client: Client,
    repo_ref: RepoRef,
    signer: std::sync::Arc<ngit::NgitSigner>,
    user_ref: ngit::login::user::UserRef,
    user_pubkey: PublicKey,
}

impl Prepared {
    async fn new(git_repo: Repo, offline: bool, auth: SignerParams<'_>) -> Result<Self> {
        let git_repo_path = git_repo.get_path()?;

        let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
        let selected_repo =
            get_resolved_repo_coordinate_for_publishing(&git_repo, &mut client).await?;
        let mut lookup = RepositoryLookup::from_resolved(&git_repo, &selected_repo)?;
        let mut repo_coordinates = selected_repo.coordinate;

        if !offline {
            fetching_with_account(
                &git_repo,
                git_repo_path,
                &mut client,
                &mut repo_coordinates,
                &mut lookup,
                auth,
            )
            .await?;
        }

        let repo_ref = lookup.load(git_repo_path).await?;

        let (signer, user_ref, _) = login::login_or_signup(
            &Some(&git_repo),
            auth.info,
            auth.password,
            Some(&client),
            true,
        )
        .await?;
        let user_pubkey = signer.get_public_key().await?;

        Ok(Self {
            git_repo,
            client,
            repo_ref,
            signer,
            user_ref,
            user_pubkey,
        })
    }

    /// The repository perspective the control is signed for.
    ///
    /// A requester asks for one perspective, so the signer's own announcement
    /// is the natural one when they have published it — that is the
    /// perspective they are a maintainer of by construction. Otherwise the
    /// selected maintainer's coordinate, which is the repository the command
    /// was resolved against.
    fn perspective(&self) -> Coordinate {
        let mine = self
            .repo_ref
            .events
            .keys()
            .any(|coordinate| {
                coordinate.public_key == self.user_pubkey
                    && coordinate.identifier == self.repo_ref.identifier
            })
            .then_some(self.user_pubkey);
        Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: mine.unwrap_or(self.repo_ref.selected_maintainer),
            identifier: self.repo_ref.identifier.clone(),
        }
    }

    /// The perspective, then every other announcement ngit holds for this
    /// repository, in ngit's canonical announcement-tag order.
    fn announcement_coordinates(&self, perspective: &Coordinate) -> Vec<Coordinate> {
        let mut coordinates = vec![perspective.clone()];
        for member in self.repo_ref.members_for_announcement_tags() {
            let has_announcement = self.repo_ref.events.keys().any(|coordinate| {
                coordinate.public_key == member && coordinate.identifier == self.repo_ref.identifier
            });
            if has_announcement && member != perspective.public_key {
                coordinates.push(Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: member,
                    identifier: self.repo_ref.identifier.clone(),
                });
            }
        }
        coordinates
    }

    fn relay_hint(&self) -> Option<RelayUrl> {
        self.repo_ref.relays.first().cloned()
    }

    /// The caveat printed when the signer is not a confirmed maintainer of
    /// the resolved repository.
    ///
    /// Never a refusal: a coordinator's default policy accepts a confirmed
    /// maintainer, but its operator may have accepted other requester keys.
    fn maintainer_warning(&self) -> Option<String> {
        if self
            .repo_ref
            .confirmed_maintainers()
            .contains(&self.user_pubkey)
        {
            return None;
        }
        Some(format!(
            "warning: {} is not a confirmed maintainer of this repository, so a coordinator may ignore this event; its default policy accepts a confirmed maintainer's request, and only its operator can accept other keys",
            self.user_pubkey
                .to_bech32()
                .unwrap_or_else(|_| self.user_pubkey.to_hex()),
        ))
    }

    /// The caveat printed when the resolved commit is not reachable from
    /// the repository state published on nostr.
    ///
    /// The NIP makes a non-pull-request trigger eligible only for a
    /// repository whose resolved state contains the requested commit, so a
    /// coordinator will discard one for a commit that was never pushed. ngit
    /// checks reachability with the objects it holds: every state ref is
    /// peeled to a commit and tested with `graph_descendant_of`. A state ngit
    /// cannot read, or one whose tips are all missing locally, produces no
    /// warning — a false alarm is worse than silence for a check that is only
    /// ever advisory.
    async fn unreachable_commit_warning(&self, resolved: &ResolvedCommit) -> Option<String> {
        let path = self.git_repo.get_path().ok()?;
        let state = get_state_from_cache(Some(path), &self.repo_ref)
            .await
            .ok()?;
        let commit = git2::Oid::from_str(&resolved.commit).ok()?;

        let mut examined = 0usize;
        for value in state.state.values() {
            let Ok(oid) = git2::Oid::from_str(value) else {
                continue;
            };
            let Ok(tip) = self
                .git_repo
                .git_repo
                .find_object(oid, None)
                .and_then(|object| object.peel_to_commit())
            else {
                continue;
            };
            examined += 1;
            if tip.id() == commit
                || self
                    .git_repo
                    .git_repo
                    .graph_descendant_of(tip.id(), commit)
                    .unwrap_or(false)
            {
                return None;
            }
        }

        (examined > 0).then(|| {
            format!(
                "warning: commit {} is not reachable from any ref in this repository's nostr state, so a coordinator will find it ineligible and discard this trigger; push it first",
                short(&resolved.commit),
            )
        })
    }

    async fn sign(&self, builder: EventBuilder, description: &str) -> Result<Event> {
        sign_event(builder, &self.signer, description.to_owned()).await
    }

    /// Publish to the repository relays, and to the coordinator's NIP-65 read
    /// relays when ngit knows them.
    ///
    /// The coordinator's inbox goes in the same bucket as the signer's own
    /// write relays rather than the repository set, so a private repository's
    /// `repo-relay-only` suppression covers it: a private repository has to
    /// hand its coordinator a repository relay rather than have ngit announce
    /// its coordinate on public inboxes.
    async fn publish(
        &mut self,
        events: Vec<Event>,
        coordinator: PublicKey,
        offline: bool,
    ) -> Result<()> {
        let mut publish_relays = self.user_ref.relays.write();
        for relay in self.coordinator_read_relays(coordinator, offline).await {
            if !publish_relays.contains(&relay) {
                publish_relays.push(relay);
            }
        }

        self.client.set_signer(self.signer.clone()).await;
        let outcomes = send_events_with_results(
            &self.client,
            Some(self.git_repo.get_path()?),
            events,
            publish_relays,
            self.repo_ref.relays.clone(),
            true,
            false,
        )
        .await?;

        // A control event exists to be read by a coordinator, so one that
        // reached nothing is a failure rather than a partial success worth
        // reporting as `ok`.
        if !outcomes.iter().any(|(_, accepted)| *accepted) {
            let attempted: Vec<&str> = outcomes.iter().map(|(relay, _)| relay.as_str()).collect();
            bail!(
                "no relay accepted the event, so no coordinator can read it (tried: {})",
                if attempted.is_empty() {
                    "no relays".to_owned()
                } else {
                    attempted.join(" ")
                },
            );
        }
        Ok(())
    }

    /// The coordinator's NIP-65 read (and unmarked) relays, through the
    /// profile machinery ngit already has. Best effort: a coordinator with no
    /// reachable relay list is still addressed on the repository relays,
    /// which the NIP-guidance names as the other publication path.
    async fn coordinator_read_relays(&self, coordinator: PublicKey, offline: bool) -> Vec<String> {
        get_user_details(
            &coordinator,
            Some(&self.client),
            Some(self.git_repo.get_path().unwrap_or(Path::new("."))),
            offline,
            false,
        )
        .await
        .map(|user| user.relays.read())
        .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Git resolution
// ---------------------------------------------------------------------------

/// The SHA-256 of the workflow file's **blob** at the resolved commit.
///
/// Never the working-tree file: `core.autocrlf` and clean/smudge filters make
/// the checkout differ from the object, and the object is what a coordinator
/// hashes.
fn workflow_blob_sha256(git_repo: &Repo, resolved: &ResolvedCommit, path: &str) -> Result<String> {
    let commit = git_repo
        .git_repo
        .find_commit(git2::Oid::from_str(&resolved.commit)?)
        .context("failed to read the resolved commit")?;
    let entry = commit
        .tree()
        .context("failed to read the resolved commit's tree")?
        .get_path(Path::new(path))
        .with_context(|| {
            format!(
                "workflow file `{path}` does not exist at commit {}",
                short(&resolved.commit),
            )
        })?;
    let blob = entry
        .to_object(&git_repo.git_repo)
        .with_context(|| format!("failed to read `{path}` at commit {}", resolved.commit))?
        .into_blob()
        .map_err(|_| {
            anyhow::anyhow!(
                "`{path}` at commit {} is not a file",
                short(&resolved.commit),
            )
        })?;
    Ok(sha256::hash(blob.content()).to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_coordinator(raw: &str) -> Result<PublicKey> {
    PublicKey::parse(raw)
        .with_context(|| format!("`{raw}` is not a coordinator public key; pass an npub or hex"))
}

fn naddr(coordinate: &Coordinate, relay_hint: Option<RelayUrl>) -> String {
    Nip19Coordinate {
        coordinate: coordinate.clone(),
        relays: relay_hint.into_iter().collect(),
    }
    .to_bech32()
    .unwrap_or_else(|_| coordinate.to_string())
}

/// The caveats as one JSON value: `null` when there is nothing to say, so a
/// consumer never branches on a missing key.
fn join_warnings(warnings: &[String]) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("\n"))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// An event ngit built that its own reader would skip is a bug in ngit, not
/// something to publish and let a coordinator puzzle over.
fn malformed(kind: u16, reason: &str) -> anyhow::Error {
    anyhow::anyhow!("ngit built a malformed kind-{kind} event: {reason}")
}

fn selection_tags(
    repo: &Repo,
    resolved: &ResolvedCommit,
    workflow: &str,
    selection: &crate::cli::CiExecutionSelection,
) -> Result<Vec<Tag>> {
    if selection.environment.is_empty() && selection.requirements.is_empty() {
        return Ok(Vec::new());
    }
    if !workflow.starts_with(".ngit/nix/workflows/") {
        bail!("environment and resource overrides require a native Nix workflow");
    }
    let jobs = workflow_jobs(repo, resolved, workflow)?;
    let mut tags = Vec::new();
    for raw in &selection.environment {
        let (job, value) = raw
            .split_once('=')
            .context("use --environment JOB=SHA256[,URL]")?;
        if !jobs.contains(job) {
            bail!("environment override names an unknown workflow job");
        }
        let (hash, url) = value.split_once(',').unwrap_or((value, ""));
        tags.push(Tag::parse(["x", hash, url, job])?);
    }
    for raw in &selection.requirements {
        let (job, expression) = raw
            .split_once('=')
            .context("use --require JOB=PATH:OP:JSON")?;
        if !jobs.contains(job) {
            bail!("resource override names an unknown workflow job");
        }
        ngit::ci::details::requirement(expression).context("invalid manual resource predicate")?;
        tags.push(Tag::parse(["f", expression, job])?);
    }
    Ok(tags)
}

fn workflow_jobs(
    repo: &Repo,
    resolved: &ResolvedCommit,
    workflow: &str,
) -> Result<std::collections::HashSet<String>> {
    let repository = git2::Repository::open(repo.get_path()?)?;
    let commit = repository.find_commit(git2::Oid::from_str(&resolved.commit)?)?;
    let tree = commit.tree()?;
    let entry = tree
        .get_path(Path::new(workflow))
        .context("workflow missing at source commit")?;
    let blob = repository.find_blob(entry.id())?;
    let yaml: serde_json::Value = serde_saphyr::from_str(std::str::from_utf8(blob.content())?)
        .context("invalid native workflow YAML")?;
    let jobs = yaml
        .get("jobs")
        .and_then(serde_json::Value::as_object)
        .context("native workflow requires a jobs mapping")?;
    Ok(jobs.keys().cloned().collect())
}

/// Publish a restart of an exact signed completed result, preserving its
/// inputs.
pub async fn launch_rerun(
    coordinator: &str,
    result: &str,
    job: Option<&str>,
    selection: &crate::cli::CiExecutionSelection,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    use ngit::client::get_events_from_local_cache;
    use nostr::prelude::{EventId, Filter};
    if selection.rerun_dependencies {
        bail!("restart retains external dependencies; use ci trigger for a fresh dependency graph");
    }
    let id = EventId::parse(result)
        .context("restart requires a full Workflow Result event id or nevent")?;
    let coordinator = parse_coordinator(coordinator)?;
    let mut prepared = Prepared::new(Repo::discover()?, offline, auth).await?;
    if !prepared
        .repo_ref
        .confirmed_maintainers()
        .contains(&prepared.user_pubkey)
    {
        bail!("workflow restart requires a confirmed repository maintainer");
    }
    let filter = Filter::default()
        .id(id)
        .kind(ngit::ci::kinds::KIND_CI_WORKFLOW_RESULT)
        .author(coordinator);
    let mut events =
        get_events_from_local_cache(prepared.git_repo.get_path()?, vec![filter.clone()]).await?;
    if events.is_empty() && !offline {
        events = prepared
            .client
            .get_events(
                prepared
                    .repo_ref
                    .relays
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                vec![filter],
            )
            .await?;
    }
    let source = events
        .into_iter()
        .find(|e| e.id == id && e.pubkey == coordinator)
        .context(
            "source Workflow Result unavailable; refresh CI status or retry without --offline",
        )?;
    source
        .verify()
        .context("invalid source Workflow Result signature")?;
    let parsed = ngit::ci::kinds::validate_workflow_result(&source)
        .context("invalid source Workflow Result")?;
    if !parsed
        .common
        .workflow
        .path
        .starts_with(".ngit/nix/workflows/")
    {
        bail!("current ngit-ci supports rerun only for native Nix workflows");
    }
    let resolved = validate_restart_source(&prepared, &parsed, job)?;
    let workflow = &parsed.common.workflow.path;
    let mut tags = restart_context_tags(&source, coordinator, id, job)?;
    tags.extend(selection_tags(
        &prepared.git_repo,
        &resolved,
        workflow,
        selection,
    )?);
    let event = prepared
        .sign(
            EventBuilder::new(KIND_CI_MANUAL_TRIGGER, "").tags(tags),
            "CI workflow restart",
        )
        .await?;
    validate_manual_trigger(&event).context("invalid workflow rerun request")?;
    let event_id = event.id;
    prepared.publish(vec![event], coordinator, offline).await?;
    if crate::output::is_json() {
        crate::output::set_value(
            serde_json::json!({"command_status": "ok", "entity": "ci", "action": "rerun-requested", "event": crate::output::event_id_to_nevent(event_id, prepared.repo_ref.relays.first()), "source": crate::output::event_id_to_nevent(id, prepared.repo_ref.relays.first()), "job": job, "workflow": workflow, "commit": resolved.commit, "coordinator": coordinator.to_bech32()?, "environments": selection.environment, "requirements": selection.requirements}),
        );
    }
    println!(
        "Rerun requested; the coordinator validates retained evidence and admission. Use ci status to observe the new attempt."
    );
    Ok(())
}

fn validate_restart_source(
    prepared: &Prepared,
    parsed: &ngit::ci::kinds::WorkflowResult,
    job: Option<&str>,
) -> Result<ResolvedCommit> {
    let coordinates = prepared.repo_ref.coordinates();
    if !parsed
        .common
        .repositories
        .iter()
        .any(|r| coordinates.iter().any(|c| c.coordinate == r.coordinate))
    {
        bail!("source Workflow Result belongs to a different repository");
    }
    let resolved = resolve_commit_ish_strict(&prepared.git_repo, parsed.common.commit())?;
    for oid in &parsed.common.commits {
        if resolve_commit_ish_strict(&prepared.git_repo, oid)?.commit != resolved.commit {
            bail!("source commit objects do not peel to the same commit");
        }
    }
    let workflow = &parsed.common.workflow.path;
    if workflow_blob_sha256(&prepared.git_repo, &resolved, workflow)?
        != parsed.common.workflow.sha256
    {
        bail!("source workflow hash does not match the source commit");
    }
    if let Some(job) = job {
        if !workflow_jobs(&prepared.git_repo, &resolved, workflow)?.contains(job) {
            bail!("restart job is absent from the source workflow");
        }
    }
    Ok(resolved)
}

/// Provision encrypted secret values without placing them on argv or in output.
pub async fn launch_secrets(
    args: &crate::cli::CiSecretsArgs,
    auth: SignerParams<'_>,
) -> Result<()> {
    use ngit::ci::secrets::{inbox, update};
    use nostr::prelude::{Filter, Timestamp};
    let values = read_secret_values(args)?;
    let coordinator = parse_coordinator(&args.coordinator)?;
    let mut prepared = Prepared::new(Repo::discover()?, false, auth).await?;
    if !prepared
        .repo_ref
        .confirmed_maintainers()
        .contains(&prepared.user_pubkey)
    {
        bail!("secret updates require a confirmed repository maintainer");
    }
    let mut advertisements = prepared
        .client
        .get_events(
            prepared
                .repo_ref
                .relays
                .iter()
                .map(ToString::to_string)
                .collect(),
            vec![Filter::new().kind(Kind::Custom(19843)).author(coordinator)],
        )
        .await?;
    advertisements
        .retain(|e| e.kind == Kind::Custom(19843) && e.pubkey == coordinator && e.verify().is_ok());
    let advertisement = advertisements.into_iter().max_by_key(|e| ngit::ci::total_order_position(e.created_at, e.id)).context("no signed coordinator advertisement found on repository relays; inspect ci inspect before retrying")?;
    let inbox = inbox(&advertisement, coordinator, Timestamp::now())?;
    let repository = Coordinate {
        kind: Kind::GitRepoAnnouncement,
        public_key: prepared.user_pubkey,
        identifier: prepared.repo_ref.identifier.clone(),
    };
    if !prepared
        .repo_ref
        .coordinates()
        .iter()
        .any(|c| c.coordinate == repository)
    {
        bail!("secret provisioning requires the signer's own confirmed repository announcement");
    }
    let confirmation_watch =
        SecretConfirmationWatch::new(&prepared, &inbox.relays, coordinator, &repository).await?;
    let relay_hint = prepared
        .repo_ref
        .relays
        .first()
        .map_or("", RelayUrl::as_str);
    let builder = update(
        prepared.user_pubkey,
        repository.clone(),
        relay_hint,
        &inbox,
        &values,
        &args.remove,
        Timestamp::now(),
    )?;
    let event = prepared.sign(builder, "encrypted CI secret update").await?;
    if Timestamp::now() >= inbox.expiration {
        bail!("coordinator secret key expired while signing; retry with a fresh advertisement");
    }
    let event_id = event.id;
    let update_time = event.created_at;
    let mut relays = inbox.relays.clone();
    for relay in &prepared.repo_ref.relays {
        if !relays.contains(relay) {
            relays.push(relay.clone());
        }
    }
    prepared.client.set_signer(prepared.signer.clone()).await;
    let outcomes = send_events_with_results(
        &prepared.client,
        Some(prepared.git_repo.get_path()?),
        vec![event],
        Vec::new(),
        relays,
        true,
        false,
    )
    .await?;
    if !outcomes.iter().any(|(relay, accepted)| {
        *accepted
            && inbox
                .relays
                .iter()
                .any(|inbox| inbox.as_str() == relay.as_str())
    }) {
        bail!(
            "no advertised secret inbox accepted the update; retry after checking coordinator connectivity"
        );
    }
    // Keep only names while waiting; plaintext has no further purpose.
    let set_names = values.0.keys().cloned().collect::<Vec<_>>();
    drop(values);
    let confirmation = confirmation_watch
        .wait(&prepared, args, update_time, &set_names)
        .await;
    confirmation_watch.report(&prepared, args, event_id, &set_names, confirmation)?;
    Ok(())
}

fn restart_context_tags(
    source: &Event,
    coordinator: PublicKey,
    id: nostr::prelude::EventId,
    job: Option<&str>,
) -> Result<Vec<Tag>> {
    let mut tags: Vec<Tag> = source
        .tags
        .iter()
        .filter(|t| {
            let t = t.as_slice();
            matches!(
                t[0].as_str(),
                "a" | "c"
                    | "w"
                    | "E"
                    | "K"
                    | "P"
                    | "e"
                    | "k"
                    | "issue"
                    | "release"
                    | "release-asset"
            ) || t[0] == "r" && t.get(1).is_some_and(|r| r.starts_with("refs/"))
        })
        .cloned()
        .collect();
    tags.push(Tag::public_key(coordinator));
    let mut restart = vec!["U".to_owned(), id.to_hex()];
    if let Some(job) = job {
        restart.push(job.into());
    }
    tags.push(Tag::parse(restart)?);
    Ok(tags)
}

fn read_secret_values(args: &crate::cli::CiSecretsArgs) -> Result<ngit::ci::secrets::SecretValues> {
    use ngit::ci::secrets::SecretValues;
    use zeroize::Zeroizing;
    let mut values = if let Some(path) = &args.set_file {
        use std::io::Read;
        let file = std::fs::File::open(path).context("failed to open secret values file")?;
        if !file.metadata()?.is_file() {
            bail!("secret values path must be a regular file");
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(65_536)
            .read_to_end(&mut bytes)
            .context("failed to read secret values file")?;
        if bytes.len() > 65_535 {
            bail!("secret values file exceeds the payload size limit");
        }
        serde_json::from_slice::<SecretValues>(&bytes).map_err(|_| {
            anyhow::anyhow!("secret values file must contain unique names and string values")
        })?
    } else {
        SecretValues::default()
    };
    for name in &args.set_from_env {
        if values.0.contains_key(name) {
            bail!("secret name supplied more than once");
        }
        let value = std::env::var(name).map_err(|_| {
            anyhow::anyhow!("a requested secret environment variable is missing or not UTF-8")
        })?;
        values.0.insert(name.clone(), value);
    }
    Ok(values)
}

/// Match gitworkshop's pending-change checks against a fresh signed inventory.
/// Removal has no per-name receipt: absence in a new snapshot is the available
/// evidence, not an acknowledgement of this exact update event ID.
fn secret_update_confirmed(
    status: &Event,
    baseline: &[nostr::prelude::EventId],
    author: PublicKey,
    created_at: nostr::prelude::Timestamp,
    set: &[String],
    remove: &[String],
    now: nostr::prelude::Timestamp,
) -> bool {
    if baseline.contains(&status.id)
        || status.created_at < created_at
        || status.tags.iter().any(|tag| {
            let tag = tag.as_slice();
            tag[0] == "expiration"
                && tag
                    .get(1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_none_or(|expiration| expiration <= now.as_secs())
        })
    {
        return false;
    }
    let inventory = status
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|tag| tag[0] == "secret")
        .collect::<Vec<_>>();
    set.iter().all(|name| {
        inventory.iter().any(|tag| {
            tag.get(1) == Some(name)
                && tag.get(2) == Some(&author.to_hex())
                && tag
                    .get(3)
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_some_and(|time| time >= created_at.as_secs())
        })
    }) && remove
        .iter()
        .all(|name| !inventory.iter().any(|tag| tag.get(1) == Some(name)))
}

struct SecretConfirmationWatch {
    relays: Vec<String>,
    filter: nostr::prelude::Filter,
    baseline: Vec<nostr::prelude::EventId>,
    coordinator: PublicKey,
    repository: String,
}

impl SecretConfirmationWatch {
    async fn new(
        prepared: &Prepared,
        inboxes: &[RelayUrl],
        coordinator: PublicKey,
        repository: &Coordinate,
    ) -> Result<Self> {
        use nostr::prelude::Filter;
        let mut status_relays = inboxes.iter().map(ToString::to_string).collect::<Vec<_>>();
        status_relays.extend(prepared.repo_ref.relays.iter().map(ToString::to_string));
        status_relays.sort();
        status_relays.dedup();
        let status_filter = Filter::new()
            .kind(Kind::Custom(39844))
            .author(coordinator)
            .custom_tag(
                nostr::prelude::SingleLetterTag::from_char('a')?,
                repository.to_string(),
            );
        let baseline = prepared
            .client
            .get_events(status_relays.clone(), vec![status_filter.clone()])
            .await?;
        Ok(Self {
            relays: status_relays,
            filter: status_filter,
            baseline: baseline.iter().map(|e| e.id).collect(),
            coordinator,
            repository: repository.to_string(),
        })
    }
    fn report(
        &self,
        prepared: &Prepared,
        args: &crate::cli::CiSecretsArgs,
        event_id: nostr::prelude::EventId,
        set_names: &[String],
        confirmation: Option<nostr::prelude::EventId>,
    ) -> Result<()> {
        let repository: Coordinate = self.repository.parse()?;
        if crate::output::is_json() {
            crate::output::set_value(
                serde_json::json!({"command_status": "ok", "entity": "ci", "action": "secret-update-published", "event": crate::output::event_id_to_nevent(event_id, prepared.repo_ref.relays.first()), "coordinator": self.coordinator.to_bech32()?, "repository": naddr(&repository, prepared.relay_hint()), "set": set_names, "remove": args.remove,
                "confirmation": if confirmation.is_some() { "accepted" } else { "pending" },
                "confirmation_event": confirmation.map(|id| id.to_hex())}),
            );
        }
        if confirmation.is_some() {
            println!("Coordinator confirmed the secret update.");
        } else {
            println!(
                "Encrypted secret update delivered; coordinator confirmation is still pending (not a rejection)."
            );
            println!(
                "Check with: ngit ci inspect --kind 39844 --author {} --json",
                self.coordinator.to_bech32()?
            );
        }
        Ok(())
    }

    async fn wait(
        &self,
        prepared: &Prepared,
        args: &crate::cli::CiSecretsArgs,
        update_time: nostr::prelude::Timestamp,
        set_names: &[String],
    ) -> Option<nostr::prelude::EventId> {
        use nostr::prelude::Timestamp;
        if args.confirmation_timeout == 0 {
            None
        } else {
            eprintln!("Encrypted secret update delivered; waiting for coordinator confirmation...");
            tokio::time::timeout(
                std::time::Duration::from_secs(args.confirmation_timeout),
                async {
                    loop {
                        if let Ok(events) = prepared
                            .client
                            .get_events(self.relays.clone(), vec![self.filter.clone()])
                            .await
                        {
                            let latest = events
                                .iter()
                                .filter(|event| {
                                    event.kind == Kind::Custom(39844)
                                        && event.pubkey == self.coordinator
                                        && event.verify().is_ok()
                                        && event.tags.iter().any(|tag| {
                                            let tag = tag.as_slice();
                                            tag[0] == "a" && tag.get(1) == Some(&self.repository)
                                        })
                                })
                                .max_by_key(|event| {
                                    ngit::ci::total_order_position(event.created_at, event.id)
                                });
                            if let Some(status) = latest {
                                if secret_update_confirmed(
                                    status,
                                    &self.baseline,
                                    prepared.user_pubkey,
                                    update_time,
                                    set_names,
                                    &args.remove,
                                    Timestamp::now(),
                                ) {
                                    return status.id;
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                },
            )
            .await
            .ok()
        }
    }
}

#[cfg(test)]
mod secret_confirmation_tests {
    use nostr::prelude::{Keys, Timestamp, event::FinalizeEvent};

    use super::*;

    #[test]
    fn confirmation_requires_fresh_inventory_and_matching_origin() {
        let keys = Keys::generate();
        let author = Keys::generate().public_key();
        let time = Timestamp::from_secs(100);
        let status = EventBuilder::new(Kind::Custom(39844), "")
            .custom_created_at(time)
            .tags([
                Tag::custom("secret", ["TOKEN".into(), author.to_hex(), "100".into()]),
                Tag::custom("expiration", ["200"]),
            ])
            .finalize(&keys)
            .unwrap();
        let set = vec!["TOKEN".to_owned()];
        let check = |baseline: &[nostr::prelude::EventId],
                     who,
                     at,
                     set: &[String],
                     remove: &[String],
                     now| {
            secret_update_confirmed(
                &status,
                baseline,
                who,
                Timestamp::from_secs(at),
                set,
                remove,
                Timestamp::from_secs(now),
            )
        };
        assert!(check(&[], author, 100, &set, &["OLD".into()], 100));
        assert!(!check(&[status.id], author, 100, &set, &[], 100));
        assert!(!check(&[], keys.public_key(), 100, &set, &[], 100));
        assert!(!check(&[], author, 101, &set, &[], 101));
        assert!(!check(&[], author, 100, &set, &[], 200));
        assert!(!check(&[], author, 100, &[], &set, 100));
        assert!(!check(&[], author, 100, &["OTHER".into()], &[], 100));
        assert!(check(&[], author, 100, &[], &["OLD".into()], 100));
    }
}
