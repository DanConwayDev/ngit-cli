//! `ngit repo accept` — accepting co-maintainership publishes the accepter's
//! own kind-30617 announcement, and nothing else locally.
//!
//! The coordinate the repo resolves from is the root of trust. If accepting
//! re-rooted resolution on the accepter's own announcement — which always
//! lists them as a maintainer — the accepter could never observe the inviter
//! removing them later. Both accept paths (defaults and explicit
//! `--grasp-server`) must therefore leave `remote.origin.url` byte-for-byte
//! unchanged and `nostr.repo` unwritten; only `ngit repo edit` / `ngit init`
//! may change the resolved coordinate deliberately.
//!
//! ## Why the announcement is asserted on the vanilla relay + grasp disk
//!
//! `ngit-grasp` routes brand-new announcements to purgatory rather than its
//! relay DB (see `tests/init_grasp.rs` for the full story), so a REQ against
//! the grasp returns nothing until git data is pushed. The observable proof
//! that the grasp accepted the announcement is the bare repository it creates
//! at `<git_data_path>/<npub>/<identifier>.git`; the vanilla "default" relay
//! (the accepter's fallback write relay) stores the event normally and lets
//! us assert on its content.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, PublishRepoOpts, PublishedRepo, Repo, tag_value, tag_values,
};

/// Publish a repo with one invited (announcement-less) co-maintainer, clone
/// it, and log the clone in as that co-maintainer.
async fn arrange_invited_clone(
    identifier: &str,
) -> Result<(Harness, PublishedRepo, Repo, PublicKey)> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some(identifier.into()),
            identifier: Some(identifier.into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;

    let co_maintainer_keys = published
        .additional_maintainer_keys
        .first()
        .context("publish_repo did not mint a co-maintainer key")?;
    let co_maintainer_pubkey = co_maintainer_keys.public_key();
    let co_maintainer_nsec = co_maintainer_keys.secret_key().to_bech32()?;

    // The invited key has never published a NIP-65 relay list. Without one
    // the accept fan-out only reaches the grasp's relay, whose purgatory is
    // not queryable (see module doc); a write relay pointing at the vanilla
    // "default" relay gives the announcement a queryable destination and
    // matches how a real logged-in user would be set up.
    let relay_url = harness.relay("default").url().to_string();
    let relay_list = RelayList::new([(RelayUrl::parse(&relay_url)?, None)])
        .finalize(co_maintainer_keys)
        .context("failed to sign co-maintainer relay list event")?;
    let nostr_client = Client::default();
    nostr_client
        .add_relay(&relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url} for relay list publish"))?;
    nostr_client.connect().await;
    let output = nostr_client
        .send_event(&relay_list)
        .to([relay_url.as_str()])
        .await
        .with_context(|| format!("failed to publish co-maintainer relay list to {relay_url}"))?;
    nostr_client.disconnect().await;
    if !output.failed.is_empty() {
        anyhow::bail!(
            "relay at {relay_url} rejected co-maintainer relay list: {:?}",
            output.failed,
        );
    }

    let clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await
        .context("clone published repo as invited co-maintainer")?;
    clone
        .git_ok(
            ["config", "--local", "nostr.nsec", &co_maintainer_nsec],
            "git config nostr.nsec (login as invited co-maintainer)",
        )
        .await?;

    Ok((harness, published, clone, co_maintainer_pubkey))
}

/// Assert the accepter's kind-30617 landed on their fallback write relay with
/// both maintainers listed, and that the grasp accepted it (bare repo on
/// disk under the accepter's npub).
async fn assert_announcement_published(
    harness: &Harness,
    published: &PublishedRepo,
    co_maintainer_pubkey: PublicKey,
) -> Result<()> {
    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("co-maintainer kind-30617 was not published on repo accept")?;

    let maintainers = tag_values(announcement, "maintainers");
    assert!(
        maintainers.contains(&co_maintainer_pubkey.to_string()),
        "announcement should list the accepting co-maintainer; got {maintainers:?}",
    );
    assert!(
        maintainers.contains(&published.maintainer_keys.public_key().to_string()),
        "announcement should retain the inviting maintainer; got {maintainers:?}",
    );

    let bare_repo = harness
        .grasp("repo")
        .git_data_path()
        .join(co_maintainer_pubkey.to_bech32()?)
        .join(format!("{}.git", published.identifier));
    assert!(
        bare_repo.is_dir(),
        "expected the grasp to accept the announcement and create {}",
        bare_repo.display(),
    );

    Ok(())
}

/// Run `ngit repo accept <extra_args>` in `clone` and assert that origin and
/// `nostr.repo` still resolve from the inviter's coordinate afterwards.
async fn accept_and_assert_resolution_untouched(clone: &Repo, extra_args: &[&str]) -> Result<()> {
    let origin_before = clone
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing before repo accept")?;
    assert!(
        clone.config("nostr.repo").await?.is_none(),
        "precondition: a fresh clone should have no nostr.repo config",
    );

    let mut args = vec!["repo", "accept"];
    args.extend_from_slice(extra_args);
    let out = clone
        .ngit(&args)
        .output()
        .await
        .context("failed to spawn ngit repo accept")?;
    assert!(
        out.status.success(),
        "ngit repo accept exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let origin_after = clone
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing after repo accept")?;
    assert_eq!(
        origin_after, origin_before,
        "repo accept must leave the origin remote untouched so resolution \
         stays rooted on the inviter's coordinate",
    );
    assert_eq!(
        clone.config("nostr.repo").await?,
        None,
        "repo accept must not write nostr.repo; re-rooting resolution on the \
         accepter's own coordinate would hide a later removal",
    );

    Ok(())
}

#[tokio::test]
async fn accept_with_defaults_publishes_announcement_without_rerooting_resolution() -> Result<()> {
    let (harness, published, clone, co_maintainer_pubkey) =
        arrange_invited_clone("repo-accept-defaults").await?;

    accept_and_assert_resolution_untouched(&clone, &[]).await?;
    assert_announcement_published(&harness, &published, co_maintainer_pubkey).await?;

    Ok(())
}

#[tokio::test]
async fn accept_with_grasp_server_flag_publishes_announcement_without_rerooting_resolution()
-> Result<()> {
    let (harness, published, clone, co_maintainer_pubkey) =
        arrange_invited_clone("repo-accept-grasp-flag").await?;

    let grasp_url = harness.grasp("repo").url().to_string();
    accept_and_assert_resolution_untouched(&clone, &["--grasp-server", &grasp_url]).await?;
    assert_announcement_published(&harness, &published, co_maintainer_pubkey).await?;

    Ok(())
}
