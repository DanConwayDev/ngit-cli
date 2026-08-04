//! `ngit init` with a pre-existing, listable `origin` fetches git data
//! that exists only on the origin server before signing and pushing the
//! origin-derived state.
//!
//! The regression this pins: `StateAction::PublishOriginState` builds
//! the candidate kind-30618 from origin's *listing*, but the local odb
//! may lack the listed objects (single-branch clones, `--no-tags`
//! clones, or — as here — refs added upstream after the local clone).
//! Without the pre-sign fetch in `publish_origin_state`
//! (`src/bin/ngit/sub_commands/init.rs`), an annotated tag that was
//! never fetched locally could not be pushed to the repository's git
//! servers and would be silently dropped from (or worse, advertised
//! unpushable in) the published state.
//!
//! ## Scenario
//!
//! 1. A vanilla smart-http git server ("old-origin") holds commit C1 on `main`;
//!    the local repo has C1 and `origin` pointing at it.
//! 2. Out-of-band (via a scratch clone), commit C2 lands on `main` and
//!    annotated tag `v1.0.0` (tag object T → C2) is pushed — both exist only on
//!    the origin server, never in the local repo.
//! 3. `ngit init --grasp-server <harness grasp> -d` runs non-interactively.
//!
//! ## Assertions
//!
//! - The grasp's clone URL advertises `refs/tags/v1.0.0` at T (with the peeled
//!   `^{}` entry at C2 when the listing provides one) and `refs/heads/main` at
//!   C2 — origin's listing, not the local ref.
//! - The kind-30618 state event on the grasp relay carries `refs/tags/v1.0.0` →
//!   T (and its peeled twin → C2).
//! - The local repo gained the objects (odb) but **no** `refs/tags/v1.0.0` ref
//!   — server refs stay dynamic.
//! - `origin` now holds the nostr URL and the old origin URL survives under the
//!   domain-derived remote name (`127-0-0-1` — IP hosts keep every octet,
//!   dash-joined).
//!
//! Per the harness push-completion contract, a successful `ngit init`
//! does not return until its state event and refs are queryable, so all
//! assertions run immediately with no polling.

use std::collections::HashMap;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, KIND_REPO_STATE, Repo, tag_value};

