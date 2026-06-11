//! End-to-end coverage of `git push` deleting a previously-pushed
//! branch (`vnext`) from a repository that still has `main`.
//!
//! Builds on the [`super::add_branch`] scenario: same manual kind-30617
//! announcement, same `git push -u origin main` followed by
//! `git push -u origin vnext`, but then issues a third push
//! `git push origin --delete vnext` to remove the second branch. The
//! kind-30618 state event the helper emits after the delete-push should
//! list `main` only — that's the central "deleted branch drops out of
//! the state event" property under test, alongside the supporting
//! "main is undisturbed" and "HEAD stays on main" guarantees.
//!
//! Why bypass `ngit init`? Same reason as the fresh-repo scenario —
//! `ngit init` runs its own `git push`, and we want the pushes under
//! test to be the only ones in play. See the module doc-comment on
//! [`super::fresh_repo`] for the full rationale.
//!
//! ## Timing
//!
//! The fixture issues three pushes to a nostr remote — `main` first,
//! then `vnext`, then `--delete vnext`. Each push emits an
//! auto-generated kind-30618 state event; each subsequent push
//! *replaces* the previous one at the replaceable coordinate. All
//! pushes go through [`test_harness::Repo::nostr_push`] (never bare
//! `git push`), which ticks one whole unix second before each push so
//! the follow-up event lands in a strictly later `created_at` second
//! and cannot id-collide with the previous one. See
//! [`test_harness::clock`] for the writeup.
//!
//! ## rstest discipline
//!
//! Mirrors [`super::add_branch`]'s structure: one [`Snapshot`] captured
//! once via a module-local [`tokio::sync::OnceCell`], one
//! `#[rstest] #[tokio::test]` per asserted property so failures name
//! the broken property in `cargo test` output.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use nostr::event::FinalizeEvent;
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, RepoSnapshot};
use tokio::sync::OnceCell;

/// `STATE_KIND` (`Kind::Custom(30618)`) mirrored locally to keep the test
/// crate free of an ngit-lib dep.
const STATE_KIND: u16 = 30618;

/// The repo's default branch. `Repo::init` runs `git init -b main`, and
/// the fixture deliberately leaves HEAD on `main` before pushing the
/// second branch so this remains the published default.
const DEFAULT_BRANCH: &str = "main";

/// The second branch added on top of the fresh-repo end state.
const SECOND_BRANCH: &str = "vnext";

/// Captured side-effects of a three-push flow: announce +
/// `git push main`, `git push vnext`, then `git push --delete vnext`.
/// All fields are owned so the harness and its tempdirs can drop after
/// the fixture returns.
struct Snapshot {
    /// Publisher's working tree captured **after** all three pushes
    /// complete. Still holds the *local* `refs/heads/{main,vnext}`
    /// (deleting a remote branch does not remove the local branch),
    /// but `refs/remotes/origin/vnext` must be gone; `head` is the
    /// publisher's `HEAD` symref target (must be `refs/heads/main`).
    publisher: RepoSnapshot,
    /// `branch.main.merge` from the publisher's local git config — set
    /// by the *first* push's `-u` and asserted to survive both the
    /// add-vnext and delete-vnext pushes.
    upstream_merge_cfg_main: String,
    /// `branch.vnext.merge` from the publisher's local git config — set
    /// by the second push's `-u`. `git push --delete` only removes the
    /// remote ref and the remote-tracking ref; this local config entry
    /// is left alone, so we still expect to find it.
    upstream_merge_cfg_vnext: String,
    /// Working tree of `git clone <nostr-url>` after all three pushes.
    /// `main` is the default so it lands at `refs/heads/main`; vnext
    /// must not appear in any form.
    nostr_clone: RepoSnapshot,
    /// Direct `git clone http://grasp1/.../...git` after all three
    /// pushes. Must not carry any vnext ref.
    grasp1_clone: RepoSnapshot,
    /// Direct `git clone http://grasp2/.../...git` after all three
    /// pushes. Must not carry any vnext ref.
    grasp2_clone: RepoSnapshot,
    /// The kind-30618 state event as the *first grasp* sees it — i.e.
    /// the replaceable event that survived the delete-vnext push. Used
    /// as the canonical version for tag-content assertions; grasp2 and
    /// the vanilla relay must report the same event id.
    state_event_grasp1: Event,
    /// `state_event_grasp1`'s twin on the second grasp.
    state_event_grasp2: Event,
    /// `state_event_grasp1`'s twin on the vanilla relay listed in the
    /// announcement.
    state_event_vanilla: Event,
    /// `refs/heads/main` — formatted once so cases don't re-stringify.
    main_branch_ref: String,
    /// `refs/heads/vnext` — same.
    vnext_branch_ref: String,
}

