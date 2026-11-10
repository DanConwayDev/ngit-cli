//! Event-first PR publication against real GRASP infrastructure.
//!
//! `GraspRecorder` places each relay acknowledgement and the first body bytes
//! of each receive-pack POST on one timeline. The tests therefore distinguish
//! event-before-push from the old implementation, which eventually produced
//! the same event and ref but only published after Git had accepted the ref.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures::StreamExt;
use nostr::nips::nip46::{NostrConnectEventBuilder, NostrConnectMessage, NostrConnectRequest};
use nostr_connect::signer::{
    NostrConnectKeys, NostrConnectRemoteSigner, NostrConnectSignerActions,
};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, GraspObservation, GraspRecorder, GraspRelayMode, Harness, KIND_PULL_REQUEST,
    PublishRepoOpts, tag_values,
};

const IDENTIFIER: &str = "event-first-pr";

struct Arrangement {
    harness: Harness,
    contributor: test_harness::Repo,
    contributor_keys: Keys,
    tip: String,
    maintainer_npub: String,
    maintainer_keys: Keys,
}

async fn arrange(identifier: &str, grasp06: bool) -> Result<Arrangement> {
    arrange_with_external(identifier, grasp06, false).await
}

async fn arrange_with_external(
    identifier: &str,
    grasp06: bool,
    include_external: bool,
) -> Result<Arrangement> {
    arrange_with_policy(identifier, grasp06, include_external, false).await
}

async fn arrange_with_policy(
    identifier: &str,
    grasp06: bool,
    include_external: bool,
    private: bool,
) -> Result<Arrangement> {
    let builder = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default");
    let builder = if grasp06 {
        builder.with_grasp_server_grasp06("repo")
    } else {
        builder.with_grasp_server("repo")
    };
    let builder = if include_external {
        builder.with_grasp_server_grasp06("external")
    } else {
        builder
    };
    let harness = builder.build().await?;
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("event-first maintainer".into()),
            identifier: Some(identifier.into()),
            private,
            ..Default::default()
        })
        .await?;
    let maintainer_keys = Keys::parse(
        &publisher
            .config("nostr.nsec")
            .await?
            .context("missing maintainer key")?,
    )?;
    let contributor = if private {
        // A private announcement cannot be cloned before credentials exist.
        // Use the authenticated maintainer worktree; the public cases below
        // retain the normal fresh-contributor clone.
        publisher
    } else {
        harness
            .clone_published_repo(
                &published,
                CloneLogin::AsContributor {
                    display_name: "event-first contributor".into(),
                },
            )
            .await?
    };
    contributor
        .git_ok(["checkout", "-b", "feature"], "create feature branch")
        .await?;
    std::fs::write(contributor.dir().join("feature.md"), "event first\n")?;
    contributor
        .git_ok(["add", "feature.md"], "stage feature")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "event first", "--no-gpg-sign"],
            "commit feature",
        )
        .await?;
    let tip = contributor.rev_parse("HEAD").await?;
    let nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("contributor nsec missing")?;
    let contributor_keys = Keys::parse(&nsec)?;
    Ok(Arrangement {
        harness,
        contributor,
        contributor_keys,
        tip,
        maintainer_npub: published.maintainer_npub,
        maintainer_keys,
    })
}

fn send_pr_command(
    arrangement: &Arrangement,
    git_server: &str,
    json: bool,
) -> tokio::process::Command {
    let mut args = vec![
        "send",
        "HEAD~1",
        "--force-pr",
        "--title",
        "event first",
        "--description",
        "stage the signed proposal before Git receive-pack",
        "--git-server",
        git_server,
    ];
    if json {
        args.push("--json");
    }
    arrangement.contributor.ngit(args)
}

async fn send_pr(
    arrangement: &Arrangement,
    git_server: &str,
    json: bool,
) -> Result<std::process::Output> {
    send_pr_command(arrangement, git_server, json)
        .output()
        .await
        .context("failed to run ngit send --force-pr")
}

