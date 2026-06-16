//! End-to-end coverage of `git push` putting **tags** — both
//! lightweight and annotated — onto a `nostr://` remote.
//!
//! Builds on the same manual-announcement topology as
//! [`super::fresh_repo`]: a hand-signed kind-30617 announcement, two
//! GRASP servers, one vanilla relay. After `git push -u origin main`
//! the publisher creates two tags on the seed commit — a lightweight
//! `v-light` and an annotated `v-annot` — and pushes both in one
//! `git push origin <tag> <tag>` invocation.
//!
//! ## Why this scenario exists
//!
//! Regression cover for the fix in
//! `fix: tag pushed via nostr remote appeared as remote branch`
//! (`src/bin/git_remote_nostr/push.rs` `update_remote_refs_pushed`).
//! Before that fix the remote helper wrote a per-remote tracking ref at
//! `refs/remotes/<remote>/<tagname>` — git's remote-tracking *branch*
//! namespace — so a pushed tag showed up as a remote branch in
//! `git branch -r`, IDE listings, completion, etc. Tags are global in
//! git's data model and have no per-remote tracking namespace; the
//! local `refs/tags/<name>` is the single source of truth.
//!
//! The central negative property under test is therefore: **after
//! pushing a tag through a nostr remote, no `refs/remotes/origin/<tag>`
//! ref exists** — on the publisher *or* on a fresh nostr clone. The
//! positive properties confirm the tag still propagates correctly:
//! `refs/tags/<name>` lands on the publisher, on both grasps' bare
//! repos, on a nostr clone, and the kind-30618 state event names it.
//!
//! ## Lightweight vs annotated
//!
//! The state event encodes the two tag kinds differently (see
//! `src/bin/git_remote_nostr/push.rs::generate_updated_state`):
//!
//! - **lightweight** — `refs/tags/v-light` → the commit oid; no `^{}`.
//! - **annotated** — `refs/tags/v-annot` → the *tag-object* oid, plus a peeled
//!   `refs/tags/v-annot^{}` → the commit oid. The peeled entry is required or
//!   `git fetch --prune` deletes the tag as unresolvable.
//!
//! Both kinds are exercised so a regression that only handles one
//! (e.g. peeling annotated tags to their commit, or dropping the `^{}`
//! line) is caught.
//!
//! ## rstest discipline
//!
//! Mirrors [`super::fresh_repo`] / [`super::add_branch`]: one
//! [`Snapshot`] captured once via a module-local
//! [`tokio::sync::OnceCell`], one `#[rstest] #[tokio::test]` per
//! asserted property so failures name the broken property in
//! `cargo test` output. Helpers are duplicated per-scenario by the
//! same deliberate convention these files already follow.

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

/// The repo's default branch. `Repo::init` runs `git init -b main`.
const DEFAULT_BRANCH: &str = "main";

/// Lightweight tag name (created with `git tag <name>`).
const LIGHTWEIGHT_TAG: &str = "v-light";

/// Annotated tag name (created with `git tag -a <name> -m ...`).
const ANNOTATED_TAG: &str = "v-annot";

/// Captured side-effects of: announce + `git push -u origin main`, then
/// `git push origin v-light v-annot`. All fields owned so the harness
/// and its tempdirs can drop after the fixture returns.
struct Snapshot {
    /// Publisher's working tree captured **after** the tag push. Holds
    /// `refs/heads/main`, `refs/tags/v-light`, `refs/tags/v-annot`, and
    /// (the negative property) must *not* hold any
    /// `refs/remotes/origin/v-*`.
    publisher: RepoSnapshot,
    /// Working tree of `git clone <nostr-url>` after the tag push. Must
    /// reproduce both tags at `refs/tags/*` and must *not* carry any
    /// `refs/remotes/origin/v-*`.
    nostr_clone: RepoSnapshot,
    /// Direct `git clone http://grasp1/.../...git` after the tag push.
    grasp1_clone: RepoSnapshot,
    /// Direct `git clone http://grasp2/.../...git` after the tag push.
    grasp2_clone: RepoSnapshot,
    /// The kind-30618 state event as the *first grasp* sees it — the
    /// replaceable event that survived the tag push. Canonical version
    /// for tag-content assertions.
    state_event_grasp1: Event,
    /// `state_event_grasp1`'s twin on the second grasp.
    state_event_grasp2: Event,
    /// The seed commit oid (`refs/heads/main` after the initial
    /// commit). The lightweight tag and the annotated tag's `^{}`
    /// peel both resolve to this.
    commit_oid: String,
    /// The annotated tag's *tag-object* oid (`git rev-parse v-annot`).
    /// Distinct from `commit_oid` — what the state event stores at the
    /// un-peeled `refs/tags/v-annot` slot.
    annotated_tag_oid: String,
}