/// Global, lazily-initialised snapshot for this scenario. Module-local
/// to [`delete_branch`] — does not share state with the
/// [`super::add_branch`] or [`super::fresh_repo`] scenarios' snapshots.
static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("delete_branch fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

/// Drive the entire setup: harness, account, commit on `main`, manual
/// announcement, `git push -u origin main`, branch + commit on `vnext`,
/// `git push -u origin vnext`, `git push origin --delete vnext`, two
/// clones, three state-event queries.
async fn capture_snapshot() -> Result<Snapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo1")
    .with_grasp_server("repo2")
    .build()
    .await?;

    // ---------- publisher: account + commit on `main` -------------------
    let publisher = harness.fresh_repo()?;
    let display_name = "git push delete-branch test";
    let identifier = "git-push-delete-branch-test";

    let create_out = publisher
        .ngit(["account", "create", "--local", "--name", display_name])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    require_success("ngit account create", &create_out)?;

    let nsec = publisher
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    let keys = Keys::parse(&nsec).context("invalid nsec in local config")?;
    let pubkey = keys.public_key();
    let npub = pubkey
        .to_bech32()
        .context("failed to bech32-encode publisher pubkey")?;

    let main_branch_ref = format!("refs/heads/{DEFAULT_BRANCH}");
    let vnext_branch_ref = format!("refs/heads/{SECOND_BRANCH}");

    // Seed commit on `main` with a deterministic body so a misrouted
    // clone is obvious.
    let seed_filename = "README.md";
    let seed_content = "hello, add-branch scenario!\n";
    std::fs::write(publisher.dir().join(seed_filename), seed_content)
        .context("failed to write seed file in publisher repo")?;
    require_success(
        "git add README.md",
        &publisher
            .git(["add", seed_filename])
            .output()
            .await
            .context("failed to spawn git add")?,
    )?;
    require_success(
        "git commit initial",
        &publisher
            .git(["commit", "-m", "initial", "--no-gpg-sign"])
            .output()
            .await
            .context("failed to spawn git commit")?,
    )?;

    // Seed-commit oid — needed up-front for the `r euc` announcement
    // tag and re-asserted post-vnext-push to confirm `main` was not
    // disturbed by the second push.
    let main_oid = publisher
        .snapshot()?
        .refs
        .get(&main_branch_ref)
        .with_context(|| format!("{main_branch_ref} missing after initial commit"))?
        .clone();

    // ---------- manual kind-30617 announcement --------------------------
    //
    // Modelled on `src/lib/repo_ref.rs::RepoRef::to_event`. Same shape
    // as the fresh-repo scenario — see that module's doc-comment for
    // why every tag is the way it is.
    let grasp1 = harness.grasp("repo1");
    let grasp2 = harness.grasp("repo2");
    let standard_relay = harness.relay("default");

    let grasp1_clone_url = format!("{}/{}/{}.git", grasp1.url(), npub, identifier);
    let grasp2_clone_url = format!("{}/{}/{}.git", grasp2.url(), npub, identifier);
    let standard_relay_url = standard_relay.url().to_string();
    let grasp1_relay_url = grasp1.relay_url();
    let grasp2_relay_url = grasp2.relay_url();

    let announcement_tags: Vec<Tag> = vec![
        Tag::identifier(identifier.to_string()),
        Tag::parse(["r".to_string(), main_oid.clone(), "euc".to_string()]).unwrap(),
        Tag::parse(["name".to_string(), display_name.to_string()]).unwrap(),
        Tag::parse([
            "description".to_string(),
            "test repo for git push delete-branch assertions".to_string(),
        ])
        .unwrap(),
        Tag::parse([
            "clone".to_string(),
            grasp1_clone_url.clone(),
            grasp2_clone_url.clone(),
        ])
        .unwrap(),
        Tag::parse(["web".to_string()]).unwrap(),
        Tag::parse([
            "relays".to_string(),
            standard_relay_url.clone(),
            grasp1_relay_url.clone(),
            grasp2_relay_url.clone(),
        ])
        .unwrap(),
        Tag::parse(["maintainers".to_string(), pubkey.to_string()]).unwrap(),
        Tag::parse(["alt".to_string(), format!("git repository: {display_name}")]).unwrap(),
    ];

    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(announcement_tags)
        .finalize(&keys)
        .context("failed to sign repo announcement")?;

    publish_event_to_all(
        &announcement,
        &[
            grasp1_relay_url.as_str(),
            grasp2_relay_url.as_str(),
            standard_relay_url.as_str(),
        ],
    )
    .await?;

    // ---------- wait for the grasps to materialise the bare repos -------
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

    // ---------- add the nostr:// remote ---------------------------------
    let relay_hint = urlencoding::encode(standard_relay.url()).into_owned();
    let nostr_url = format!("nostr://{npub}/{relay_hint}/{identifier}");

    require_success(
        "git remote add origin <nostr-url>",
        &publisher
            .git(["remote", "add", "origin", &nostr_url])
            .output()
            .await
            .context("failed to spawn git remote add origin")?,
    )?;

    // ---------- first push: `main` --------------------------------------
    //
    // `Repo::nostr_push` runs `git push <args>` then ticks one whole
    // unix second so the second push's auto state event lands in a
    // strictly later created_at second than this one's and can't
    // id-collide. `-u` writes `branch.main.merge` into local config.
    publisher
        .nostr_push(["-u", "origin", DEFAULT_BRANCH])
        .await
        .context("git push -u origin main")?;

    // ---------- create `vnext` with an extra commit ---------------------
    //
    // `checkout -b vnext` switches HEAD to vnext while creating it;
    // commit-on-vnext gives us a distinct oid for that branch; then
    // `checkout main` restores HEAD before the second push so the state
    // event the helper writes still names `main` as the default branch.
    require_success(
        "git checkout -b vnext",
        &publisher
            .git(["checkout", "-b", SECOND_BRANCH])
            .output()
            .await
            .context("failed to spawn git checkout -b vnext")?,
    )?;
    let feature_filename = "FEATURE.md";
    let feature_content = "vnext branch feature work\n";
    std::fs::write(publisher.dir().join(feature_filename), feature_content)
        .context("failed to write feature file on vnext")?;
    require_success(
        "git add FEATURE.md",
        &publisher
            .git(["add", feature_filename])
            .output()
            .await
            .context("failed to spawn git add FEATURE.md")?,
    )?;
    require_success(
        "git commit vnext",
        &publisher
            .git(["commit", "-m", "vnext: add feature", "--no-gpg-sign"])
            .output()
            .await
            .context("failed to spawn git commit on vnext")?,
    )?;
    // Capture vnext oid for the post-push self-check below, and switch
    // HEAD back to main so the upcoming state event publishes
    // `HEAD = refs/heads/main`.
    let vnext_oid = publisher
        .snapshot()?
        .refs
        .get(&vnext_branch_ref)
        .with_context(|| format!("{vnext_branch_ref} missing after commit on vnext"))?
        .clone();
    require_success(
        "git checkout main",
        &publisher
            .git(["checkout", DEFAULT_BRANCH])
            .output()
            .await
            .context("failed to spawn git checkout main")?,
    )?;

    // ---------- second push: `vnext` ------------------------------------
    publisher
        .nostr_push(["-u", "origin", SECOND_BRANCH])
        .await
        .context("git push -u origin vnext")?;

    // ---------- third push: delete `vnext` from the remote --------------
    //
    // `git push origin --delete <branch>` removes the remote ref on
    // `origin` and the matching `refs/remotes/origin/<branch>` locally,
    // but leaves the local `refs/heads/<branch>` and the
    // `branch.<branch>.merge` config entry untouched — that asymmetry
    // is part of what we're asserting below. `nostr_push` ticks one
    // whole unix second first so the kind-30618 state event this push
    // emits cannot id-collide with the one from the vnext-add push.
    publisher
        .nostr_push(["origin", "--delete", SECOND_BRANCH])
        .await
        .context("git push origin --delete vnext")?;

    // ---------- capture publisher state ---------------------------------
    let publisher_snap = publisher
        .snapshot()
        .context("capturing publisher snapshot after delete-vnext push")?;

    // Self-checks on what we captured — if the publisher's view of
    // either branch doesn't match what we expect, the rest of the
    // assertions are operating on bad data, so fail fast here. `main`'s
    // local oid must be unchanged; `vnext`'s local branch must still
    // exist (delete only hits the remote); the remote-tracking ref for
    // vnext must be gone.
    let local_main = publisher_snap
        .refs
        .get(&main_branch_ref)
        .with_context(|| format!("{main_branch_ref} missing from publisher post-push snapshot"))?;
    anyhow::ensure!(
        *local_main == main_oid,
        "publisher's {main_branch_ref} drifted from {main_oid} to {local_main} \
         between the initial commit and the delete-vnext push — captured \
         snapshot is no longer a clean fixture for the rest of the cases",
    );
    let local_vnext = publisher_snap
        .refs
        .get(&vnext_branch_ref)
        .with_context(|| {
            format!(
                "{vnext_branch_ref} missing from publisher post-push snapshot \
                 — `git push --delete` should not have removed the local \
                 branch, only the remote one"
            )
        })?;
    anyhow::ensure!(
        *local_vnext == vnext_oid,
        "publisher's {vnext_branch_ref} drifted from {vnext_oid} to {local_vnext} \
         between commit and snapshot",
    );
    let publisher_vnext_remote_tracking = format!("refs/remotes/origin/{SECOND_BRANCH}");
    anyhow::ensure!(
        !publisher_snap
            .refs
            .contains_key(&publisher_vnext_remote_tracking),
        "publisher still has {publisher_vnext_remote_tracking} after \
         `git push --delete vnext` — the delete-push did not remove the \
         remote-tracking ref",
    );

    let upstream_merge_cfg_main = publisher
        .config(&format!("branch.{DEFAULT_BRANCH}.merge"))
        .await?
        .with_context(|| {
            format!(
                "branch.{DEFAULT_BRANCH}.merge missing — first push's `-u` \
                 did not set upstream tracking, or a later push wiped it"
            )
        })?;
    let upstream_merge_cfg_vnext = publisher
        .config(&format!("branch.{SECOND_BRANCH}.merge"))
        .await?
        .with_context(|| {
            format!(
                "branch.{SECOND_BRANCH}.merge missing — second push's `-u` did \
                 not set upstream tracking, or the delete-vnext push wiped it \
                 (it should not — `git push --delete` only removes the remote \
                 and remote-tracking refs, not local branch config)"
            )
        })?;

    // ---------- nostr clone --------------------------------------------
    let cloner = harness.fresh_repo()?;
    let nostr_clone_subdir = "cloned-via-nostr";
    let nostr_clone_out = cloner
        .git(["clone", &nostr_url, nostr_clone_subdir])
        .output()
        .await
        .context("failed to spawn git clone <nostr-url>")?;
    require_success("git clone <nostr-url>", &nostr_clone_out)?;
    let nostr_clone = RepoSnapshot::capture(&cloner.dir().join(nostr_clone_subdir))
        .context("capturing nostr clone snapshot")?;

    // ---------- direct grasp clones (plain smart-http, no helper) -------
    let host1 = harness
        .fresh_repo()
        .context("fresh_repo for direct grasp1 clone")?;
    let host2 = harness
        .fresh_repo()
        .context("fresh_repo for direct grasp2 clone")?;
    let grasp1_clone = clone_via_http_and_snapshot(host1.dir(), &grasp1_clone_url)
        .await
        .context("direct grasp1 clone")?;
    let grasp2_clone = clone_via_http_and_snapshot(host2.dir(), &grasp2_clone_url)
        .await
        .context("direct grasp2 clone")?;

    // ---------- state-event queries ------------------------------------
    //
    // Kind 30618 is replaceable, so each relay returns the single
    // surviving copy at this `(pubkey, kind, d)` coordinate — i.e. the
    // event published by the *delete-vnext* push, which should list
    // `main` only.
    let filter = || Filter::new().author(pubkey).kind(Kind::Custom(STATE_KIND));
    let grasp1_state = grasp1.events(filter()).await?;
    let grasp2_state = grasp2.events(filter()).await?;
    let relay_state = standard_relay.events(filter()).await?;
    let state_event_grasp1 = pick_state_event(&grasp1_state, identifier)
        .context("no state event with the expected `d` tag on grasp1")?
        .clone();
    let state_event_grasp2 = pick_state_event(&grasp2_state, identifier)
        .context("no state event with the expected `d` tag on grasp2")?
        .clone();
    let state_event_vanilla = pick_state_event(&relay_state, identifier)
        .context("no state event with the expected `d` tag on the vanilla relay")?
        .clone();

    Ok(Snapshot {
        publisher: publisher_snap,
        upstream_merge_cfg_main,
        upstream_merge_cfg_vnext,
        nostr_clone,
        grasp1_clone,
        grasp2_clone,
        state_event_grasp1,
        state_event_grasp2,
        state_event_vanilla,
        main_branch_ref,
        vnext_branch_ref,
    })
}

