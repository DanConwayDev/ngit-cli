//! Shared arrangement code for the `ngit sync` scenario suite.
//!
//! Every scenario starts from the same base shape, modelled on
//! `tests/git_push_state/delete_branch.rs`:
//!
//! 1. a fresh maintainer account (`ngit account create --local`),
//! 2. a seed commit on `main`,
//! 3. a manually signed kind-30617 announcement listing the harness's git
//!    servers as `clone` URLs and the default relay as the repo relay,
//! 4. a `nostr://` remote named `origin`, and
//! 5. an initial `git push -u origin main` through the remote helper so a
//!    kind-30618 state event exists on the default relay.
//!
//! The announcement is signed manually (rather than via `ngit init`) so
//! each scenario controls exactly which git servers the repository
//! lists — including plain-http vanilla servers that
//! `is_grasp_server_clone_url` rejects, and (for the grasp-seeding
//! scenario) a later announcement revision that adds a grasp server.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{Harness, KIND_REPO_STATE, Repo, tag_value};

/// Everything the scenarios need from the shared arrangement.
pub struct SyncSetup {
    pub harness: Harness,
    pub publisher: Repo,
    pub maintainer_keys: Keys,
    pub npub: String,
    pub identifier: String,
    /// URL of the vanilla relay registered under the `default` role —
    /// the only repo relay on the announcement.
    pub relay_url: String,
    /// Announced clone URLs, in the same order as the roles passed to
    /// [`setup_vanilla`].
    pub server_urls: Vec<String>,
    /// Seed-commit oid that `refs/heads/main` points at after the
    /// arrangement.
    pub main_oid: String,
    /// The kind-30617 announcement as published — kept so scenarios can
    /// publish a strictly newer revision without sleeping.
    pub announcement: Event,
}

/// Build a harness with the default relay plus one empty vanilla git
/// server per role, then run [`announce_and_push`] with all of them as
/// the announced clone URLs.
pub async fn setup_vanilla(identifier: &str, server_roles: &[&str]) -> Result<SyncSetup> {
    let mut builder = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default");
    for role in server_roles {
        builder = builder.with_vanilla_git_server(*role);
    }
    let harness = builder.build().await?;
    let server_urls = server_roles
        .iter()
        .map(|role| {
            format!(
                "{}/{identifier}.git",
                harness.vanilla_git_server(role).url()
            )
        })
        .collect();
    announce_and_push(harness, identifier, server_urls).await
}

/// Mint an account and a seed commit in a fresh publisher repo, announce
/// `server_urls` with the default relay as the only repo relay, add the
/// `nostr://` remote, and push `main` through the remote helper.
pub async fn announce_and_push(
    harness: Harness,
    identifier: &str,
    server_urls: Vec<String>,
) -> Result<SyncSetup> {
    let publisher = harness.fresh_repo()?;
    let maintainer_keys = create_local_account(&publisher, identifier).await?;
    let npub = maintainer_keys
        .public_key()
        .to_bech32()
        .context("failed to bech32-encode maintainer pubkey")?;

    let main_oid = seed_commit(&publisher).await?;

    let relay_url = harness.relay("default").url().to_string();
    let announcement = sign_announcement(
        &maintainer_keys,
        identifier,
        &main_oid,
        &server_urls,
        std::slice::from_ref(&relay_url),
        None,
    )?;
    publish_event_to_all(&announcement, &[relay_url.as_str()]).await?;

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

    Ok(SyncSetup {
        harness,
        publisher,
        maintainer_keys,
        npub,
        identifier: identifier.to_string(),
        relay_url,
        server_urls,
        main_oid,
        announcement,
    })
}

