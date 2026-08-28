//! Normal-path coverage for announcement convergence and local lead selection.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, PublishRepoOpts, PublishedRepo, Repo, tag_value, tag_values,
};

async fn latest_announcement(
    harness: &Harness,
    author: PublicKey,
    identifier: &str,
) -> Result<Event> {
    let filter = Filter::new().author(author).kind(Kind::GitRepoAnnouncement);
    let mut events = harness.relay("default").events(filter.clone()).await?;
    events.extend(harness.grasp("repo").events(filter).await?);
    events
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .context("repository announcement is missing")
}

async fn command_ok(repo: &Repo, args: &[&str]) -> Result<()> {
    let output = repo.ngit(args).output().await?;
    if !output.status.success() {
        bail!(
            "ngit command {args:?} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

async fn accept_as(harness: &Harness, published: &PublishedRepo, keys: &Keys) -> Result<Repo> {
    harness.publish_user_relay_list(keys).await?;
    let repo = harness.clone_published_repo_as(published, keys).await?;
    command_ok(&repo, &["repo", "accept"]).await?;
    Ok(repo)
}

fn active_role(event: &Event, letter: &str, pubkey: PublicKey) -> Option<Vec<String>> {
    let pubkey = pubkey.to_string();
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .find(|tag| {
            tag.first().map(String::as_str) == Some(letter)
                && tag.get(1) == Some(&pubkey)
                && tag.len() % 2 == 1
        })
}

async fn assert_selected_lead(repo: &Repo, lead: PublicKey) -> Result<()> {
    let lead_npub = lead.to_bech32()?;
    let origin = repo
        .config("remote.origin.url")
        .await?
        .context("origin URL is missing")?;
    assert!(
        origin.contains(&lead_npub),
        "origin should select {lead_npub}, got {origin}",
    );
    let coordinate = repo
        .config("nostr.repo")
        .await?
        .context("nostr.repo is missing after follow-lead")?;
    assert_eq!(Nip19Coordinate::from_bech32(&coordinate)?.public_key, lead,);
    Ok(())
}

#[tokio::test]
async fn confirmed_co_maintainer_converges_history_and_local_selection() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("follow confirmed lead".into()),
            identifier: Some("follow-confirmed-lead".into()),
            additional_maintainer_count: 2,
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    let carol_keys = published.additional_maintainer_keys[1].clone();
    let carol = carol_keys.public_key();
    let bob_repo = accept_as(&harness, &published, &bob_keys).await?;
    let carol_repo = accept_as(&harness, &published, &carol_keys).await?;
    let bob_npub = bob.to_bech32()?;

    command_ok(&bob_repo, &["repo", "edit", "--lead-maintainer", &bob_npub]).await?;
    command_ok(
        &alice_repo,
        &["repo", "edit", "--lead-maintainer", &bob_npub],
    )
    .await?;
    let info = carol_repo
        .ngit(["repo", "--json", "--offline"])
        .output()
        .await?;
    assert!(info.status.success());
    let info: serde_json::Value = serde_json::from_slice(&info.stdout)?;
    assert_eq!(info["lead_source"], "explicit");
    assert!(
        info["lead_path"]
            .as_array()
            .is_some_and(|path| !path.is_empty())
    );
    assert!(info["pending_actions"].is_array());
    assert!(info["health"]["status"].is_string());
    command_ok(&carol_repo, &["repo", "follow-lead"]).await?;

    assert_selected_lead(&carol_repo, bob).await?;
    let followed = latest_announcement(&harness, carol, &published.identifier).await?;
    assert!(active_role(&followed, "M", bob).is_some());
    assert!(active_role(&followed, "m", carol).is_some());
    assert!(active_role(&followed, "M", alice).is_none());
    assert_eq!(
        tag_values(&followed, "maintainers")
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        [bob.to_string(), carol.to_string()].into_iter().collect(),
    );
    Ok(())
}

#[tokio::test]
async fn removed_maintainer_adopts_the_lead_end_and_keeps_only_redirect_active() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("follow after removal".into()),
            identifier: Some("follow-after-removal".into()),
            additional_maintainer_count: 2,
            ..Default::default()
        })
        .await?;
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    let carol_keys = published.additional_maintainer_keys[1].clone();
    let carol = carol_keys.public_key();
    let bob_repo = accept_as(&harness, &published, &bob_keys).await?;
    let carol_repo = accept_as(&harness, &published, &carol_keys).await?;
    let bob_npub = bob.to_bech32()?;
    let carol_npub = carol.to_bech32()?;

    command_ok(
        &alice_repo,
        &["repo", "edit", "--remove-maintainer", &carol_npub],
    )
    .await?;
    command_ok(&bob_repo, &["repo", "follow-lead"]).await?;
    command_ok(&bob_repo, &["repo", "edit", "--lead-maintainer", &bob_npub]).await?;
    command_ok(
        &alice_repo,
        &["repo", "edit", "--lead-maintainer", &bob_npub],
    )
    .await?;
    command_ok(&carol_repo, &["repo", "follow-lead"]).await?;

    assert_selected_lead(&carol_repo, bob).await?;
    let followed = latest_announcement(&harness, carol, &published.identifier).await?;
    assert!(active_role(&followed, "M", bob).is_some());
    assert!(active_role(&followed, "m", carol).is_none());
    assert_eq!(tag_values(&followed, "maintainers"), vec![bob.to_string()]);
    let self_history = followed.tags.iter().find_map(|tag| {
        let tag = tag.as_slice();
        (tag.first().map(String::as_str) == Some("m") && tag.get(1) == Some(&carol.to_string()))
            .then(|| tag.to_vec())
    });
    assert!(
        self_history.is_some_and(|history| {
            history.len() % 2 == 0 && history.last().is_some_and(|end| end.parse::<u64>().is_ok())
        }),
        "Carol's self-role should end at the lead's numeric boundary",
    );
    Ok(())
}