// ---------- cases -----------------------------------------------------------
//
// One `#[rstest] #[tokio::test]` per asserted property. The fresh-repo
// scenario already covers everything a single-branch push must
// guarantee and the add-branch scenario covers what a second branch
// must add; cases here focus on the deltas introduced by the *third*
// push (`git push --delete vnext`): vnext disappears from every
// observable surface (publisher remote-tracking, nostr clone, both
// grasp clones, state event), while `main` is undisturbed, HEAD stays
// on `main`, and the local `vnext` branch + its `branch.vnext.merge`
// config are *not* collateral damage (since `--delete` only acts on
// the remote and remote-tracking refs).

/// Publisher's `HEAD` is still pointing at `refs/heads/main` after the
/// delete-vnext push — the third push did not silently move HEAD.
#[rstest]
#[tokio::test]
async fn publisher_head_still_points_at_main(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let head = s.publisher.head.as_deref().context(
        "publisher snapshot has no HEAD — repo state was somehow unborn after \
         three successful pushes",
    )?;
    assert_eq!(
        head, s.main_branch_ref,
        "publisher HEAD is {head:?}, expected {:?} — delete-vnext push moved \
         the working-tree HEAD away from main",
        s.main_branch_ref,
    );
    Ok(())
}

/// Publisher's `refs/remotes/origin/main` still equals the locally-held
/// `main` oid after the delete-vnext push — deleting a different branch
/// on the remote left main's remote-tracking ref alone.
#[rstest]
#[tokio::test]
async fn publisher_main_remote_tracking_matches_local(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let local = oid_at(&s.publisher, &s.main_branch_ref, "publisher local main")?;
    let remote_tracking_ref = format!("refs/remotes/origin/{DEFAULT_BRANCH}");
    let remote_tracking = oid_at(
        &s.publisher,
        &remote_tracking_ref,
        "publisher remote-tracking main",
    )?;
    assert_eq!(
        remote_tracking, local,
        "publisher's {remote_tracking_ref} ({remote_tracking}) does not match \
         local {} ({local}) — delete-vnext push disturbed main's remote tracking",
        s.main_branch_ref,
    );
    Ok(())
}

