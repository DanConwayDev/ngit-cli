//! Coverage of a no-op push against a vanilla (non-GRASP) git server:
//! the server already holds the requested change (a tag pushed to it
//! out-of-band), so the per-server plan is empty and the push must
//! succeed without any git data transfer while still publishing the
//! updated state event.
//!
//! A vanilla git server is essential here: its bare repo can be mutated
//! out-of-band (plain `git push` to its URL), while a GRASP server only
//! changes through nostr and re-aligns refs with state events. The
//! announcement is published manually (as in [`super::fresh_repo`]) so
//! the clone URL can be a plain `http://…/test-repo.git` path that
//! `is_grasp_server_clone_url` rejects.

use anyhow::{Context, Result};
use nostr::event::FinalizeEvent;
use nostr_sdk::prelude::*;
use test_harness::{Harness, KIND_REPO_STATE, Repo, tag_value};

/// Everything the scenario needs from the shared arrangement.
struct VanillaSetup {
    harness: Harness,
    publisher: Repo,
    maintainer_pubkey: PublicKey,
    identifier: String,
    /// full clone URL announced for the vanilla server
    /// (`http://127.0.0.1:<port>/test-repo.git`)
    server_repo_url: String,
}

/// Mint an account, seed a commit on `main`, announce the repo with the
/// vanilla git server as its only clone URL and the default relay as its
/// only repo relay, then push `main` over the nostr remote.
async fn setup(identifier: &str) -> Result<VanillaSetup> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("server")
    .build()
    .await?;

    let publisher = harness.fresh_repo()?;
    publisher
        .ngit(["account", "create", "--local", "--name", identifier])
        .output()
        .await
        .context("failed to spawn ngit account create")?
        .status
        .success()
        .then_some(())
        .context("ngit account create failed")?;

    let nsec = publisher
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after account create")?;
    let keys = Keys::parse(&nsec).context("invalid nsec in local config")?;
    let maintainer_pubkey = keys.public_key();
    let npub = maintainer_pubkey.to_bech32()?;

    std::fs::write(publisher.dir().join("README.md"), "vanilla server test\n")
        .context("failed to write seed file")?;
    publisher.git_ok(["add", "README.md"], "git add").await?;
    publisher
        .git_ok(
            ["commit", "-m", "initial", "--no-gpg-sign"],
            "git commit initial",
        )
        .await?;
    let seed_oid = publisher.rev_parse("HEAD").await?;

    // The path suffix keeps the URL out of the GRASP clone-url shape
    // (`http://host/<npub>/<identifier>.git`), which would otherwise
    // subject the server to GRASP staging eligibility.
    let server_repo_url = format!(
        "{}/test-repo.git",
        harness.vanilla_git_server("server").url()
    );
    let relay_url = harness.relay("default").url().to_string();

    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(vec![
            Tag::identifier(identifier.to_string()),
            Tag::parse(["r".to_string(), seed_oid, "euc".to_string()]).unwrap(),
            Tag::parse(["name".to_string(), identifier.to_string()]).unwrap(),
            Tag::parse(["clone".to_string(), server_repo_url.clone()]).unwrap(),
            Tag::parse(["relays".to_string(), relay_url.clone()]).unwrap(),
            Tag::parse(["maintainers".to_string(), maintainer_pubkey.to_string()]).unwrap(),
        ])
        .finalize(&keys)
        .context("failed to sign repo announcement")?;

    let client = Client::default();
    client.add_relay(&relay_url).await?;
    client.connect().await;
    let output = client.send_event(&announcement).await?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        anyhow::bail!("relay rejected the announcement: {:?}", output.failed);
    }

    let relay_hint = urlencoding::encode(&relay_url).into_owned();
    let nostr_url = format!("nostr://{npub}/{relay_hint}/{identifier}");
    publisher
        .git_ok(
            ["remote", "add", "origin", &nostr_url],
            "git remote add origin",
        )
        .await?;
    publisher
        .nostr_push(["-u", "origin", "main"])
        .await
        .context("initial git push -u origin main")?;

    Ok(VanillaSetup {
        harness,
        publisher,
        maintainer_pubkey,
        identifier: identifier.to_string(),
        server_repo_url,
    })
}

/// State events for the maintainer/identifier currently on the default
/// relay.
async fn relay_state_events(setup: &VanillaSetup) -> Result<Vec<Event>> {
    Ok(setup
        .harness
        .relay("default")
        .events(
            Filter::new()
                .author(setup.maintainer_pubkey)
                .kind(KIND_REPO_STATE),
        )
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(setup.identifier.as_str()))
        .collect())
}

/// An annotated tag already present on the git server (pushed to it
/// directly, bypassing nostr) yields an empty per-server plan when pushed
/// through the nostr remote. The push must succeed and the state event
/// must gain the tag with its `^{}` peel.
#[tokio::test(flavor = "multi_thread")]
async fn noop_push_of_tag_already_on_server_succeeds() -> Result<()> {
    let setup = setup("state-push-vanilla-noop").await?;
    let publisher = &setup.publisher;

    publisher
        .git_ok(
            ["tag", "-a", "v1.0", "-m", "release v1.0"],
            "git tag -a v1.0",
        )
        .await?;
    let tag_oid = publisher.rev_parse("v1.0").await?;
    let peeled_oid = publisher.rev_parse("v1.0^{}").await?;
    assert_ne!(
        tag_oid, peeled_oid,
        "arrange bug: annotated tag should have a distinct tag-object oid",
    );

    // Seed the tag on the server directly — a plain git push that never
    // touches nostr, so the nostr state still lacks the tag.
    publisher
        .git_ok(
            ["push", &setup.server_repo_url, "v1.0"],
            "git push <server-url> v1.0",
        )
        .await?;

    // The nostr push now has nothing left to send to the server; it must
    // still succeed and publish the updated state event.
    publisher
        .nostr_push(["origin", "v1.0"])
        .await
        .context("no-op nostr push of a tag already on the git server should succeed")?;

    let state_events = relay_state_events(&setup).await?;
    let tag_in_state = state_events.iter().any(|event| {
        tag_value(event, "refs/tags/v1.0").as_deref() == Some(tag_oid.as_str())
            && tag_value(event, "refs/tags/v1.0^{}").as_deref() == Some(peeled_oid.as_str())
    });
    assert!(
        tag_in_state,
        "state event on the repo relay should declare the annotated tag and its peel",
    );

    Ok(())
}
