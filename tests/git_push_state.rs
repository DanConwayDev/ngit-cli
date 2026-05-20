//! End-to-end coverage of `git push` to a `nostr://` remote against a
//! **manually-published** kind-30617 announcement, where the publisher
//! constructs and signs the announcement themselves rather than delegating
//! to `ngit init`.
//!
//! Why bypass `ngit init`? `ngit init` runs `git push` itself as part of
//! its setup flow (see `src/bin/ngit/sub_commands/init.rs` and the
//! purgatory short-circuit at `init.rs:1195`). Driving the announcement
//! by hand decouples the push under test from any push that `init` itself
//! emits, so a regression in either path doesn't mask the other.
//!
//! ## Topology
//!
//! - Two GRASP servers (roles `"repo1"` / `"repo2"`) — both git server and
//!   repo-relay, both listed in the announcement's `clone` and `relays` tags.
//!   After push, each must end up with the bare repo's `refs/heads/main`
//!   advanced to the publisher's commit, and each must carry the kind-30618
//!   state event.
//! - One vanilla relay (role `"default"`) — doubles as (a) the user-relay that
//!   `ngit account create` publishes kind 0 / kind 10002 to and (b) the
//!   "non-grasp standard relay" listed in the repo announcement's `relays` tag,
//!   so we can assert the state event lands on a non-GRASP surface too.
//!
//! ## Assertions (in order)
//!
//! 1. The publisher's `refs/remotes/origin/main` advances to the local commit
//!    after `git push -u origin main`.
//! 2. A fresh `git clone <nostr-url>` reproduces `refs/heads/main` at the same
//!    OID.
//! 3. Direct `git clone http://<grasp>/<npub>/<id>.git` against *both* grasps'
//!    smart-http endpoints succeeds and yields the same OID.
//! 4. The kind-30618 state event lands on both grasps **and** the vanilla relay
//!    listed in the announcement.
//! 5. The state event carries `HEAD: ref: refs/heads/main` and
//!    `refs/heads/main: <oid>` tags, matching the local commit.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{Harness, clock};

/// `STATE_KIND` (`Kind::Custom(30618)`) mirrored locally to keep the test
/// crate free of an ngit-lib dep.
const STATE_KIND: u16 = 30618;

/// The single source of truth for the default branch name across this
/// test. `Repo::init` runs `git init -b main` so the local repo always
/// uses `main`.
const DEFAULT_BRANCH: &str = "main";