fn command_output(output: &std::process::Output) -> String {
    format!(
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[derive(Default)]
struct RemoteSignRequests(AtomicUsize);

struct RejectProposalSignatures(Arc<RemoteSignRequests>);

impl NostrConnectSignerActions for RejectProposalSignatures {
    fn approve(&self, _public_key: &PublicKey, request: &NostrConnectRequest) -> bool {
        if matches!(request, NostrConnectRequest::SignEvent(_)) {
            self.0.0.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        true
    }
}

async fn wait_for_remote_signer(relay_url: &RelayUrl, signer_pubkey: PublicKey) -> Result<()> {
    let probe_keys = Keys::generate();
    let client = Client::default();
    client.add_relay(relay_url.clone()).await?;
    client.connect().await;
    client
        .subscribe(
            Filter::new()
                .pubkey(probe_keys.public_key())
                .kind(Kind::NostrConnect)
                .limit(0),
        )
        .await?;
    let mut notifications = client.notifications();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ping = NostrConnectEventBuilder::new(
            signer_pubkey,
            NostrConnectMessage::request(&NostrConnectRequest::Ping),
        )
        .finalize(&probe_keys)?;
        client.send_event(&ping).await?;
        let pong = tokio::time::timeout(Duration::from_millis(250), async {
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    if event.kind == Kind::NostrConnect && event.pubkey == signer_pubkey {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        if matches!(pong, Ok(true)) {
            client.disconnect().await;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            client.disconnect().await;
            bail!("remote signer did not answer a readiness probe before its deadline");
        }
    }
}

fn returned_event_id(output: &std::process::Output) -> Result<EventId> {
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report["command_status"] == "ok",
        "send did not report success: {report}"
    );
    ensure!(
        report["entity"] == "pr",
        "send did not identify a PR: {report}"
    );
    Ok(Nip19Event::from_bech32(
        report["id"]
            .as_str()
            .with_context(|| format!("send omitted its PR id: {report}"))?,
    )?
    .event_id)
}

async fn returned_pr(arrangement: &Arrangement, output: &std::process::Output) -> Result<Event> {
    let id = returned_event_id(output)?;
    arrangement
        .harness
        .grasp("repo")
        .events(Filter::new().id(id))
        .await?
        .into_iter()
        .find(|event| {
            event.id == id
                && event.kind == KIND_PULL_REQUEST
                && event.pubkey == arrangement.contributor_keys.public_key()
        })
        .with_context(|| format!("reported PR {id} not visible on the repository GRASP"))
}

async fn cached_id(arrangement: &Arrangement, id: EventId) -> Result<bool> {
    Ok(!ngit::client::get_events_from_local_cache(
        arrangement.contributor.dir(),
        vec![Filter::new().id(id)],
    )
    .await?
    .is_empty())
}

fn offered_id(observations: &[GraspObservation]) -> Result<EventId> {
    observations
        .iter()
        .find_map(|observation| match observation {
            GraspObservation::ProposalOffered(id) => Some(*id),
            _ => None,
        })
        .context("recorder saw no signed PR EVENT")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_event_precedes_receive_body_and_is_reused() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    // A direct GRASP-01 URL is origin-independent during receive-pack. Unlike
    // a proxied GRASP-06 /prs URL, its signed clone tag need not match the
    // upstream service's configured public origin.
    let explicit_clone = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = send_pr(&arrangement, &explicit_clone, true).await?;
    ensure!(
        output.status.success(),
        "send failed: {}",
        command_output(&output)
    );

    let event = returned_pr(&arrangement, &output).await?;
    event.verify()?;
    let observations = recorder.observations();
    let accepted = observations
        .iter()
        .position(
            |entry| matches!(entry, GraspObservation::ProposalAccepted(id) if *id == event.id),
        )
        .with_context(|| {
            format!("signed PR was not acknowledged on the paired relay: {observations:?}")
        })?;
    let receive_body = observations
        .iter()
        .position(|entry| matches!(entry, GraspObservation::ReceivePackBody))
        .context("recorder saw no receive-pack request body")?;
    assert!(
        accepted < receive_body,
        "relay acknowledgement must precede receive-pack body: {observations:?}"
    );
    assert_eq!(offered_id(&observations)?, event.id);

    assert_eq!(tag_values(&event, "clone"), vec![explicit_clone]);
    let id = event.id.to_hex();
    assert_eq!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref(&arrangement.maintainer_npub, IDENTIFIER, &id)
            .await?,
        arrangement.tip,
        "the later repository server must reuse the first signed event id"
    );

    // Early paired publication and any subsequent ordinary fanout all carry
    // this signed event when they target the same relay. Storage is idempotent.
    let all = arrangement
        .harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(arrangement.contributor_keys.public_key())
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    assert_eq!(
        all.iter()
            .filter(|candidate| candidate.id == event.id)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_git_acceptance_survives_total_late_fanout_failure() -> Result<()> {
    let mut arrangement = arrange_with_external(IDENTIFIER, true, true).await?;
    let external_url = arrangement.harness.grasp("external").url().to_string();
    drop(
        arrangement
            .harness
            .take_grasp("repo")
            .context("repository GRASP missing")?,
    );
    drop(
        arrangement
            .harness
            .take_relay("default")
            .context("default relay missing")?,
    );
    arrangement
        .contributor
        .set_relay_timeout(test_harness::RelayTimeout::ExpectedFailure);

    let output = send_pr(&arrangement, &external_url, true).await?;
    ensure!(
        output.status.success(),
        "early relay + Git acceptance became a false total failure: {}",
        command_output(&output)
    );
    let id = returned_event_id(&output)?;
    let event = arrangement
        .harness
        .grasp("external")
        .events(Filter::new().id(id))
        .await?
        .into_iter()
        .find(|event| event.id == id)
        .context("early accepted event is not present on its actual relay")?;
    event.verify()?;
    assert_eq!(
        tag_values(&event, "clone"),
        vec![format!(
            "{}/prs/{}/{}.git",
            external_url,
            arrangement.contributor_keys.public_key().to_bech32()?,
            IDENTIFIER
        )]
    );
    assert_eq!(
        arrangement
            .harness
            .grasp("external")
            .read_nostr_ref_prs(
                &arrangement.contributor_keys.public_key().to_hex(),
                IDENTIFIER,
                &id.to_hex(),
            )
            .await?,
        arrangement.tip
    );
    assert!(cached_id(&arrangement, id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_rejection_skips_git_and_uses_the_next_server() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), true).await?;
    let output = send_pr(&arrangement, recorder.url(), true).await?;
    ensure!(
        output.status.success(),
        "next primary did not succeed: {}",
        command_output(&output)
    );

    let observations = recorder.observations();
    let rejected = offered_id(&observations)?;
    assert!(
        observations.iter().any(
            |entry| matches!(entry, GraspObservation::ProposalRejected(id) if *id == rejected)
        ),
        "rejection was not recorded: {observations:?}"
    );
    assert!(
        !observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody)),
        "relay rejection must prevent receive-pack: {observations:?}"
    );

    let event = returned_pr(&arrangement, &output).await?;
    assert_ne!(event.id, rejected, "the rejected draft must not be reused");
    assert_eq!(
        tag_values(&event, "clone"),
        vec![format!(
            "{}/{}/{}.git",
            arrangement.harness.grasp("repo").url(),
            arrangement.maintainer_npub,
            IDENTIFIER
        )]
    );
    assert_eq!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref(&arrangement.maintainer_npub, IDENTIFIER, &event.id.to_hex())
            .await?,
        arrangement.tip
    );
    assert!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref_prs(
                &arrangement.contributor_keys.public_key().to_hex(),
                IDENTIFIER,
                &rejected.to_hex(),
            )
            .await
            .is_err(),
        "the rejected server has no GRASP-06 ref"
    );
    assert!(!cached_id(&arrangement, rejected).await?);
    assert!(cached_id(&arrangement, event.id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_proposal_signer_refusal_is_fatal_before_any_git_attempt() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    let signer_relay = RelayUrl::parse(arrangement.harness.relay("default").url())?;
    let remote_signer_keys = Keys::generate();
    let remote_signer = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: remote_signer_keys.clone(),
            user: arrangement.contributor_keys.clone(),
        },
        [signer_relay.clone()],
        None,
        None,
    )?;
    let bunker_uri = remote_signer.bunker_uri().to_string();
    let sign_requests = Arc::new(RemoteSignRequests::default());
    let signer_task = {
        let actions = RejectProposalSignatures(Arc::clone(&sign_requests));
        tokio::spawn(async move { remote_signer.serve(actions).await })
    };
    wait_for_remote_signer(&signer_relay, remote_signer_keys.public_key()).await?;

    let app_keys = Keys::generate();
    let app_key = app_keys.secret_key().to_secret_hex();
    let explicit_clone = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        arrangement
            .contributor
            .ngit([
                "--bunker-uri",
                &bunker_uri,
                "--bunker-app-key",
                &app_key,
                "send",
                "HEAD~1",
                "--force-pr",
                "--title",
                "remote signer refusal",
                "--description",
                "fatal before every Git candidate",
                "--git-server",
                &explicit_clone,
                "--json",
            ])
            .output(),
    )
    .await
    .context("remote-signer refusal was retried or parked")??;
    signer_task.abort();

    assert!(
        !output.status.success(),
        "signer refusal unexpectedly succeeded: {}",
        command_output(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("failed to sign event: Rejected: Rejected")
            && stderr.contains("0: Rejected")
            && stderr.contains("1: Rejected"),
        "underlying remote-signer rejection was not preserved: {}",
        command_output(&output)
    );
    assert_eq!(
        sign_requests.0.load(Ordering::SeqCst),
        1,
        "a fatal signer refusal must not request another candidate signature: {}",
        command_output(&output)
    );
    let observations = recorder.observations();
    assert!(
        observations.iter().all(|entry| !matches!(
            entry,
            GraspObservation::ProposalOffered(_) | GraspObservation::ReceivePackBody
        )),
        "signer refusal was followed by relay or Git publication: {observations:?}"
    );
    let cached = ngit::client::get_events_from_local_cache(
        arrangement.contributor.dir(),
        vec![
            Filter::new()
                .author(arrangement.contributor_keys.public_key())
                .kind(KIND_PULL_REQUEST),
        ],
    )
    .await?;
    assert!(
        cached.is_empty(),
        "refused proposal entered cache: {cached:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_event_with_failed_git_push_is_not_cached_or_reused() -> Result<()> {
    // The real relay accepts the candidate into purgatory, but this service
    // does not enable GRASP-06, so the corresponding /prs receive-pack fails.
    let arrangement = arrange(IDENTIFIER, false).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    let output = send_pr(&arrangement, recorder.url(), true).await?;
    ensure!(
        output.status.success(),
        "fallback Git push failed: {}",
        command_output(&output)
    );

    let observations = recorder.observations();
    let staged = offered_id(&observations)?;
    assert!(
        observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalAccepted(id) if *id == staged)),
        "candidate was not accepted before its failed Git push: {observations:?}"
    );
    // GRASP-06 is disabled, so receive-pack discovery fails before request
    // body bytes exist. A fresh accepted id below proves that this staged
    // candidate's Git attempt did not become the successful draft.

    let event = returned_pr(&arrangement, &output).await?;
    assert_ne!(
        event.id, staged,
        "successful fallback needs its fresh clone URL and id"
    );
    assert_eq!(
        tag_values(&event, "clone"),
        vec![format!(
            "{}/{}/{}.git",
            arrangement.harness.grasp("repo").url(),
            arrangement.maintainer_npub,
            IDENTIFIER
        )]
    );
    assert_eq!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref(&arrangement.maintainer_npub, IDENTIFIER, &event.id.to_hex())
            .await?,
        arrangement.tip
    );
    assert!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref_prs(
                &arrangement.contributor_keys.public_key().to_hex(),
                IDENTIFIER,
                &staged.to_hex(),
            )
            .await
            .is_err(),
        "GRASP-06 was disabled, so no fallback ref should exist there"
    );
    assert!(!cached_id(&arrangement, staged).await?);
    assert!(cached_id(&arrangement, event.id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_relay_is_never_used_for_restricted_repositories() -> Result<()> {
    for setting in ["nostr.repo-relay-only", "nostr.private"] {
        let announced_private = setting == "nostr.private";
        let arrangement = arrange_with_policy(IDENTIFIER, true, false, announced_private).await?;
        if !announced_private {
            arrangement
                .contributor
                .git_ok(["config", "--local", setting, "true"], "restrict relays")
                .await?;
        }
        let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
        let explicit_clone = format!(
            "{}/{}/{}.git",
            recorder.url(),
            arrangement.maintainer_npub,
            IDENTIFIER
        );
        let output = send_pr(&arrangement, &explicit_clone, true).await?;
        ensure!(
            output.status.success(),
            "{setting} push failed: {}",
            command_output(&output)
        );
        let observations = recorder.observations();
        assert!(
            !observations
                .iter()
                .any(|entry| matches!(entry, GraspObservation::ProposalOffered(_))),
            "{setting} leaked an event to the undeclared pair: {observations:?}\n{}",
            command_output(&output)
        );
        assert!(
            observations
                .iter()
                .any(|entry| matches!(entry, GraspObservation::ReceivePackBody)),
            "{setting} incorrectly blocked the Git override: {observations:?}"
        );
        let event = returned_pr(&arrangement, &output).await?;
        event.verify()?;
        assert!(cached_id(&arrangement, event.id).await?);
        let default_events = arrangement
            .harness
            .relay("default")
            .events(
                Filter::new()
                    .author(arrangement.contributor_keys.public_key())
                    .kind(KIND_PULL_REQUEST),
            )
            .await?;
        assert!(
            default_events.is_empty(),
            "{setting} broadened publication to non-repository relays: {default_events:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_paired_relay_skips_live_git_and_uses_fresh_draft() -> Result<()> {
    let mut arrangement = arrange(IDENTIFIER, true).await?;
    arrangement
        .contributor
        .set_relay_timeout(test_harness::RelayTimeout::ExpectedFailure);
    let recorder = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        GraspRelayMode::Unreachable,
    )
    .await?;
    let explicit_clone = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = send_pr(&arrangement, &explicit_clone, true).await?;
    ensure!(
        output.status.success(),
        "fallback did not survive an unreachable paired relay: {}",
        command_output(&output)
    );
    let observations = recorder.observations();
    assert!(
        !observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalOffered(_))),
        "unreachable relay unexpectedly received an offer: {observations:?}"
    );
    assert!(
        !observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody)),
        "Git remained live, so early relay failure must skip receive-pack: {observations:?}"
    );
    let event = returned_pr(&arrangement, &output).await?;
    assert_eq!(
        tag_values(&event, "clone"),
        vec![format!(
            "{}/{}/{}.git",
            arrangement.harness.grasp("repo").url(),
            arrangement.maintainer_npub,
            IDENTIFIER
        )],
        "the served fallback must generate its own clone tag, not reuse the failed candidate"
    );
    assert!(cached_id(&arrangement, event.id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_paired_relay_failing_preserves_failure_and_empty_cache() -> Result<()> {
    let mut arrangement = arrange(IDENTIFIER, true).await?;
    arrangement
        .contributor
        .set_relay_timeout(test_harness::RelayTimeout::ExpectedFailure);
    let recorder = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        GraspRelayMode::RejectAfterGate,
    )
    .await?;
    let send = tokio::spawn(send_pr_command(&arrangement, recorder.url(), true).output());
    recorder.wait_for_proposal_offer().await?;
    let upstream = arrangement
        .harness
        .take_grasp("repo")
        .context("repository GRASP missing")?;
    drop(upstream);
    recorder.release_rejection();
    let output = send.await??;
    let rejected = offered_id(&recorder.observations())?;
    assert!(
        recorder.observations().iter().any(
            |entry| matches!(entry, GraspObservation::ProposalRejected(id) if *id == rejected)
        ),
        "reachable relay did not reject the observed candidate"
    );
    assert!(
        !output.status.success(),
        "all rejected servers unexpectedly succeeded"
    );
    let stdout = String::from_utf8(output.stdout)?;
    if !stdout.trim().is_empty() {
        let report: serde_json::Value = serde_json::from_str(&stdout)?;
        assert_ne!(
            report["command_status"], "ok",
            "failed send reported JSON ok"
        );
    }
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("failed to push PR"),
        "existing CLI failure context was lost: {stderr}"
    );
    assert!(
        stderr.contains("event-first relay publication failure"),
        "aggregate failure hid the event-first relay reason: {stderr}"
    );
    assert!(
        recorder
            .observations()
            .iter()
            .all(|entry| !matches!(entry, GraspObservation::ReceivePackBody)),
        "a server without relay acceptance received Git data"
    );
    let cached = ngit::client::get_events_from_local_cache(
        arrangement.contributor.dir(),
        vec![
            Filter::new()
                .author(arrangement.contributor_keys.public_key())
                .kind(KIND_PULL_REQUEST),
        ],
    )
    .await?;
    assert!(
        cached.is_empty(),
        "failed signed drafts entered the accepted cache: {cached:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hint_unlocks_all_relays_before_replica_git() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let first = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    let second = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        GraspRelayMode::RejectAfterGate,
    )
    .await?;
    let clone_url =
        |base: &str| format!("{base}/{}/{}.git", arrangement.maintainer_npub, IDENTIFIER);
    let first_clone = clone_url(first.url());
    let second_clone = clone_url(second.url());
    // Advertise the gated proxy as the repository destination; the explicit
    // candidate is the accepting proxy. Both forward to the real repository.
    let announcements = arrangement
        .harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitRepoAnnouncement)
                .author(arrangement.maintainer_keys.public_key()),
        )
        .await?;
    let original = announcements
        .iter()
        .max_by_key(|e| e.created_at)
        .context("missing announcement")?;
    let mut tags: Vec<Tag> = original
        .tags
        .iter()
        .filter(|tag| tag.kind() != "clone")
        .cloned()
        .collect();
    tags.push(Tag::parse(["clone", second_clone.as_str()])?);
    // A GRASP pair is identified by matching clone and relay origins.
    let second_relay = second.url().replacen("http://", "ws://", 1);
    let mut relays = vec![
        "relays".to_string(),
        arrangement.harness.grasp("repo").relay_url(),
        second_relay,
    ];
    tags.retain(|tag| tag.kind() != "relays");
    tags.push(Tag::parse(std::mem::take(&mut relays))?);
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, original.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(original.created_at.as_secs() + 1))
        .finalize(&arrangement.maintainer_keys)?;
    let client = Client::default();
    client
        .add_relay(arrangement.harness.relay("default").url())
        .await?;
    client.connect().await;
    client.send_event(&announcement).await?;
    client.shutdown().await;
    ngit::client::save_event_in_local_cache(arrangement.contributor.dir(), &announcement).await?;

    let mut command = send_pr_command(&arrangement, &first_clone, true);
    command.kill_on_drop(true);
    let pending = command.output();
    tokio::pin!(pending);
    tokio::select! {
        output = &mut pending => bail!("send finished before the admission gate: {}", command_output(&output?)),
        result = second.wait_for_proposal_offer() => result?,
    }
    assert!(
        first
            .observations()
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody)),
        "the hint Git push must succeed before admitting replicas"
    );
    // The replica is deliberately stalled in admission. Ordinary relays must
    // nevertheless receive the PR now, without waiting for its Git transfer.
    // This publication is genuinely asynchronous with the gated relay.
    let id = offered_id(&second.observations())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = arrangement
                .harness
                .relay("default")
                .events(Filter::new().id(id))
                .await?;
            if events.iter().any(|event| event.id == id) {
                break anyhow::Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("ordinary relay waited for replica admission/Git")??;
    assert!(
        !second
            .observations()
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody))
    );
    second.release_rejection();
    let output = tokio::time::timeout(Duration::from_secs(60), pending).await??;
    ensure!(output.status.success(), "{}", command_output(&output));
    let event = returned_pr(&arrangement, &output).await?;
    assert_eq!(tag_values(&event, "clone"), vec![first_clone]);
    assert_eq!(offered_id(&first.observations())?, event.id);
    assert_eq!(offered_id(&second.observations())?, event.id);
    assert_eq!(
        second
            .observations()
            .iter()
            .filter(|entry| matches!(entry, GraspObservation::ProposalOffered(_)))
            .count(),
        1,
        "final fanout must not repeat a rejected GRASP admission"
    );
    assert!(
        first
            .observations()
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody))
    );
    assert!(
        !second
            .observations()
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintainer_replicates_to_ordinary_git_only_after_grasp_success() -> Result<()> {
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_vanilla_git_server("replica")
    .build()
    .await?;
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;
    let replica_url = format!(
        "{}/replica.git",
        harness.vanilla_git_server("replica").url()
    );
    let announcements = harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitRepoAnnouncement)
                .author(published.maintainer_keys.public_key()),
        )
        .await?;
    let original = announcements
        .iter()
        .max_by_key(|e| e.created_at)
        .context("missing announcement")?;
    let mut tags: Vec<Tag> = original
        .tags
        .iter()
        .filter(|tag| tag.kind() != "clone")
        .cloned()
        .collect();
    let mut clone_tags = vec!["clone".to_string()];
    clone_tags.extend(tag_values(original, "clone"));
    clone_tags.push(replica_url);
    tags.push(Tag::parse(clone_tags)?);
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, original.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(original.created_at.as_secs() + 1))
        .finalize(&published.maintainer_keys)?;
    ngit::client::save_event_in_local_cache(publisher.dir(), &announcement).await?;
    publisher
        .git_ok(["checkout", "-b", "feature"], "create feature branch")
        .await?;
    std::fs::write(
        publisher.dir().join("feature.md"),
        "replicate after admission\n",
    )?;
    publisher
        .git_ok(["add", "feature.md"], "stage feature")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "feature", "--no-gpg-sign"],
            "commit feature",
        )
        .await?;
    let output = publisher
        .ngit([
            "send",
            "HEAD~1",
            "--force-pr",
            "--title",
            "replication",
            "--description",
            "phase three",
            "--json",
        ])
        .output()
        .await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let id = returned_event_id(&output)?;
    assert_eq!(
        harness
            .vanilla_git_server("replica")
            .read_nostr_ref(&id.to_hex())
            .await?,
        publisher.rev_parse("HEAD").await?
    );
    let ref_count = || -> Result<usize> {
        Ok(
            git2::Repository::open_bare(harness.vanilla_git_server("replica").repo_path())?
                .references()?
                .count(),
        )
    };
    let before = ref_count()?;
    let replica_path = harness.vanilla_git_server("replica").repo_path().to_owned();
    drop(harness.take_grasp("repo").context("missing GRASP")?);
    publisher
        .git_ok(
            [
                "commit",
                "--allow-empty",
                "-m",
                "another proposal",
                "--no-gpg-sign",
            ],
            "advance proposal",
        )
        .await?;
    let output = publisher
        .ngit([
            "send",
            "HEAD~1",
            "--force-pr",
            "--title",
            "unavailable",
            "--description",
            "no replication",
            "--json",
        ])
        .output()
        .await?;
    assert!(
        !output.status.success(),
        "unavailable GRASP must fail admission"
    );
    assert_eq!(
        git2::Repository::open_bare(replica_path)?
            .references()?
            .count(),
        before,
        "failed GRASP admission must not replicate a new ref to ordinary Git"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_replaces_the_hint_when_first_git_endpoint_rejects() -> Result<()> {
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_grasp_server("no_prs")
    .with_grasp_server_grasp06("fallback")
    .build()
    .await?;
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "fallback phase contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(["checkout", "-b", "feature"], "create feature branch")
        .await?;
    std::fs::write(contributor.dir().join("feature.md"), "fallback phase\n")?;
    contributor
        .git_ok(["add", "feature.md"], "stage feature")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "feature", "--no-gpg-sign"],
            "commit feature",
        )
        .await?;
    drop(harness.take_grasp("repo").context("missing repo GRASP")?);
    let output = contributor
        .ngit([
            "send",
            "HEAD~1",
            "--force-pr",
            "--title",
            "fallback",
            "--description",
            "select a working hint",
            "--json",
        ])
        .env(
            "NGIT_GRASP_DEFAULT_SET",
            format!(
                "{};{}",
                harness.grasp("no_prs").url(),
                harness.grasp("fallback").url()
            ),
        )
        .output()
        .await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let id = returned_event_id(&output)?;
    let event = harness
        .grasp("fallback")
        .events(Filter::new().id(id))
        .await?
        .into_iter()
        .next()
        .context("fallback event not published")?;
    let hints = tag_values(&event, "clone");
    assert_eq!(hints.len(), 1);
    assert!(hints[0].starts_with(harness.grasp("fallback").url()));
    assert_eq!(
        harness
            .grasp("fallback")
            .read_nostr_ref_prs(&event.pubkey.to_hex(), IDENTIFIER, &id.to_hex())
            .await?,
        contributor.rev_parse("HEAD").await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_update_is_newer_than_the_unserved_attempt() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, false).await?;
    let output = send_pr(&arrangement, arrangement.harness.grasp("repo").url(), true).await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let original = returned_pr(&arrangement, &output).await?;
    // A future reference makes same-timestamp retries deterministic without sleeps.
    let root = EventBuilder::new(original.kind, original.content.clone())
        .tags(original.tags.iter().cloned())
        .custom_created_at(Timestamp::from_secs(Timestamp::now().as_secs() + 60))
        .finalize(&arrangement.contributor_keys)?;
    ngit::client::save_event_in_local_cache(arrangement.contributor.dir(), &root).await?;
    arrangement
        .contributor
        .git_ok(
            ["commit", "--allow-empty", "-m", "update", "--no-gpg-sign"],
            "commit update",
        )
        .await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    let mut command = send_pr_command(&arrangement, recorder.url(), true);
    command.args(["--in-reply-to", &root.id.to_hex()]);
    let output = command.output().await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let attempted = recorder
        .observations()
        .iter()
        .find_map(|entry| match entry {
            GraspObservation::ProposalCreatedAt(id, timestamp) => Some((*id, *timestamp)),
            _ => None,
        })
        .context("missing attempted update")?;
    let updates = ngit::client::get_events_from_local_cache(
        arrangement.contributor.dir(),
        vec![Filter::new().kind(test_harness::KIND_PULL_REQUEST_UPDATE)],
    )
    .await?;
    let successful = updates
        .iter()
        .filter(|event| tag_values(event, "E").contains(&root.id.to_hex()))
        .max_by_key(|event| event.created_at)
        .context("missing successful update")?;
    assert!(
        successful.created_at.as_secs() > attempted.1,
        "successful revision must outrank the relay-retained failed hint"
    );
    assert!(!cached_id(&arrangement, attempted.0).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replica_git_failure_does_not_suppress_patch_close() -> Result<()> {
    let arrangement = arrange_with_external(IDENTIFIER, false, true).await?;
    let output = arrangement
        .contributor
        .ngit([
            "send",
            "HEAD~1",
            "--force-patch",
            "--title",
            "original patch",
            "--description",
            "convert this patch to a PR",
        ])
        .output()
        .await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let patches = arrangement
        .harness
        .relay("default")
        .events(
            Filter::new()
                .kind(Kind::GitPatch)
                .author(arrangement.contributor_keys.public_key()),
        )
        .await?;
    let patch = patches
        .iter()
        .find(|event| ngit::git_events::event_is_patch_set_root(event))
        .context("missing patch root")?;
    let announcements = arrangement
        .harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitRepoAnnouncement)
                .author(arrangement.maintainer_keys.public_key()),
        )
        .await?;
    let original = announcements
        .iter()
        .max_by_key(|event| event.created_at)
        .context("missing announcement")?;
    let replica = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        GraspRelayMode::RejectGit,
    )
    .await?;
    let unavailable_clone = format!(
        "{}/{}/{}.git",
        replica.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let replica_relay = replica.url().replacen("http://", "ws://", 1);
    let mut tags: Vec<Tag> = original
        .tags
        .iter()
        .filter(|tag| tag.kind() != "clone" && tag.kind() != "relays")
        .cloned()
        .collect();
    tags.push(Tag::parse(["clone", unavailable_clone.as_str()])?);
    tags.push(Tag::parse(["relays", replica_relay.as_str()])?);
    let announcement = EventBuilder::new(original.kind, original.content.clone())
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(original.created_at.as_secs() + 1))
        .finalize(&arrangement.maintainer_keys)?;
    ngit::client::save_event_in_local_cache(arrangement.contributor.dir(), &announcement).await?;
    let mut command = send_pr_command(
        &arrangement,
        arrangement.harness.grasp("external").url(),
        true,
    );
    command.args(["--in-reply-to", &patch.id.to_hex()]);
    let output = command.output().await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    assert!(
        replica
            .observations()
            .contains(&GraspObservation::GitRequestRejected),
        "fixture did not exercise replica Git failure"
    );
    assert!(
        replica
            .observations()
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalAccepted(_))),
        "fixture must accept the PR before rejecting Git"
    );
    let statuses = arrangement
        .harness
        .grasp("repo")
        .events(
            Filter::new()
                .kind(Kind::GitStatusClosed)
                .author(arrangement.contributor_keys.public_key()),
        )
        .await?;
    assert!(
        statuses
            .iter()
            .any(|event| tag_values(event, "e").contains(&patch.id.to_hex())),
        "replica Git failure suppressed the original patch's close status"
    );
    Ok(())
}