/// Global, lazily-initialised snapshot for this scenario. Module-local;
/// does not share state with sibling scenarios.
static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("push_tag fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

/// Drive the entire setup: harness, account, commit on `main`, manual
/// announcement, `git push -u origin main`, create both tags, push both
/// tags, two clones, two state-event queries.
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
    let display_name = "git push tag test";
    let identifier = "git-push-tag-test";

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

    let branch_ref = format!("refs/heads/{DEFAULT_BRANCH}");

    // Seed commit on `main` with a deterministic body.
    let seed_filename = "README.md";
    let seed_content = "hello, tag scenario!\n";
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

    let commit_oid = publisher
        .snapshot()?
        .refs
        .get(&branch_ref)
        .with_context(|| format!("{branch_ref} missing after initial commit"))?
        .clone();

    // ---------- manual kind-30617 announcement --------------------------
    //
    // Same shape as the sibling scenarios — see
    // `super::fresh_repo`'s doc-comment for why every tag is the way
    // it is.
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
        Tag::parse(["r".to_string(), commit_oid.clone(), "euc".to_string()]).unwrap(),
        Tag::parse(["name".to_string(), display_name.to_string()]).unwrap(),
        Tag::parse([
            "description".to_string(),
            "test repo for git push tag assertions".to_string(),
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
    // `nostr_push` ticks one whole unix second so the upcoming tag
    // push's auto state event lands in a strictly later created_at
    // second than this one's and can't id-collide. See
    // `test_harness::clock`.
    publisher
        .nostr_push(["-u", "origin", DEFAULT_BRANCH])
        .await
        .context("git push -u origin main")?;

    // ---------- create both tags on the seed commit ---------------------
    //
    // Lightweight tag: `git tag <name>` — `refs/tags/v-light` points
    // straight at the commit oid, no tag object.
    require_success(
        "git tag v-light",
        &publisher
            .git(["tag", LIGHTWEIGHT_TAG])
            .output()
            .await
            .context("failed to spawn git tag (lightweight)")?,
    )?;
    // Annotated tag: `-a ... -m ...` creates a tag *object*, so
    // `refs/tags/v-annot` points at the tag object, which peels to the
    // commit. Catches the regression where the helper unwrapped
    // annotated tags to their commit (losing the tag object oid).
    require_success(
        "git tag -a v-annot",
        &publisher
            .git(["tag", "-a", ANNOTATED_TAG, "-m", "release annotated tag"])
            .output()
            .await
            .context("failed to spawn git tag -a (annotated)")?,
    )?;

    // The annotated tag's own object oid (distinct from the commit it
    // peels to) — what the state event stores at the un-peeled slot.
    let annotated_tag_oid = publisher
        .rev_parse(ANNOTATED_TAG)
        .await
        .context("git rev-parse v-annot")?;
    anyhow::ensure!(
        annotated_tag_oid != commit_oid,
        "annotated tag {ANNOTATED_TAG} rev-parsed to the commit oid \
         ({commit_oid}) — expected a distinct tag-object oid; the tag was \
         not created as annotated",
    );

    // ---------- push both tags in one invocation ------------------------
    //
    // `git push origin <tag> <tag>` pushes the named tags. Goes through
    // `nostr_push` for the same created_at-tick discipline as every
    // other nostr push in the harness.
    publisher
        .nostr_push(["origin", LIGHTWEIGHT_TAG, ANNOTATED_TAG])
        .await
        .context("git push origin v-light v-annot")?;

    // ---------- capture publisher state ---------------------------------
    let publisher_snap = publisher
        .snapshot()
        .context("capturing publisher snapshot after tag push")?;

    // Self-check: the tags we just created are present locally at the
    // oids we expect, so the rest of the assertions operate on good
    // data.
    let local_light = publisher_snap
        .refs
        .get(&format!("refs/tags/{LIGHTWEIGHT_TAG}"))
        .with_context(|| format!("refs/tags/{LIGHTWEIGHT_TAG} missing from publisher snapshot"))?;
    anyhow::ensure!(
        *local_light == commit_oid,
        "publisher's refs/tags/{LIGHTWEIGHT_TAG} ({local_light}) is not the \
         seed commit ({commit_oid}) — lightweight tag setup is wrong",
    );
    let local_annot = publisher_snap
        .refs
        .get(&format!("refs/tags/{ANNOTATED_TAG}"))
        .with_context(|| format!("refs/tags/{ANNOTATED_TAG} missing from publisher snapshot"))?;
    anyhow::ensure!(
        *local_annot == annotated_tag_oid,
        "publisher's refs/tags/{ANNOTATED_TAG} ({local_annot}) is not the \
         annotated tag-object oid ({annotated_tag_oid})",
    );

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
    let filter = || Filter::new().author(pubkey).kind(Kind::Custom(STATE_KIND));
    let grasp1_state = grasp1.events(filter()).await?;
    let grasp2_state = grasp2.events(filter()).await?;
    let state_event_grasp1 = pick_state_event(&grasp1_state, identifier)
        .context("no state event with the expected `d` tag on grasp1")?
        .clone();
    let state_event_grasp2 = pick_state_event(&grasp2_state, identifier)
        .context("no state event with the expected `d` tag on grasp2")?
        .clone();

    Ok(Snapshot {
        publisher: publisher_snap,
        nostr_clone,
        grasp1_clone,
        grasp2_clone,
        state_event_grasp1,
        state_event_grasp2,
        commit_oid,
        annotated_tag_oid,
    })
}

// ---------- cases -----------------------------------------------------------

/// **The core regression.** The publisher must *not* have a
/// `refs/remotes/origin/<tag>` ref for either tag after the push — that
/// namespace is git's remote-tracking *branch* namespace, and an entry
/// there makes the tag appear as a remote branch in `git branch -r`,
/// IDEs, completion, etc.
#[rstest]
#[tokio::test]
async fn publisher_has_no_remote_tracking_ref_for_tags(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_no_tag_remote_tracking_refs(&s.publisher, "publisher");
    Ok(())
}

/// As above, but on a fresh nostr clone — the clone must not synthesise
/// a `refs/remotes/origin/<tag>` ref either. (A clone has no upstream
/// push history, so this guards the *fetch/list* path independently of
/// the push path the publisher case covers.)
#[rstest]
#[tokio::test]
async fn nostr_clone_has_no_remote_tracking_ref_for_tags(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_no_tag_remote_tracking_refs(&s.nostr_clone, "nostr clone");
    Ok(())
}

/// The publisher still holds `refs/tags/v-light` at the seed commit oid
/// after the push — the local tag (git's single source of truth) is
/// undisturbed.
#[rstest]
#[tokio::test]
async fn publisher_retains_lightweight_tag(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}");
    let oid = oid_at(&s.publisher, &tag_ref, "publisher lightweight tag")?;
    assert_eq!(
        *oid, s.commit_oid,
        "publisher's {tag_ref} ({oid}) is not the seed commit ({})",
        s.commit_oid,
    );
    Ok(())
}

/// The publisher still holds `refs/tags/v-annot` at the *tag-object* oid
/// after the push — not peeled to its commit. Catches a regression that
/// rewrites the local annotated tag to its commit.
#[rstest]
#[tokio::test]
async fn publisher_retains_annotated_tag_object(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{ANNOTATED_TAG}");
    let oid = oid_at(&s.publisher, &tag_ref, "publisher annotated tag")?;
    assert_eq!(
        *oid, s.annotated_tag_oid,
        "publisher's {tag_ref} ({oid}) is not the annotated tag-object oid ({})",
        s.annotated_tag_oid,
    );
    Ok(())
}

/// `git clone <nostr-url>` reproduces `refs/tags/v-light` at the seed
/// commit oid — the lightweight tag propagated through the remote
/// helper as a real tag (not a branch).
#[rstest]
#[tokio::test]
async fn nostr_clone_reproduces_lightweight_tag(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}");
    let cloned = oid_at(&s.nostr_clone, &tag_ref, "nostr clone lightweight tag")?;
    assert_eq!(
        *cloned, s.commit_oid,
        "nostr clone's {tag_ref} ({cloned}) is not the seed commit ({})",
        s.commit_oid,
    );
    Ok(())
}