#[tokio::test]
async fn stale_removed_coordinate_rejects_push_until_following_the_lead() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("stale selected removal".into()),
            identifier: Some("stale-selected-removal".into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;
    let alice = published.maintainer_keys.public_key();
    let removed_keys = published.additional_maintainer_keys[0].clone();
    let removed = removed_keys.public_key();
    let removed_npub = removed.to_bech32()?;
    let removed_repo = accept_as(&harness, &published, &removed_keys).await?;

    let removed_coordinate = Nip19Coordinate {
        coordinate: Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: removed,
            identifier: published.identifier.clone(),
        },
        relays: vec![],
    }
    .to_bech32()?;
    let removed_url = published
        .clone_url
        .replacen(&published.maintainer_npub, &removed_npub, 1);
    assert_ne!(removed_url, published.clone_url);
    removed_repo
        .git_ok(
            ["config", "--local", "nostr.repo", &removed_coordinate],
            "select removed maintainer coordinate",
        )
        .await?;
    removed_repo
        .git_ok(
            ["remote", "set-url", "origin", &removed_url],
            "point origin at removed maintainer coordinate",
        )
        .await?;

    command_ok(
        &alice_repo,
        &["repo", "edit", "--remove-maintainer", &removed_npub],
    )
    .await?;
    std::fs::write(removed_repo.dir().join("stale.md"), "must not publish\n")?;
    removed_repo.git_ok(["add", "stale.md"], "git add").await?;
    removed_repo
        .git_ok(
            ["commit", "-m", "stale removed push", "--no-gpg-sign"],
            "git commit",
        )
        .await?;

    let rejected = removed_repo
        .nostr_push_expecting_failure(["origin", "HEAD:main"])
        .await?;
    let rejection = format!(
        "{}\n{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr),
    );
    assert!(
        rejection.contains("no longer a confirmed maintainer")
            && rejection.contains("ngit repo follow-lead"),
        "unexpected push rejection: {rejection}",
    );

    command_ok(&removed_repo, &["repo", "follow-lead"]).await?;
    assert_selected_lead(&removed_repo, alice).await?;
    let followed = latest_announcement(&harness, removed, &published.identifier).await?;
    assert!(active_role(&followed, "M", alice).is_some());
    assert!(
        active_role(&followed, "m", removed).is_none(),
        "removed self-role stayed active: {:?}",
        followed.tags,
    );
    Ok(())
}

#[tokio::test]
async fn user_without_a_role_changes_only_local_selection() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (alice_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("follow without role".into()),
            identifier: Some("follow-without-role".into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;
    let bob_keys = published.additional_maintainer_keys[0].clone();
    let bob = bob_keys.public_key();
    let bob_npub = bob.to_bech32()?;
    let bob_repo = accept_as(&harness, &published, &bob_keys).await?;
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "repo reader".into(),
            },
        )
        .await?;

    command_ok(&bob_repo, &["repo", "edit", "--lead-maintainer", &bob_npub]).await?;
    command_ok(
        &alice_repo,
        &["repo", "edit", "--lead-maintainer", &bob_npub],
    )
    .await?;
    let announcements_before = harness
        .relay("default")
        .events(Filter::new().kind(Kind::GitRepoAnnouncement))
        .await?
        .len();

    command_ok(&contributor, &["repo", "follow-lead"]).await?;

    assert_selected_lead(&contributor, bob).await?;
    let announcements_after = harness
        .relay("default")
        .events(Filter::new().kind(Kind::GitRepoAnnouncement))
        .await?
        .len();
    assert_eq!(
        announcements_after, announcements_before,
        "a user without a repository role must not publish an announcement",
    );
    Ok(())
}