// Seed a cache written by an earlier CLI process; no repository-local state is
// involved. Old entries remain authoritative while a refresh is attempted.
fn cached_pyramid(relay: &RelayUrl, fetched_at: u64) -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let cache = directory.path().join("relay-information");
    std::fs::create_dir_all(&cache)?;
    let key = bitcoin_hashes::sha256::Hash::hash(relay.as_str().as_bytes());
    std::fs::write(
        cache.join(format!("{key}.json")),
        serde_json::to_vec(&serde_json::json!({
            "relay": relay.as_str(),
            "fetched_at": fetched_at,
            "attempted_at": fetched_at,
            "document": {
                "software": "https://github.com/fiatjaf/pyramid",
                "version": "v1.4.4"
            }
        }))?,
    )?;
    Ok(directory)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_cached_pyramid_hint_pushes_git_before_publishing() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    let relay = RelayUrl::parse(&recorder.url().replacen("http://", "ws://", 1))?;
    let cache = cached_pyramid(&relay, 1)?;
    let clone_url = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = send_pr_command(&arrangement, &clone_url, true)
        .env("NGIT_CACHE_DIR", cache.path())
        .output()
        .await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let event = returned_pr(&arrangement, &output).await?;
    let observations = recorder.observations();
    let git = observations
        .iter()
        .position(|entry| matches!(entry, GraspObservation::ReceivePackBody))
        .context("no Git transfer")?;
    let offered = observations
        .iter()
        .position(|entry| matches!(entry, GraspObservation::ProposalOffered(id) if *id == event.id))
        .context("no PR publication")?;
    assert!(
        git < offered,
        "cached Pyramid hint must be served before publication: {observations:?}"
    );
    assert_eq!(tag_values(&event, "clone"), vec![clone_url]);
    assert_eq!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref(&arrangement.maintainer_npub, IDENTIFIER, &event.id.to_hex())
            .await?,
        arrangement.tip
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_pyramid_git_failure_never_publishes_an_abandoned_hint() -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let recorder = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        GraspRelayMode::RejectGit,
    )
    .await?;
    let relay = RelayUrl::parse(&recorder.url().replacen("http://", "ws://", 1))?;
    let cache = cached_pyramid(&relay, Timestamp::now().as_secs())?;
    let clone_url = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = send_pr_command(&arrangement, &clone_url, true)
        .env("NGIT_CACHE_DIR", cache.path())
        .output()
        .await?;
    ensure!(output.status.success(), "{}", command_output(&output));
    let observations = recorder.observations();
    assert!(observations.contains(&GraspObservation::GitRequestRejected));
    assert!(
        !observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalOffered(_))),
        "failed Pyramid hint leaked an abandoned PR: {observations:?}"
    );
    let event = returned_pr(&arrangement, &output).await?;
    assert!(!tag_values(&event, "clone").contains(&clone_url));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pyramid_signer_refusal_follows_successful_git_and_stops_publication() -> Result<()> {
    pyramid_signing_waits_for_git(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_pyramid_hint_is_tried_before_requesting_a_signature() -> Result<()> {
    pyramid_signing_waits_for_git(true).await
}

async fn pyramid_signing_waits_for_git(reject_git: bool) -> Result<()> {
    let arrangement = arrange(IDENTIFIER, true).await?;
    let recorder = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        if reject_git {
            GraspRelayMode::RejectGit
        } else {
            GraspRelayMode::Forward
        },
    )
    .await?;
    let relay = RelayUrl::parse(&recorder.url().replacen("http://", "ws://", 1))?;
    let cache = cached_pyramid(&relay, Timestamp::now().as_secs())?;
    let signer_relay = RelayUrl::parse(arrangement.harness.relay("default").url())?;
    let remote_signer_keys = Keys::generate();
    let remote_signer = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: remote_signer_keys.clone(),
            user: arrangement.contributor_keys.clone(),
        },
        [signer_relay.clone()],
        None,
        None,
    )?;
    let bunker_uri = remote_signer.bunker_uri().to_string();
    let sign_requests = Arc::new(RemoteSignRequests::default());
    let actions = RejectProposalSignatures(Arc::clone(&sign_requests));
    let signer_task = tokio::spawn(async move { remote_signer.serve(actions).await });
    wait_for_remote_signer(&signer_relay, remote_signer_keys.public_key()).await?;
    let app_keys = Keys::generate();
    let clone_url = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        arrangement
            .contributor
            .ngit([
                "--bunker-uri",
                &bunker_uri,
                "--bunker-app-key",
                &app_keys.secret_key().to_secret_hex(),
                "send",
                "HEAD~1",
                "--force-pr",
                "--title",
                "deferred signing",
                "--description",
                "sign after the Pyramid hint works",
                "--git-server",
                &clone_url,
                "--json",
            ])
            .env("NGIT_CACHE_DIR", cache.path())
            .output(),
    )
    .await??;
    signer_task.abort();
    assert!(!output.status.success(), "signer refusal must remain fatal");
    assert_eq!(
        sign_requests.0.load(Ordering::SeqCst),
        1,
        "failed Pyramid hints must not request a signature; a later refusal must not retry"
    );
    let observations = recorder.observations();
    assert!(
        observations.contains(if reject_git {
            &GraspObservation::GitRequestRejected
        } else {
            &GraspObservation::ReceivePackBody
        }),
        "signing ran before Git: {observations:?}"
    );
    assert!(
        !observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalOffered(_))),
        "refused proposal was published: {observations:?}"
    );
    let cached = ngit::client::get_events_from_local_cache(
        arrangement.contributor.dir(),
        vec![
            Filter::new()
                .author(arrangement.contributor_keys.public_key())
                .kind(KIND_PULL_REQUEST),
        ],
    )
    .await?;
    assert!(
        cached.is_empty(),
        "unsigned or refused proposal entered cache"
    );
    Ok(())
}