/// `git clone <nostr-url>` reproduces `refs/tags/v-annot` at the
/// tag-object oid — the annotated tag round-trips through the helper as
/// a tag object, with the `^{}` peel intact enough that git resolves it
/// (otherwise the clone would not carry the ref at the tag-object oid).
#[rstest]
#[tokio::test]
async fn nostr_clone_reproduces_annotated_tag(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{ANNOTATED_TAG}");
    let cloned = oid_at(&s.nostr_clone, &tag_ref, "nostr clone annotated tag")?;
    assert_eq!(
        *cloned, s.annotated_tag_oid,
        "nostr clone's {tag_ref} ({cloned}) is not the annotated tag-object \
         oid ({}) — the helper either dropped the tag object or the missing \
         ^{{}} peel made git refuse the tag",
        s.annotated_tag_oid,
    );
    Ok(())
}

/// Direct `git clone http://grasp1/...git` reproduces both tags — the
/// bare repo on grasp1's git server actually received the tag push,
/// independent of the nostr remote-helper path.
#[rstest]
#[tokio::test]
async fn grasp1_direct_clone_reproduces_both_tags(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let light_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}");
    let annot_ref = format!("refs/tags/{ANNOTATED_TAG}");
    let light = oid_at(&s.grasp1_clone, &light_ref, "grasp1 clone lightweight")?;
    let annot = oid_at(&s.grasp1_clone, &annot_ref, "grasp1 clone annotated")?;
    assert_eq!(
        *light, s.commit_oid,
        "grasp1 direct clone's {light_ref} ({light}) is not the seed commit ({})",
        s.commit_oid,
    );
    assert_eq!(
        *annot, s.annotated_tag_oid,
        "grasp1 direct clone's {annot_ref} ({annot}) is not the annotated \
         tag-object oid ({})",
        s.annotated_tag_oid,
    );
    Ok(())
}

