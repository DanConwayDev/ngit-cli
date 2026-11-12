//! Event-first ordering and publication outcomes specific to `git push pr/...`.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, GraspObservation, GraspRecorder, GraspRelayMode, Harness, KIND_PULL_REQUEST,
    PublishRepoOpts, Repo, tag_values,
};

const IDENTIFIER: &str = "helper-event-first-pr";

struct Arrangement {
    harness: Harness,
    contributor: Repo,
    contributor_keys: Keys,
    maintainer_npub: String,
    tip: String,
}

async fn arrange(external: bool) -> Result<Arrangement> {
    let builder = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server_grasp06("repo");
    let builder = if external {
        builder.with_grasp_server_grasp06("external")
    } else {
        builder
    };
    let harness = builder.build().await?;
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
                display_name: "helper event-first contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(["checkout", "-b", "pr/feature"], "create proposal branch")
        .await?;
    std::fs::write(contributor.dir().join("feature.md"), "helper event first\n")?;
    contributor
        .git_ok(["add", "feature.md"], "stage proposal")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "helper event first", "--no-gpg-sign"],
            "commit proposal",
        )
        .await?;
    let tip = contributor.rev_parse("HEAD").await?;
    let contributor_keys = Keys::parse(
        &contributor
            .config("nostr.nsec")
            .await?
            .context("missing contributor nsec")?,
    )?;
    Ok(Arrangement {
        harness,
        contributor,
        contributor_keys,
        maintainer_npub: published.maintainer_npub,
        tip,
    })
}

fn push_option(url: &str) -> String {
    format!("git-server={url}")
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn first_accepted(observations: &[GraspObservation]) -> Result<EventId> {
    observations
        .iter()
        .find_map(|observation| match observation {
            GraspObservation::ProposalAccepted(id) => Some(*id),
            _ => None,
        })
        .context("helper PR was not accepted by the paired relay")
}

async fn cached_id(repo: &Repo, event_id: EventId) -> Result<bool> {
    Ok(
        !ngit::client::get_events_from_local_cache(repo.dir(), vec![Filter::new().id(event_id)])
            .await?
            .is_empty(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_publishes_signed_pr_before_push_with_matching_ref_and_cache() -> Result<()> {
    let arrangement = arrange(false).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("repo"), false).await?;
    let clone = format!(
        "{}/{}/{}.git",
        recorder.url(),
        arrangement.maintainer_npub,
        IDENTIFIER
    );
    let output = arrangement
        .contributor
        .nostr_push(["-u", "origin", "pr/feature", "-o", &push_option(&clone)])
        .await?;
    assert!(
        output.status.success(),
        "helper proposal push failed: {}",
        output_text(&output)
    );
    assert_eq!(
        arrangement
            .contributor
            .snapshot()?
            .refs
            .get("refs/remotes/origin/pr/feature"),
        Some(&arrangement.tip),
        "Git did not record the helper's accepted proposal ref"
    );
    let observations = recorder.observations();
    let id = first_accepted(&observations)?;
    let accepted = observations
        .iter()
        .position(|entry| matches!(entry, GraspObservation::ProposalAccepted(event_id) if *event_id == id))
        .context("accepted event missing from recorder")?;
    let receive_body = observations
        .iter()
        .position(|entry| matches!(entry, GraspObservation::ReceivePackBody))
        .context("helper sent no receive-pack body")?;
    assert!(
        accepted < receive_body,
        "helper ordering was Git-first: {observations:?}"
    );
    let event = arrangement
        .harness
        .grasp("repo")
        .events(Filter::new().id(id))
        .await?
        .into_iter()
        .find(|event| event.id == id)
        .context("helper event missing from repository relay")?;
    event.verify()?;
    assert_eq!(tag_values(&event, "clone"), vec![clone]);
    assert_eq!(
        arrangement
            .harness
            .grasp("repo")
            .read_nostr_ref(&arrangement.maintainer_npub, IDENTIFIER, &id.to_hex())
            .await?,
        arrangement.tip
    );
    assert!(cached_id(&arrangement.contributor, id).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_relay_rejection_never_reports_false_ok() -> Result<()> {
    let mut arrangement = arrange(false).await?;
    arrangement
        .contributor
        .set_relay_timeout(test_harness::RelayTimeout::ExpectedFailure);
    let recorder = GraspRecorder::start_with_mode(
        arrangement.harness.grasp("repo"),
        GraspRelayMode::RejectAfterGate,
    )
    .await?;
    let mut command = arrangement.contributor.git([
        "push",
        "origin",
        "pr/feature",
        "-o",
        &push_option(recorder.url()),
    ]);
    let output_task = tokio::spawn(async move { command.output().await });
    recorder.wait_for_proposal_offer().await?;
    drop(
        arrangement
            .harness
            .take_grasp("repo")
            .context("repository GRASP missing")?,
    );
    recorder.release_rejection();
    let output = output_task.await??;
    assert!(!output.status.success());
    let rendered = output_text(&output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        !stdout.contains("ok refs/heads/pr/feature"),
        "helper emitted ok after early relay rejection: {rendered}"
    );
    let observations = recorder.observations();
    assert!(
        observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalOffered(_)))
    );
    assert!(
        observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ProposalRejected(_)))
    );
    assert!(
        !observations
            .iter()
            .any(|entry| matches!(entry, GraspObservation::ReceivePackBody))
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
    assert!(cached.is_empty(), "rejected helper proposal entered cache");
    assert!(
        !arrangement
            .contributor
            .snapshot()?
            .refs
            .contains_key("refs/remotes/origin/pr/feature"),
        "git trusted a false helper ok"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_reports_ok_when_early_pair_is_only_successful_publication() -> Result<()> {
    let mut arrangement = arrange(true).await?;
    let recorder = GraspRecorder::start(arrangement.harness.grasp("external"), false).await?;
    let push_option = push_option(recorder.url());
    arrangement
        .contributor
        .set_relay_timeout(test_harness::RelayTimeout::ExpectedFailure);
    let push =
        arrangement
            .contributor
            .nostr_push(["-u", "origin", "pr/feature", "-o", &push_option]);
    tokio::pin!(push);
    tokio::select! {
        output = &mut push => {
            let output = output?;
            panic!("push completed before the test could isolate its early pair: {}", output_text(&output));
        }
        observed = recorder.wait_for_proposal_offer() => {
            observed.context("helper never reached its early external staging operation")?;
        }
    }
    // Initial `list` has now completed against the primary. Only the early
    // external pair remains usable for the remainder of this operation.
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
    let output = push.await?;
    assert!(
        output.status.success(),
        "early acceptance turned into false helper failure: {}",
        output_text(&output)
    );
    assert_eq!(
        arrangement
            .contributor
            .snapshot()?
            .refs
            .get("refs/remotes/origin/pr/feature"),
        Some(&arrangement.tip),
        "early-only publication did not produce Git's accepted tracking ref"
    );
    let events = arrangement
        .harness
        .grasp("external")
        .events(
            Filter::new()
                .author(arrangement.contributor_keys.public_key())
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let event = events
        .into_iter()
        .next()
        .context("external relay lost helper's early accepted event")?;
    assert_eq!(
        arrangement
            .harness
            .grasp("external")
            .read_nostr_ref_prs(
                &arrangement.contributor_keys.public_key().to_hex(),
                IDENTIFIER,
                &event.id.to_hex(),
            )
            .await?,
        arrangement.tip
    );
    assert!(cached_id(&arrangement.contributor, event.id).await?);
    Ok(())
}
