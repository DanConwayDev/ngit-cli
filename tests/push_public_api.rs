//! Compatibility checks for the original public proposal-push APIs.
//!
//! Receipt-aware CLI operations use a separate selector. The original APIs
//! retain their signatures and push-then-sign behavior, with no hidden early
//! relay publication whose acceptance their callers could not redeem.

use std::{str::FromStr, sync::Arc};

use anyhow::{Context, Result, ensure};
use bitcoin_hashes::sha1::Hash as Sha1Hash;
use console::Term;
use ngit::{
    client::{Client, Connect, get_repo_ref_from_cache},
    git::Repo as NgitRepo,
    login::user::{UserGraspList, UserMetadata, UserRef, UserRelays},
    push::{
        ProposalMetadata, push_refs_and_generate_pr_or_pr_update_event,
        select_servers_push_refs_and_generate_pr_or_pr_update_event,
    },
    signer::NgitSigner,
};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, GraspObservation, GraspRecorder, Harness, KIND_PULL_REQUEST, PublishRepoOpts,
    tag_values,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_public_apis_push_git_without_early_relay_staging() -> Result<()> {
    const IDENTIFIER: &str = "legacy-public-selector";
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server_grasp06("repo")
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
                display_name: "legacy public selector contributor".into(),
            },
        )
        .await?;
    contributor
        .git_ok(["checkout", "-b", "feature"], "create feature branch")
        .await?;
    std::fs::write(contributor.dir().join("feature.md"), "legacy selector\n")?;
    contributor
        .git_ok(["add", "feature.md"], "stage feature")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "legacy selector", "--no-gpg-sign"],
            "commit feature",
        )
        .await?;

    let recorder = GraspRecorder::start(harness.grasp("repo"), false).await?;
    let clone = format!(
        "{}/{}/{}.git",
        recorder.url(),
        published.maintainer_npub,
        IDENTIFIER
    );
    let git_repo = NgitRepo::from_path(&contributor.dir().to_path_buf())?;
    let coordinate = Nip19Coordinate {
        coordinate: Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: published.maintainer_keys.public_key(),
            identifier: published.identifier.clone(),
        },
        relays: vec![],
    };
    let repo_ref = get_repo_ref_from_cache(Some(contributor.dir()), &coordinate).await?;
    let keys = Keys::parse(
        &contributor
            .config("nostr.nsec")
            .await?
            .context("contributor nsec missing")?,
    )?;
    let signer = Arc::new(NgitSigner::Keys(keys.clone()));
    let user_ref = UserRef {
        public_key: keys.public_key(),
        metadata: UserMetadata {
            name: "legacy public selector contributor".into(),
            created_at: Timestamp::from(0),
            nip05: None,
        },
        relays: UserRelays {
            relays: vec![],
            created_at: Timestamp::from(0),
        },
        grasp_list: UserGraspList {
            source_event: None,
            urls: vec![],
            created_at: Timestamp::from(0),
        },
    };
    let tip = Sha1Hash::from_str(&contributor.rev_parse("HEAD").await?)?;
    let merge_base = Sha1Hash::from_str(&contributor.rev_parse("HEAD~1").await?)?;
    let metadata = ProposalMetadata::default();
    let title_description = Some((
        "legacy selector".to_string(),
        "preserve the public push-then-sign contract".to_string(),
    ));
    let client = Client::default();
    let events = select_servers_push_refs_and_generate_pr_or_pr_update_event(
        &client,
        &git_repo,
        &repo_ref,
        &tip,
        &tip,
        Some(&merge_base),
        &metadata,
        &user_ref,
        None,
        &title_description,
        &signer,
        &Term::stderr(),
        &[],
        Some(&clone),
    )
    .await?;

    assert_eq!(events.len(), 1);
    let event = &events[0];
    event.verify()?;
    assert_eq!(event.kind, KIND_PULL_REQUEST);
    assert_eq!(tag_values(event, "clone"), vec![clone.clone()]);

    // Invoke the raw string helper too: this checks every argument position and
    // its original `(Option<Vec<Event>>, Vec<(String, Result<()>)>)` return
    // shape while exercising its deliberately separate legacy wrapper.
    let servers = vec![clone.clone()];
    let (raw_events, responses) = push_refs_and_generate_pr_or_pr_update_event(
        &git_repo,
        &repo_ref,
        &tip,
        &tip,
        Some(&merge_base),
        &metadata,
        &user_ref,
        None,
        &title_description,
        &servers,
        None,
        &signer,
        &Term::stderr(),
        &[],
    )
    .await?;
    ensure!(
        responses.iter().all(|(_, result)| result.is_ok()),
        "legacy raw helper rejected its repeated Git ref updates: {responses:?}"
    );
    let raw_events = raw_events.context("legacy raw helper returned no signed proposal")?;
    assert_eq!(raw_events.len(), 1);
    raw_events[0].verify()?;

    let observations = recorder.observations();
    assert!(
        observations
            .iter()
            .any(|entry| { matches!(entry, GraspObservation::ReceivePackBody) }),
        "legacy selector did not push Git data: {observations:?}"
    );
    assert!(
        observations.iter().all(|entry| {
            !matches!(
                entry,
                GraspObservation::ProposalOffered(_) | GraspObservation::ProposalAccepted(_)
            )
        }),
        "receipt-free public selector created an early relay publication: {observations:?}"
    );
    ensure!(
        harness
            .grasp("repo")
            .read_nostr_ref(&published.maintainer_npub, IDENTIFIER, &event.id.to_hex(),)
            .await?
            == tip.to_string(),
        "legacy selector's Git ref does not identify the returned event and tip"
    );
    Ok(())
}