/// `ngit account create --local --name <name>`, returning the freshly
/// minted keys read back from the repo's local git config.
pub async fn create_local_account(repo: &Repo, name: &str) -> Result<Keys> {
    let out = repo
        .ngit(["account", "create", "--local", "--name", name])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    if !out.status.success() {
        bail!(
            "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let nsec = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")
}

/// Seed commit on `main` (a README with deterministic content), returning
/// its oid.
pub async fn seed_commit(repo: &Repo) -> Result<String> {
    std::fs::write(repo.dir().join("README.md"), "ngit sync scenario seed\n")
        .context("failed to write seed file")?;
    repo.git_ok(["add", "README.md"], "git add README.md")
        .await?;
    repo.git_ok(
        ["commit", "-m", "initial", "--no-gpg-sign"],
        "git commit initial",
    )
    .await?;
    repo.rev_parse("HEAD").await
}

/// Commit a new file on the current branch and return the new HEAD oid.
pub async fn commit_new_file(repo: &Repo, filename: &str, message: &str) -> Result<String> {
    std::fs::write(repo.dir().join(filename), format!("{message}\n"))
        .with_context(|| format!("failed to write {filename}"))?;
    repo.git_ok(["add", filename], "git add").await?;
    repo.git_ok(
        ["commit", "-m", message, "--no-gpg-sign"],
        "git commit new file",
    )
    .await?;
    repo.rev_parse("HEAD").await
}

/// Sign a kind-30617 announcement for `identifier`, listing `clone_urls`
/// and `relay_urls`. `created_at` overrides the event timestamp — used
/// by scenarios that publish a strictly newer announcement revision
/// without waiting for the wall clock.
///
/// Tag shape modelled on `src/lib/repo_ref.rs::RepoRef::to_event`, same
/// as `tests/git_push_state/delete_branch.rs`.
pub fn sign_announcement(
    keys: &Keys,
    identifier: &str,
    euc_oid: &str,
    clone_urls: &[String],
    relay_urls: &[String],
    reference: Option<&Event>,
) -> Result<Event> {
    let mut clone_tag = vec!["clone".to_string()];
    clone_tag.extend(clone_urls.iter().cloned());
    let mut relays_tag = vec!["relays".to_string()];
    relays_tag.extend(relay_urls.iter().cloned());
    let tags: Vec<Tag> = vec![
        Tag::identifier(identifier.to_string()),
        Tag::parse(["r".to_string(), euc_oid.to_string(), "euc".to_string()])
            .context("failed to build euc tag")?,
        Tag::parse(["name".to_string(), identifier.to_string()])
            .context("failed to build name tag")?,
        Tag::parse(clone_tag).context("failed to build clone tag")?,
        Tag::parse(relays_tag).context("failed to build relays tag")?,
        Tag::parse(["maintainers".to_string(), keys.public_key().to_string()])
            .context("failed to build maintainers tag")?,
    ];
    test_harness::finalize_ordered_fixture(
        EventBuilder::new(Kind::GitRepoAnnouncement, "").tags(tags),
        keys,
        reference,
        test_harness::event_ordering::OrderingPolicy::StrictlyLater,
    )
}

/// Publish `event` to every relay URL in `urls`, bailing if any relay
/// rejects it.
pub async fn publish_event_to_all(event: &Event, urls: &[&str]) -> Result<()> {
    let client = Client::default();
    for url in urls {
        client
            .add_relay(*url)
            .await
            .with_context(|| format!("add_relay {url}"))?;
    }
    client.connect().await;
    let output = client
        .send_event(event)
        .to(urls.iter().copied())
        .await
        .context("send_event fan-out")?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "one or more relays rejected event id={}: {:?}",
            event.id,
            output.failed,
        );
    }
    Ok(())
}

/// Run `ngit sync <args>` in `repo`, bailing with captured output on
/// non-zero exit.
pub async fn sync_ok(repo: &Repo, args: &[&str]) -> Result<()> {
    let mut argv = vec!["sync"];
    argv.extend_from_slice(args);
    let out = repo
        .ngit(&argv)
        .output()
        .await
        .context("failed to spawn ngit sync")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "ngit sync {args:?} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }
}

/// Read the oid `refname` resolves to inside a bare repo on disk, or
/// `None` when the ref does not exist.
pub async fn bare_ref(bare_repo: &Path, refname: &str) -> Result<Option<String>> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare_repo)
        .args(["for-each-ref", "--format=%(objectname)", refname])
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn git for-each-ref {refname} in {}",
                bare_repo.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "git for-each-ref {refname} exited non-zero in {}: {}",
            bare_repo.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let oid = String::from_utf8(out.stdout)
        .context("git for-each-ref output is not valid UTF-8")?
        .trim()
        .to_string();
    Ok(if oid.is_empty() { None } else { Some(oid) })
}

/// Point `refname` at `oid` inside a bare repo, bypassing any transport
/// — simulates out-of-band server mutation (e.g. a hosting-side
/// rollback that made the server miss a push).
pub async fn set_bare_ref(bare_repo: &Path, refname: &str, oid: &str) -> Result<()> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare_repo)
        .args(["update-ref", refname, oid])
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn git update-ref {refname} in {}",
                bare_repo.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "git update-ref {refname} {oid} failed in {}: {}",
            bare_repo.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(())
}

/// The current NIP-01 winner among the kind-30618 state events for
/// `identifier` on the default relay: max `created_at`, lower id on a
/// tie. The relay's memory database normally stores only the winner;
/// the tie-break keeps the pick deterministic either way.
pub async fn latest_state_event(
    harness: &Harness,
    author: PublicKey,
    identifier: &str,
) -> Result<Event> {
    harness
        .relay("default")
        .events(Filter::new().author(author).kind(KIND_REPO_STATE))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
        .with_context(|| format!("no kind-30618 state event for {identifier} on the default relay"))
}

/// Poll for `path` to exist, with a short ceiling — the grasp's
/// announcement policy creates the bare repo synchronously on receipt
/// but the relay ACK can return before the filesystem op is visible.
/// This is one of the two waits the harness docs permit: observing
/// bare-repo creation after a raw announcement publish, which has no
/// push-completion barrier to lean on.
pub async fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while !path.is_dir() {
        if std::time::Instant::now() >= deadline {
            bail!(
                "timed out after {:?} waiting for {} to be created — \
                 did the grasp accept the announcement?",
                timeout,
                path.display(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
