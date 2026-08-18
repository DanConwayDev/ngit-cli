//! End-to-end coverage for GRASP services mounted below a domain root.
//!
//! ngit-grasp exposes `NGIT_BASE_PATH` so one authority can host several
//! services. These tests exercise ngit's path-preserving URL handling against
//! the real subprocess rather than proving only the string helpers in
//! `repo_ref.rs`.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, PublishRepoOpts, event_branch_name_tag, tag_value,
    tag_values,
};

const BASE_PATH: &str = "/services/grasp";

#[tokio::test]
async fn publish_push_and_clone_through_path_mounted_grasp() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server_at_base_path("repo", BASE_PATH)
    .build()
    .await?;

    let grasp = harness.grasp("repo");
    assert!(
        grasp.url().ends_with(BASE_PATH),
        "path-mounted GRASP HTTP URL lost its base path: {}",
        grasp.url(),
    );
    assert!(
        grasp.relay_url().ends_with(BASE_PATH),
        "path-mounted GRASP relay URL lost its base path: {}",
        grasp.relay_url(),
    );

    let identifier = "path-mounted-grasp-repo";
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("path-mounted GRASP maintainer".into()),
            identifier: Some(identifier.into()),
            initial_file: Some(("mounted.txt".into(), "served below a path\n".into())),
            ..Default::default()
        })
        .await?;

    let announcements = grasp
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .context("path-mounted GRASP did not expose the repository announcement")?;

    let expected_clone_url = format!(
        "{}/{}/{}.git",
        grasp.url(),
        published.maintainer_npub,
        identifier,
    );
    assert_eq!(
        tag_values(announcement, "clone"),
        vec![expected_clone_url],
        "announcement clone URL must retain the GRASP base path",
    );
    assert!(
        tag_values(announcement, "relays").contains(&grasp.relay_url()),
        "announcement relay URL must retain the GRASP base path",
    );
    assert!(
        published.clone_url.contains("%2Fservices%2Fgrasp"),
        "nostr:// relay hint must percent-encode the GRASP base path: {}",
        published.clone_url,
    );

    let clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await
        .context("clone through path-mounted GRASP failed")?;
    assert_eq!(
        std::fs::read_to_string(clone.dir().join("mounted.txt"))?,
        "served below a path\n",
    );
    assert_eq!(clone.rev_parse("HEAD").await?, published.initial_oid);

    Ok(())
}

#[tokio::test]
async fn explicit_grasp06_push_uses_path_mounted_pr_endpoint() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server_grasp06_at_base_path("repo", BASE_PATH)
    .build()
    .await?;

    let identifier = "path-mounted-grasp06-repo";
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("path-mounted GRASP-06 maintainer".into()),
            identifier: Some(identifier.into()),
            ..Default::default()
        })
        .await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "path-mounted GRASP-06 contributor".into(),
            },
        )
        .await?;

    contributor
        .git_ok(
            ["checkout", "-b", "path-mounted-feature"],
            "create contributor feature branch",
        )
        .await?;
    std::fs::write(contributor.dir().join("feature.txt"), "path-aware PR\n")?;
    contributor
        .git_ok(["add", "feature.txt"], "stage contributor change")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add path-aware feature", "--no-gpg-sign"],
            "commit contributor change",
        )
        .await?;
    let tip = contributor.rev_parse("HEAD").await?;

    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("contributor clone has no local nostr.nsec")?;
    let contributor_keys = Keys::parse(&contributor_nsec)?;
    let contributor_npub = contributor_keys.public_key().to_bech32()?;
    let contributor_pubkey_hex = contributor_keys.public_key().to_hex();
    let grasp_relay_url = harness.grasp("repo").relay_url();

    let send = contributor
        .ngit([
            "send",
            "HEAD~1",
            "--force-pr",
            "--title",
            "path-aware feature",
            "--description",
            "exercise a path-mounted GRASP-06 endpoint",
            "--git-server",
            &grasp_relay_url,
        ])
        .output()
        .await
        .context("spawn ngit send against path-mounted GRASP-06")?;
    if !send.status.success() {
        bail!(
            "ngit send against path-mounted GRASP-06 failed ({:?})\nstdout: {}\nstderr: {}",
            send.status,
            String::from_utf8_lossy(&send.stdout),
            String::from_utf8_lossy(&send.stderr),
        );
    }

    let pr_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_keys.public_key())
                .kind(KIND_PULL_REQUEST),
        )
        .await?;
    let pr_event = pr_events
        .iter()
        .find(|event| event_branch_name_tag(event).as_deref() == Some("path-mounted-feature"))
        .context("path-mounted GRASP did not expose the contributor's PR event")?;

    let expected_pr_url = format!(
        "{}/prs/{}/{}.git",
        harness.grasp("repo").url(),
        contributor_npub,
        identifier,
    );
    assert_eq!(
        tag_values(pr_event, "clone"),
        vec![expected_pr_url],
        "GRASP-06 clone URL must insert /prs below the configured base path",
    );

    let stored_tip = harness
        .grasp("repo")
        .read_nostr_ref_prs(&contributor_pubkey_hex, identifier, &pr_event.id.to_hex())
        .await?;
    assert_eq!(stored_tip, tip);

    Ok(())
}