/// Publisher's `refs/remotes/origin/vnext` is gone after the
/// delete-vnext push — the third push pruned the remote-tracking ref
/// in lockstep with deleting the remote ref.
#[rstest]
#[tokio::test]
async fn publisher_vnext_remote_tracking_removed(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let remote_tracking_ref = format!("refs/remotes/origin/{SECOND_BRANCH}");
    assert!(
        !s.publisher.refs.contains_key(&remote_tracking_ref),
        "publisher still has {remote_tracking_ref} after \
         `git push origin --delete vnext` — remote-tracking ref was not pruned",
    );
    Ok(())
}

/// Publisher's *local* `refs/heads/vnext` still exists with its
/// previous oid — `git push --delete` removes only the remote and its
/// remote-tracking mirror, not the local branch itself.
#[rstest]
#[tokio::test]
async fn publisher_local_vnext_branch_preserved(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let _ = oid_at(&s.publisher, &s.vnext_branch_ref, "publisher local vnext").context(
        "publisher's local refs/heads/vnext is gone — `git push --delete` \
             should not touch the local branch, only the remote",
    )?;
    Ok(())
}

/// `branch.main.merge` still resolves to `refs/heads/main` — none of
/// the three pushes wiped the upstream tracking config set by the
/// first.
#[rstest]
#[tokio::test]
async fn publisher_main_upstream_tracking_config_preserved(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.upstream_merge_cfg_main, s.main_branch_ref,
        "branch.{DEFAULT_BRANCH}.merge = {:?}, expected {:?}",
        s.upstream_merge_cfg_main, s.main_branch_ref,
    );
    Ok(())
}