const IDENTIFIER: &str = "origin-only-tag";
const TAG_NAME: &str = "v1.0.0";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_only_annotated_tag_is_fetched_and_published() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_vanilla_git_server("old-origin")
    .build()
    .await?;

    let origin_url = harness.vanilla_git_server("old-origin").url().to_string();

    // --- arrange: local repo with C1, origin pointing at the vanilla server ---
    let repo = harness.fresh_repo()?;
    let create = repo
        .ngit(["account", "create", "--local", "--name", "origin tag test"])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    assert!(
        create.status.success(),
        "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        create.status,
        String::from_utf8_lossy(&create.stdout),
        String::from_utf8_lossy(&create.stderr),
    );
    let nsec = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    let npub = Keys::parse(&nsec)
        .context("nostr.nsec from local config is not a valid key")?
        .public_key()
        .to_bech32()
        .context("failed to bech32-encode the new account's public key")?;

    std::fs::write(repo.dir().join("README.md"), "hello, ngit!\n")
        .context("failed to write seed file")?;
    repo.git_ok(["add", "README.md"], "git add README.md")
        .await?;
    repo.git_ok(
        ["commit", "-m", "initial", "--no-gpg-sign"],
        "git commit (C1)",
    )
    .await?;
    let c1 = repo.rev_parse("HEAD").await?;

    repo.git_ok(
        ["push", &origin_url, "main:main"],
        "git push C1 to old-origin",
    )
    .await?;
    repo.git_ok(
        ["remote", "add", "origin", &origin_url],
        "git remote add origin",
    )
    .await?;

    // --- arrange: C2 + annotated tag, present only on the origin server ------
    let scratch = harness.clone_url(&origin_url).await?;
    std::fs::write(scratch.dir().join("feature.md"), "added upstream\n")
        .context("failed to write feature.md in scratch clone")?;
    scratch
        .git_ok(["add", "feature.md"], "git add feature.md (scratch)")
        .await?;
    scratch
        .git_ok(
            ["commit", "-m", "add feature", "--no-gpg-sign"],
            "git commit (C2, scratch)",
        )
        .await?;
    scratch
        .git_ok(
            ["tag", "-a", TAG_NAME, "-m", "first release"],
            "git tag -a (scratch)",
        )
        .await?;
    scratch
        .git_ok(
            ["push", "origin", "main", TAG_NAME],
            "git push C2 + tag to old-origin",
        )
        .await?;
    let c2 = scratch.rev_parse("HEAD").await?;
    // `rev-parse refs/tags/<name>` resolves to the annotated tag *object*,
    // not the tagged commit — exactly what origin's listing advertises for
    // the unpeeled ref.
    let tag_object = scratch.rev_parse(&format!("refs/tags/{TAG_NAME}")).await?;
    assert_ne!(
        tag_object, c2,
        "tag should be annotated: its tag object must differ from the tagged commit"
    );

    // Sanity: neither the tag object nor C2 is in the local repo.
    assert!(
        !object_in_odb(&repo, &tag_object).await?,
        "arrange broken: the tag object {tag_object} is already in the local odb"
    );
    assert!(
        !object_in_odb(&repo, &c2).await?,
        "arrange broken: C2 {c2} is already in the local odb"
    );

    // --- act: non-interactive ngit init against the harness grasp ------------
    let grasp_url = harness.grasp("repo").url().to_string();
    let init = repo
        .ngit([
            "init",
            "--name",
            "origin tag test",
            "--identifier",
            IDENTIFIER,
            "--grasp-server",
            &grasp_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init")?;
    assert!(
        init.status.success(),
        "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        init.status,
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );

    // --- assert (a): the grasp advertises the tag at origin's oids -----------
    let grasp_clone_url = format!("{grasp_url}/{npub}/{IDENTIFIER}.git");
    let listing = ls_remote(&repo, &grasp_clone_url).await?;
    assert_eq!(
        listing.get(&format!("refs/tags/{TAG_NAME}")),
        Some(&tag_object),
        "grasp clone URL should advertise refs/tags/{TAG_NAME} at origin's \
         tag-object oid {tag_object}; full listing: {listing:?}",
    );
    if let Some(peeled) = listing.get(&format!("refs/tags/{TAG_NAME}^{{}}")) {
        assert_eq!(
            peeled, &c2,
            "peeled refs/tags/{TAG_NAME}^{{}} on the grasp should resolve to C2 {c2}",
        );
    }
    assert_eq!(
        listing.get("refs/heads/main"),
        Some(&c2),
        "grasp should hold main at origin's listed tip C2 {c2} (not the \
         local repo's stale C1 {c1}); full listing: {listing:?}",
    );

    // --- assert (b): the kind-30618 state event includes the tag -------------
    let state_event = fetch_state_event(&harness, &npub).await?;
    assert_eq!(
        tag_value(&state_event, &format!("refs/tags/{TAG_NAME}")).as_deref(),
        Some(tag_object.as_str()),
        "state event should list refs/tags/{TAG_NAME} at the tag-object oid; \
         tags: {:?}",
        state_event.tags,
    );
    if let Some(peeled) = tag_value(&state_event, &format!("refs/tags/{TAG_NAME}^{{}}")) {
        assert_eq!(
            peeled, c2,
            "state event's peeled refs/tags/{TAG_NAME}^{{}} should resolve to C2 {c2}",
        );
    }
    assert_eq!(
        tag_value(&state_event, "refs/heads/main").as_deref(),
        Some(c2.as_str()),
        "state event should list main at origin's tip C2; tags: {:?}",
        state_event.tags,
    );

    // --- assert (c): objects fetched into the odb, but no local tag ref ------
    assert!(
        object_in_odb(&repo, &tag_object).await?,
        "the tag object {tag_object} should have been fetched from origin \
         into the local odb during init"
    );
    assert!(
        object_in_odb(&repo, &c2).await?,
        "C2 {c2} should have been fetched from origin into the local odb during init"
    );
    let snapshot = repo.snapshot()?;
    assert!(
        !snapshot.refs.contains_key(&format!("refs/tags/{TAG_NAME}")),
        "init must not create a local refs/tags/{TAG_NAME} ref (objects land \
         in the odb only; server refs stay dynamic); refs: {:?}",
        snapshot.refs,
    );
    assert_eq!(
        snapshot.refs.get("refs/heads/main"),
        Some(&c1),
        "init must not move the local main branch off C1",
    );

    // --- assert (d): origin repointed, old URL preserved ---------------------
    let origin_after = repo
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing after init")?;
    assert!(
        origin_after.starts_with("nostr://"),
        "origin should now hold the nostr URL; got {origin_after:?}",
    );
    assert!(
        origin_after.contains(IDENTIFIER),
        "origin's nostr URL should reference the identifier; got {origin_after:?}",
    );
    // `derive_remote_name_from_url` dash-joins every octet of an IP host,
    // so `http://127.0.0.1:<port>` is preserved as remote `127-0-0-1`.
    let preserved = repo
        .config("remote.127-0-0-1.url")
        .await?
        .context("old origin URL was not preserved under remote '127-0-0-1'")?;
    assert_eq!(
        preserved, origin_url,
        "preserved remote should carry the pre-init origin URL",
    );

    Ok(())
}

/// Whether the repo's odb holds `oid` (any object type) — `git cat-file -e`.
async fn object_in_odb(repo: &Repo, oid: &str) -> Result<bool> {
    let out = repo
        .git(["cat-file", "-e", oid])
        .output()
        .await
        .with_context(|| format!("failed to spawn git cat-file -e {oid}"))?;
    Ok(out.status.success())
}

/// `git ls-remote <url>` parsed into a ref-name → oid map. Peeled entries
/// (`refs/tags/<name>^{}`) appear as their own keys when the server
/// provides them.
async fn ls_remote(repo: &Repo, url: &str) -> Result<HashMap<String, String>> {
    let out = repo
        .git(["ls-remote", url])
        .output()
        .await
        .with_context(|| format!("failed to spawn git ls-remote {url}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git ls-remote {url} exited non-zero ({:?})\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let stdout = String::from_utf8(out.stdout).context("ls-remote output is not valid UTF-8")?;
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some((name.trim().to_string(), oid.trim().to_string()))
        })
        .collect())
}

/// The current kind-30618 for `(author, IDENTIFIER)` on the grasp's relay
/// surface — the NIP-01 winner if the relay somehow retained several.
async fn fetch_state_event(harness: &Harness, npub: &str) -> Result<Event> {
    let author = PublicKey::parse(npub).context("failed to re-parse npub")?;
    harness
        .grasp("repo")
        .events(Filter::new().author(author).kind(KIND_REPO_STATE))
        .await?
        .into_iter()
        .filter(|e| tag_value(e, "d").as_deref() == Some(IDENTIFIER))
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.cmp(&left.id))
        })
        .with_context(|| {
            format!(
                "no kind-30618 state event with d tag {IDENTIFIER:?} on the \
                 grasp relay after `ngit init` — the origin-state transaction \
                 did not publish"
            )
        })
}