/// As above, but for grasp2 — both grasps must carry the pushed tags.
#[rstest]
#[tokio::test]
async fn grasp2_direct_clone_reproduces_both_tags(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let light_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}");
    let annot_ref = format!("refs/tags/{ANNOTATED_TAG}");
    let light = oid_at(&s.grasp2_clone, &light_ref, "grasp2 clone lightweight")?;
    let annot = oid_at(&s.grasp2_clone, &annot_ref, "grasp2 clone annotated")?;
    assert_eq!(
        *light, s.commit_oid,
        "grasp2 direct clone's {light_ref} ({light}) is not the seed commit ({})",
        s.commit_oid,
    );
    assert_eq!(
        *annot, s.annotated_tag_oid,
        "grasp2 direct clone's {annot_ref} ({annot}) is not the annotated \
         tag-object oid ({})",
        s.annotated_tag_oid,
    );
    Ok(())
}

/// The state event names `refs/tags/v-light` at the seed commit oid —
/// the lightweight tag is recorded in the kind-30618 the push wrote.
#[rstest]
#[tokio::test]
async fn state_event_lightweight_tag_matches_commit(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}");
    let value = tag_value(&s.state_event_grasp1, &tag_ref)
        .with_context(|| format!("state event missing a {tag_ref} tag"))?;
    assert_eq!(
        value, s.commit_oid,
        "state event {tag_ref} = {value}, expected seed commit {}",
        s.commit_oid,
    );
    Ok(())
}