/// `branch.vnext.merge = refs/heads/vnext` is still set after the
/// delete-vnext push — `git push --delete` does not clean up local
/// upstream-tracking config, so the second push's `-u` write survives.
#[rstest]
#[tokio::test]
async fn publisher_vnext_upstream_tracking_config_preserved(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.upstream_merge_cfg_vnext, s.vnext_branch_ref,
        "branch.{SECOND_BRANCH}.merge = {:?}, expected {:?} — delete-vnext \
         push cleared local upstream tracking, which it should not do",
        s.upstream_merge_cfg_vnext, s.vnext_branch_ref,
    );
    Ok(())
}

/// `git clone <nostr-url>` reproduces `refs/heads/main` at the
/// publisher's main oid — the default branch survives the delete-vnext
/// push from the cloner's perspective.
#[rstest]
#[tokio::test]
async fn nostr_clone_reproduces_main(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let local = oid_at(&s.publisher, &s.main_branch_ref, "publisher local main")?;
    let cloned = oid_at(&s.nostr_clone, &s.main_branch_ref, "nostr clone main")?;
    assert_eq!(
        cloned, local,
        "nostr clone's {} ({cloned}) does not match publisher's local ({local})",
        s.main_branch_ref,
    );
    Ok(())
}

/// `git clone <nostr-url>` carries no vnext ref in any form — the
/// delete-vnext push removed the branch from the remote, so a fresh
/// clone via the nostr surface must not see it either as a local
/// branch or as a remote-tracking ref.
#[rstest]
#[tokio::test]
async fn nostr_clone_has_no_vnext(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let remote_tracking_ref = format!("refs/remotes/origin/{SECOND_BRANCH}");
    assert!(
        !s.nostr_clone.refs.contains_key(&s.vnext_branch_ref),
        "nostr clone has {} after delete-vnext push — vnext leaked back \
         into a fresh clone via the nostr surface",
        s.vnext_branch_ref,
    );
    assert!(
        !s.nostr_clone.refs.contains_key(&remote_tracking_ref),
        "nostr clone has {remote_tracking_ref} after delete-vnext push — \
         vnext leaked back into a fresh clone via the nostr surface",
    );
    Ok(())
}