#[tokio::test]
async fn manual_announcement_then_git_push_propagates_refs_and_state() -> Result<()> {
    // ---------- harness: 2 grasps + 1 vanilla relay -------------------------
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo1")
    .with_grasp_server("repo2")
    .build()
    .await?;

    // ---------- publisher: account + commit on `main` -----------------------
    let publisher = harness.fresh_repo()?;
    let display_name = "git push state test";
    let identifier = "git-push-state-test";

    let create_out = publisher
        .ngit(["account", "create", "--local", "--name", display_name])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    assert!(
        create_out.status.success(),
        "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        create_out.status,
        String::from_utf8_lossy(&create_out.stdout),
        String::from_utf8_lossy(&create_out.stderr),
    );

    let nsec = publisher
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    let keys = Keys::parse(&nsec).context("invalid nsec in local config")?;
    let pubkey = keys.public_key();
    let npub = pubkey
        .to_bech32()
        .context("failed to bech32-encode publisher pubkey")?;

    // seed commit: pick a unique-ish body so a misrouted clone is obvious
    let seed_filename = "README.md";
    let seed_content = "hello, manual announcement!\n";
    std::fs::write(publisher.dir().join(seed_filename), seed_content)
        .context("failed to write seed file in publisher repo")?;
    check_ok(
        "git add",
        publisher
            .git(["add", seed_filename])
            .output()
            .await
            .context("failed to spawn git add")?,
    );
    check_ok(
        "git commit",
        publisher
            .git(["commit", "-m", "initial", "--no-gpg-sign"])
            .output()
            .await
            .context("failed to spawn git commit")?,
    );

    let snapshot_before = publisher.snapshot()?;
    let local_branch_ref = format!("refs/heads/{DEFAULT_BRANCH}");
    let local_oid = snapshot_before
        .refs
        .get(&local_branch_ref)
        .with_context(|| format!("{local_branch_ref} missing after initial commit"))?
        .clone();

    // ---------- manual kind-30617 announcement ------------------------------
    //
    // Modelled on `src/lib/repo_ref.rs::RepoRef::to_event` so the tag shape
    // is one production code paths already parse. The two grasps' clone
    // URLs use the `http://host:port/<npub>/<identifier>.git` layout that
    // ngit-grasp's announcement policy provisions
    // (`<git_data_path>/<npub>/<identifier>.git`), and the relays list
    // includes both grasp ws endpoints **plus** the standalone vanilla
    // relay so we can later assert the state event lands on all three.

    let grasp1 = harness.grasp("repo1");
    let grasp2 = harness.grasp("repo2");
    let standard_relay = harness.relay("default");

    let grasp1_clone_url = format!("{}/{}/{}.git", grasp1.url(), npub, identifier);
    let grasp2_clone_url = format!("{}/{}/{}.git", grasp2.url(), npub, identifier);
    let standard_relay_url = standard_relay.url().to_string();
    let grasp1_relay_url = grasp1.relay_url();
    let grasp2_relay_url = grasp2.relay_url();

    let root_commit_short = local_oid[..7].to_string();

    let announcement_tags: Vec<Tag> = vec![
        Tag::identifier(identifier.to_string()),
        Tag::custom(
            TagKind::Custom("r".into()),
            vec![local_oid.clone(), "euc".to_string()],
        ),
        Tag::custom(
            TagKind::Custom("name".into()),
            vec![display_name.to_string()],
        ),
        Tag::custom(
            TagKind::Custom("description".into()),
            vec!["test repo for git push state assertions".to_string()],
        ),
        Tag::custom(
            TagKind::Custom("clone".into()),
            vec![grasp1_clone_url.clone(), grasp2_clone_url.clone()],
        ),
        Tag::custom(TagKind::Custom("web".into()), Vec::<String>::new()),
        Tag::custom(
            TagKind::Custom("relays".into()),
            vec![
                standard_relay_url.clone(),
                grasp1_relay_url.clone(),
                grasp2_relay_url.clone(),
            ],
        ),
        Tag::custom(
            TagKind::Custom("maintainers".into()),
            vec![pubkey.to_string()],
        ),
        Tag::custom(
            TagKind::Custom("alt".into()),
            vec![format!("git repository: {display_name}")],
        ),
    ];
    // Touch `root_commit_short` so a future refactor that wants to use the
    // commit prefix as the identifier (the ngit default) can do so without
    // a dead-code warning shuffle.
    let _ = root_commit_short;

    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(announcement_tags)
        .sign_with_keys(&keys)
        .context("failed to sign repo announcement")?;

    // Publish to all three surfaces. Each grasp's announcement policy
    // accepts because its own ws URL is in the relays tag and its own
    // clone URL is in the clone tag; the vanilla relay accepts unconditionally.
    publish_event_to_all(
        &announcement,
        &[
            grasp1_relay_url.as_str(),
            grasp2_relay_url.as_str(),
            standard_relay_url.as_str(),
        ],
    )
    .await?;
    // Tick after a manual publish so the next event (the state event
    // emitted by `git push` below) lands in a strictly later unix second
    // and can't id-collide. See `test_harness::clock` for the writeup.
    clock::tick_to_next_second().await;

    // ---------- wait for the grasps to materialise the bare repos -----------
    //
    // ngit-grasp's announcement policy creates `<root>/<npub>/<id>.git`
    // synchronously on receipt, but the relay ACK fires before the
    // filesystem op is observable from this process — poll briefly.
    let bare1 = grasp1
        .git_data_path()
        .join(&npub)
        .join(format!("{identifier}.git"));
    let bare2 = grasp2
        .git_data_path()
        .join(&npub)
        .join(format!("{identifier}.git"));
    wait_for_path(&bare1, Duration::from_secs(5)).await?;
    wait_for_path(&bare2, Duration::from_secs(5)).await?;

    // ---------- add the nostr:// remote and push ----------------------------
    //
    // Build the nostr URL by hand to match the format
    // `NostrUrlDecoded`'s Display impl produces (and `parse_and_resolve`
    // accepts): `nostr://<npub>/<urlencoded-ws-relay>/<identifier>`. Use
    // the vanilla relay as the hint — the announcement is there, the
    // relay is reachable for the cloner, and an http-style local URL
    // doesn't need any `wss://`-stripping magic the Display impl does
    // for production wss URLs.
    let relay_hint = urlencoding::encode(standard_relay.url()).into_owned();
    let nostr_url = format!("nostr://{npub}/{relay_hint}/{identifier}");

    check_ok(
        "git remote add origin <nostr-url>",
        publisher
            .git(["remote", "add", "origin", &nostr_url])
            .output()
            .await
            .context("failed to spawn git remote add origin")?,
    );

    // `Repo::nostr_push` runs `git push <args>` then ticks one whole unix
    // second — see `test_harness::clock` for why bare `git push` is
    // forbidden against a nostr remote. `-u` writes the upstream
    // tracking config so subsequent `git push` calls in this repo would
    // work without re-specifying the ref.
    publisher
        .nostr_push(["-u", "origin", DEFAULT_BRANCH])
        .await
        .context("git push -u origin main")?;

    // ===================================================================
    // Assertion 1 — local refs/remotes/origin/main matches the local oid
    // ===================================================================
    let snapshot_after = publisher.snapshot()?;
    let remote_tracking_ref = format!("refs/remotes/origin/{DEFAULT_BRANCH}");
    let local_remote_tracking_oid =
        snapshot_after
            .refs
            .get(&remote_tracking_ref)
            .with_context(|| {
                format!(
                    "{remote_tracking_ref} missing after git push -u — \
                 push went through but git did not update the remote-tracking ref"
                )
            })?;
    assert_eq!(
        local_remote_tracking_oid, &local_oid,
        "publisher's {remote_tracking_ref} ({local_remote_tracking_oid}) does \
         not match local {local_branch_ref} ({local_oid})"
    );

    // Also confirm the upstream tracking config was actually written by
    // `-u`. Catches a regression where the helper accepts the push but
    // git-config is somehow left untouched.
    let merge_cfg = publisher
        .config(&format!("branch.{DEFAULT_BRANCH}.merge"))
        .await?
        .with_context(|| {
            format!(
                "branch.{DEFAULT_BRANCH}.merge missing — `git push -u` did \
                 not set upstream tracking"
            )
        })?;
    assert_eq!(merge_cfg, local_branch_ref);

    // ===================================================================
    // Assertion 2 — fresh `git clone <nostr-url>` reproduces the ref
    // ===================================================================
    let cloner = harness.fresh_repo()?;
    let clone_subdir = "cloned-via-nostr";
    let clone_target = cloner.dir().join(clone_subdir);
    let clone_out = cloner
        .git(["clone", &nostr_url, clone_subdir])
        .output()
        .await
        .context("failed to spawn git clone <nostr-url>")?;
    assert!(
        clone_out.status.success(),
        "git clone <nostr-url> exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        clone_out.status,
        String::from_utf8_lossy(&clone_out.stdout),
        String::from_utf8_lossy(&clone_out.stderr),
    );
    let cloned_oid = read_local_ref_oid(&clone_target, &local_branch_ref)
        .with_context(|| format!("reading {local_branch_ref} from nostr-clone"))?;
    assert_eq!(
        cloned_oid, local_oid,
        "nostr clone's {local_branch_ref} ({cloned_oid}) does not match \
         publisher's local ({local_oid})"
    );

    // ===================================================================
    // Assertion 3 — direct `git clone http://<grasp>/...` against *both*
    // grasps yields the same ref. Uses a vanilla `tempfile::TempDir` (no
    // harness env needed: we're talking plain smart-http, no remote
    // helper) so we exercise the grasp's git server independently of
    // ngit's discovery code path.
    // ===================================================================
    for (label, http_url) in [("grasp1", &grasp1_clone_url), ("grasp2", &grasp2_clone_url)] {
        // `harness.fresh_repo()` gives a tempdir with a benign git
        // identity already configured. We don't reuse it as a git repo;
        // the `git clone http://...` lands in a sibling subdir and uses
        // plain smart-http, no remote helper involved.
        let host = harness
            .fresh_repo()
            .with_context(|| format!("fresh_repo for direct {label} clone"))?;
        let subdir = format!("direct-{label}");
        let direct_out = host
            .git(["clone", http_url, &subdir])
            .output()
            .await
            .with_context(|| format!("failed to spawn direct {label} clone"))?;
        assert!(
            direct_out.status.success(),
            "direct {label} clone of {http_url} exited non-zero ({:?})\n\
             stdout: {}\nstderr: {}",
            direct_out.status,
            String::from_utf8_lossy(&direct_out.stdout),
            String::from_utf8_lossy(&direct_out.stderr),
        );
        let direct_oid = read_local_ref_oid(&host.dir().join(&subdir), &local_branch_ref)
            .with_context(|| format!("reading {local_branch_ref} from direct {label} clone"))?;
        assert_eq!(
            direct_oid, local_oid,
            "direct {label} clone's {local_branch_ref} ({direct_oid}) \
             does not match publisher's local ({local_oid})"
        );
    }

    // ===================================================================
    // Assertion 4 — state event lands on all three relay surfaces
    // ===================================================================
    let state_filter = || Filter::new().author(pubkey).kind(Kind::Custom(STATE_KIND));
    let grasp1_state = grasp1.events(state_filter()).await?;
    let grasp2_state = grasp2.events(state_filter()).await?;
    let relay_state = standard_relay.events(state_filter()).await?;
    assert!(
        !grasp1_state.is_empty(),
        "no kind {STATE_KIND} state event on grasp1 after push"
    );
    assert!(
        !grasp2_state.is_empty(),
        "no kind {STATE_KIND} state event on grasp2 after push"
    );
    assert!(
        !relay_state.is_empty(),
        "no kind {STATE_KIND} state event on the vanilla repo-relay after push"
    );

    // The state event for this repo's identifier is replaceable, so all
    // three relays should converge on the same id. Cross-check that the
    // grasps and the vanilla relay aren't somehow holding different
    // state events.
    let canonical = pick_state_event(&grasp1_state, identifier);
    let canonical = canonical.context("no state event with the expected `d` tag on grasp1")?;
    let on_grasp2 = pick_state_event(&grasp2_state, identifier)
        .context("no state event with the expected `d` tag on grasp2")?;
    let on_relay = pick_state_event(&relay_state, identifier)
        .context("no state event with the expected `d` tag on the vanilla relay")?;
    assert_eq!(
        canonical.id, on_grasp2.id,
        "state events on grasp1 and grasp2 differ — push did not converge"
    );
    assert_eq!(
        canonical.id, on_relay.id,
        "state events on grasp1 and the vanilla relay differ — push did not converge"
    );

    // ===================================================================
    // Assertion 5 — state event lists HEAD + the default branch correctly
    // ===================================================================
    let head_value = tag_value(canonical, "HEAD")
        .context("state event missing a HEAD tag — default-branch HEAD is required")?;
    assert_eq!(
        head_value,
        format!("ref: {local_branch_ref}"),
        "state event HEAD tag {head_value:?} does not point at {local_branch_ref}"
    );
    let branch_value = tag_value(canonical, &local_branch_ref).with_context(|| {
        format!("state event missing a {local_branch_ref} tag — the pushed ref")
    })?;
    assert_eq!(
        branch_value, local_oid,
        "state event {local_branch_ref} tag {branch_value} does not match \
         local oid {local_oid}"
    );

    Ok(())
}