/// The state event names `refs/tags/v-annot` at the *tag-object* oid
/// (not the commit) — the annotated tag is recorded as a tag object so
/// cloners reconstruct the annotation, not a lightweight tag.
#[rstest]
#[tokio::test]
async fn state_event_annotated_tag_matches_tag_object(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{ANNOTATED_TAG}");
    let value = tag_value(&s.state_event_grasp1, &tag_ref)
        .with_context(|| format!("state event missing a {tag_ref} tag"))?;
    assert_eq!(
        value, s.annotated_tag_oid,
        "state event {tag_ref} = {value}, expected annotated tag-object oid \
         {} (the state event should store the tag object, not the peeled \
         commit)",
        s.annotated_tag_oid,
    );
    Ok(())
}

/// The state event carries the peeled `refs/tags/v-annot^{}` entry at
/// the commit oid. Without it `git fetch --prune` deletes the annotated
/// tag as unresolvable — see
/// `generate_updated_state` in `src/bin/git_remote_nostr/push.rs`.
#[rstest]
#[tokio::test]
async fn state_event_annotated_tag_has_peeled_entry(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let peeled_ref = format!("refs/tags/{ANNOTATED_TAG}^{{}}");
    let value = tag_value(&s.state_event_grasp1, &peeled_ref).with_context(|| {
        format!(
            "state event missing the peeled {peeled_ref} entry — git fetch \
             --prune would delete the annotated tag as unresolvable"
        )
    })?;
    assert_eq!(
        value, s.commit_oid,
        "state event {peeled_ref} = {value}, expected the seed commit {} the \
         annotated tag peels to",
        s.commit_oid,
    );
    Ok(())
}

/// The lightweight tag must *not* have a spurious `^{}` peeled entry in
/// the state event — only annotated tags get a peel. A `^{}` line for a
/// lightweight tag would be meaningless and could confuse cloners.
#[rstest]
#[tokio::test]
async fn state_event_lightweight_tag_has_no_peeled_entry(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let peeled_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}^{{}}");
    assert!(
        tag_value(&s.state_event_grasp1, &peeled_ref).is_none(),
        "state event unexpectedly carries a peeled {peeled_ref} entry for a \
         lightweight tag",
    );
    Ok(())
}

/// The state event's id is identical across both grasps after the tag
/// push — the replacement converged everywhere.
#[rstest]
#[tokio::test]
async fn grasps_state_events_agree_on_id(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.state_event_grasp1.id, s.state_event_grasp2.id,
        "state events on grasp1 ({}) and grasp2 ({}) differ after the tag \
         push — replacement did not converge across grasps",
        s.state_event_grasp1.id, s.state_event_grasp2.id,
    );
    Ok(())
}

// ---------- helpers ---------------------------------------------------------
//
// Intentionally duplicated from the sibling scenarios rather than
// hoisted to a shared module — keeping each scenario self-contained
// makes it trivial to read one file end-to-end. See `super::add_branch`'s
// helper-section note.

/// Assert the snapshot holds no `refs/remotes/*/<tagname>` ref for
/// either of this scenario's tags. Panics (via `assert!`) with a
/// pinpoint message naming the offending ref so the regression is
/// obvious in `cargo test` output.
fn assert_no_tag_remote_tracking_refs(snap: &RepoSnapshot, label: &str) {
    for refname in snap.refs.keys() {
        let is_tag_tracking = refname.starts_with("refs/remotes/")
            && (refname.ends_with(&format!("/{LIGHTWEIGHT_TAG}"))
                || refname.ends_with(&format!("/{ANNOTATED_TAG}")));
        assert!(
            !is_tag_tracking,
            "{label} has a remote-tracking ref {refname} for a tag — tags \
             must not be written into git's remote-tracking branch namespace \
             (refs/remotes/...); the local refs/tags/<name> is the single \
             source of truth",
        );
    }
}

/// Look up an OID in a captured [`RepoSnapshot`] with a labelled error
/// when the ref is missing.
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
        .send_event(event)
        .to(urls.iter().copied())
        .await
        .context("send_event fan-out")?;
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