/// `git clone <nostr-url>` reports HEAD as `refs/heads/main` — the
/// remote helper preserves the published default branch even after
/// vnext has been deleted from the remote.
#[rstest]
#[tokio::test]
async fn nostr_clone_head_points_at_main(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let head = s.nostr_clone.head.as_deref().context(
        "nostr clone snapshot has no HEAD — clone left the working tree in \
         an unborn state",
    )?;
    assert_eq!(
        head, s.main_branch_ref,
        "nostr clone HEAD is {head:?}, expected {:?}",
        s.main_branch_ref,
    );
    Ok(())
}

/// Direct `git clone http://grasp1/...git` reproduces `refs/heads/main`
/// at the publisher's main oid — independent of the nostr remote
/// helper, the bare repo on grasp1 still has main after the delete-vnext
/// push.
#[rstest]
#[tokio::test]
async fn grasp1_direct_clone_reproduces_main(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let local = oid_at(&s.publisher, &s.main_branch_ref, "publisher local main")?;
    let cloned = oid_at(
        &s.grasp1_clone,
        &s.main_branch_ref,
        "grasp1 direct clone main",
    )?;
    assert_eq!(
        cloned, local,
        "direct grasp1 clone's {} ({cloned}) does not match publisher's local ({local})",
        s.main_branch_ref,
    );
    Ok(())
}

