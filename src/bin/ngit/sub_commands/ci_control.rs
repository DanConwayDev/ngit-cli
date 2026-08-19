//! `ngit ci request|stop|trigger` — the maintainer controls that publish the
//! Level 1 evidence `ngit ci status` consumes.
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
//! a refusal.

use std::path::Path;

use anyhow::{Context, Result, bail};
use bitcoin_hashes::sha256;
use ngit::{
    ci::kinds::{
        KIND_CI_MANUAL_TRIGGER, KIND_CI_SERVICE_REQUEST, KIND_CI_SERVICE_STOP,
        validate_manual_trigger, validate_service_control,
    },
    client::{Params, get_repo_ref_from_cache, get_state_from_cache, send_events, sign_event},
    login::user::get_user_details,
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
    repo_ref::get_repo_coordinates_for_publishing,
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
            "status": "ok",
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
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
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
        // No relay hint: the NIP grants one to a Service Request's `a` tag,
        // not to the common tags, and a coordinator that parses the common
        // tags strictly drops a Manual Trigger whose `a` has a third
        // element. A hint here would buy nothing anyway — the coordinator
        // already watches the repository it serves.
        tags.push(Tag::coordinate(coordinate, None));
    }
    for commit in &resolved.ids {
        tags.push(Tag::parse(["c", commit])?);
    }
    tags.push(Tag::parse(["w", workflow, &workflow_hash])?);
    if let Some(git_ref) = git_ref {
        tags.push(Tag::parse(["r", git_ref])?);
    }

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
            "status": "ok",
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
        let mut repo_coordinates =
            get_repo_coordinates_for_publishing(&git_repo, &mut client).await?;

        if !offline {
            fetching_with_account(
                &git_repo,
                git_repo_path,
                &mut client,
                &mut repo_coordinates,
                auth,
            )
            .await?;
        }

        let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

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
        for maintainer in self.repo_ref.maintainers_for_announcement_tags() {
            let has_announcement = self.repo_ref.events.keys().any(|coordinate| {
                coordinate.public_key == maintainer
                    && coordinate.identifier == self.repo_ref.identifier
            });
            if has_announcement && maintainer != perspective.public_key {
                coordinates.push(Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: maintainer,
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
        let outcomes = send_events(
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