// ---------- helpers ---------------------------------------------------------

/// `git rev-parse <ref>` against a working tree on disk via `git2`.
fn read_local_ref_oid(working_path: &Path, refname: &str) -> Result<String> {
    let repo = git2::Repository::open(working_path)
        .with_context(|| format!("open {}", working_path.display()))?;
    let reference = repo
        .find_reference(refname)
        .with_context(|| format!("find_reference {refname}"))?;
    let oid = reference
        .target()
        .with_context(|| format!("reference {refname} has no direct target"))?;
    Ok(oid.to_string())
}

/// Poll for `path` to exist, with a short ceiling — the grasp's
/// announcement policy creates the bare repo synchronously on receipt but
/// the relay ACK can return before the filesystem op is visible.
async fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while !path.is_dir() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out after {:?} waiting for {} to be created — \
                 did the grasp accept the announcement?",
                timeout,
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Publish `event` to every relay URL in `urls`, bailing if any relay
/// rejects it. A single `Client` is used for the whole fan-out so the
/// `send_event_to` call can ACK-or-fail per relay without us having to
/// thread that around per-target.
async fn publish_event_to_all(event: &Event, urls: &[&str]) -> Result<()> {
    let client = Client::default();
    for url in urls {
        client
            .add_relay(*url)
            .await
            .with_context(|| format!("add_relay {url}"))?;
    }
    client.connect().await;
    let output = client
        .send_event_to(urls.iter().copied(), event)
        .await
        .context("send_event_to fan-out")?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        anyhow::bail!(
            "one or more relays rejected announcement event id={}: {:?}",
            event.id,
            output.failed
        );
    }
    Ok(())
}

/// Pick the (single) state event whose `d` tag matches `identifier`.
/// Returns `None` rather than panicking so the caller can produce a
/// pinpoint error message.
fn pick_state_event<'a>(events: &'a [Event], identifier: &str) -> Option<&'a Event> {
    events
        .iter()
        .find(|e| tag_value(e, "d").as_deref() == Some(identifier))
}

/// First value of the `[<name>, <value>, ...]` tag on `event`, if present.
fn tag_value(event: &Event, name: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(name) {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

/// Assert that `out.status.success()`, dumping captured stdio on failure.
fn check_ok(label: &str, out: std::process::Output) {
    assert!(
        out.status.success(),
        "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