/// Direct `git clone http://grasp1/...git` carries no vnext ref —
/// the delete-vnext push reached grasp1's git server (not just the
/// nostr surface), so the bare repo's vnext is actually gone.
#[rstest]
#[tokio::test]
async fn grasp1_direct_clone_has_no_vnext(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let remote_tracking_ref = format!("refs/remotes/origin/{SECOND_BRANCH}");
    assert!(
        !s.grasp1_clone.refs.contains_key(&s.vnext_branch_ref),
        "direct grasp1 clone has {} after delete-vnext push — the bare \
         repo on grasp1 still carries vnext",
        s.vnext_branch_ref,
    );
    assert!(
        !s.grasp1_clone.refs.contains_key(&remote_tracking_ref),
        "direct grasp1 clone has {remote_tracking_ref} after delete-vnext \
         push — the bare repo on grasp1 still carries vnext",
    );
    Ok(())
}

/// As [`grasp1_direct_clone_reproduces_main`], but for grasp2 — both
/// grasps must reflect the post-delete state of `main`.
#[rstest]
#[tokio::test]
async fn grasp2_direct_clone_reproduces_main(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let local = oid_at(&s.publisher, &s.main_branch_ref, "publisher local main")?;
    let cloned = oid_at(
        &s.grasp2_clone,
        &s.main_branch_ref,
        "grasp2 direct clone main",
    )?;
    assert_eq!(
        cloned, local,
        "direct grasp2 clone's {} ({cloned}) does not match publisher's local ({local})",
        s.main_branch_ref,
    );
    Ok(())
}

/// As [`grasp1_direct_clone_has_no_vnext`], but for grasp2 — both
/// grasps must have dropped the deleted branch.
#[rstest]
#[tokio::test]
async fn grasp2_direct_clone_has_no_vnext(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let remote_tracking_ref = format!("refs/remotes/origin/{SECOND_BRANCH}");
    assert!(
        !s.grasp2_clone.refs.contains_key(&s.vnext_branch_ref),
        "direct grasp2 clone has {} after delete-vnext push — the bare \
         repo on grasp2 still carries vnext",
        s.vnext_branch_ref,
    );
    assert!(
        !s.grasp2_clone.refs.contains_key(&remote_tracking_ref),
        "direct grasp2 clone has {remote_tracking_ref} after delete-vnext \
         push — the bare repo on grasp2 still carries vnext",
    );
    Ok(())
}

/// The state event's id is identical across both grasps after the
/// delete-vnext push — the replacement converged everywhere, so
/// neither grasp is stuck on a stale (vnext-still-present) state event.
#[rstest]
#[tokio::test]
async fn grasps_state_events_agree_on_id(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.state_event_grasp1.id, s.state_event_grasp2.id,
        "state events on grasp1 ({}) and grasp2 ({}) differ after the \
         delete-vnext push — replacement did not converge across grasps",
        s.state_event_grasp1.id, s.state_event_grasp2.id,
    );
    Ok(())
}

/// The post-delete-vnext-push state event also landed on the vanilla
/// (non-grasp) relay with the same id as the grasps' copy — the
/// replacement reached every relay listed in the announcement.
#[rstest]
#[tokio::test]
async fn grasp_and_vanilla_state_events_agree_on_id(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.state_event_grasp1.id, s.state_event_vanilla.id,
        "state events on grasp1 ({}) and the vanilla relay ({}) differ after \
         the delete-vnext push — replacement did not reach the non-grasp relay",
        s.state_event_grasp1.id, s.state_event_vanilla.id,
    );
    Ok(())
}

/// State event's `HEAD` tag is still `"ref: refs/heads/main"` after the
/// delete-vnext push — see `src/lib/repo_state.rs::add_head`. The
/// remote helper read the publisher's HEAD (which never moved away
/// from `main`), so the published default branch is preserved.
#[rstest]
#[tokio::test]
async fn state_event_head_still_points_at_main(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let head_value = tag_value(&s.state_event_grasp1, "HEAD")
        .context("state event missing a HEAD tag — required by add_head()")?;
    assert_eq!(
        head_value,
        format!("ref: {}", s.main_branch_ref),
        "state event HEAD tag {head_value:?} does not point at {} — the \
         delete-vnext push flipped the published default branch",
        s.main_branch_ref,
    );
    Ok(())
}

/// State event's `refs/heads/main` tag matches the publisher's local
/// main oid — the surviving post-delete-vnext-push state event still
/// names `main` accurately.
#[rstest]
#[tokio::test]
async fn state_event_main_ref_matches_local_oid(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let local = oid_at(&s.publisher, &s.main_branch_ref, "publisher local main")?;
    let branch_value = tag_value(&s.state_event_grasp1, &s.main_branch_ref).with_context(|| {
        format!(
            "state event missing a {} tag — main dropped out of the state \
             event after the delete-vnext push",
            s.main_branch_ref,
        )
    })?;
    assert_eq!(
        branch_value, *local,
        "state event {} tag {branch_value} does not match local oid {local}",
        s.main_branch_ref,
    );
    Ok(())
}

/// State event has no `refs/heads/vnext` tag — the delete-vnext push
/// dropped the branch from the published state, which is the core
/// property of this scenario.
#[rstest]
#[tokio::test]
async fn state_event_has_no_vnext_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert!(
        tag_value(&s.state_event_grasp1, &s.vnext_branch_ref).is_none(),
        "state event still carries a {} tag after the delete-vnext push — \
         the replacement state event did not drop the deleted branch",
        s.vnext_branch_ref,
    );
    Ok(())
}

// ---------- helpers ---------------------------------------------------------
//
// Intentionally duplicated from `super::fresh_repo` and
// `super::add_branch` rather than hoisted to a shared module — keeping
// each scenario self-contained makes it trivial to read one file
// end-to-end. When the duplication starts to bite, a follow-up pass
// can lift these into a `common` submodule.

/// Look up an OID in a captured [`RepoSnapshot`] with a labelled error
/// when the ref is missing — turns a `None` from the map into a
/// pinpoint message naming both the source repo and the refname.
fn oid_at<'a>(snap: &'a RepoSnapshot, refname: &str, label: &str) -> Result<&'a String> {
    snap.refs
        .get(refname)
        .with_context(|| format!("{label} snapshot has no {refname}"))
}

/// Poll for `path` to exist, with a short ceiling — the grasp's
/// announcement policy creates the bare repo synchronously on receipt
/// but the relay ACK can return before the filesystem op is visible.
async fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while !path.is_dir() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
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

/// Publish `event` to every relay URL in `urls`, bailing if any relay
/// rejects it.
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
            output.failed,
        );
    }
    Ok(())
}

/// `git clone http_url cloned-via-http` inside `host_dir`, then capture
/// the resulting working tree as a [`RepoSnapshot`].
async fn clone_via_http_and_snapshot(host_dir: &Path, http_url: &str) -> Result<RepoSnapshot> {
    let subdir = "cloned-via-http";
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(host_dir);
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd.args(["clone", http_url, subdir]);
    let out = cmd.output().await.context("failed to spawn direct clone")?;
    require_success("direct http clone", &out)?;
    RepoSnapshot::capture(&host_dir.join(subdir)).context("capturing direct http clone snapshot")
}

/// Pick the (single) state event whose `d` tag matches `identifier`.
fn pick_state_event<'a>(events: &'a [Event], identifier: &str) -> Option<&'a Event> {
    events
        .iter()
        .find(|e| tag_value(e, "d").as_deref() == Some(identifier))
}

/// First value of the `[<name>, <value>, ...]` tag on `event`, if
/// present.
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

/// Bail with a captured-output error when `out.status.success()` is
/// false.
fn require_success(label: &str, out: &std::process::Output) -> Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }
}
