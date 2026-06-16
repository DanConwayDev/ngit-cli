//! End-to-end coverage of a **new user cloning a repo over `nostr://`
//! and then interacting with the tags** that were pushed before they
//! arrived.
//!
//! [`super::push_tag`] proves a tag *push* lands correctly on every
//! surface (publisher refs, both grasps, a nostr clone's resting refs,
//! the kind-30618 state event). What it does *not* prove is that a
//! fresh cloner can then *use* those tags — and the annotated-tag path
//! has a specific, easy-to-regress failure mode there:
//!
//! [`super::push_tag`] proves a tag *push* lands correctly on every
//! surface (publisher refs, both grasps, a nostr clone's resting refs,
//! the kind-30618 state event — including the annotated tag's peeled
//! `refs/tags/<name>^{}` entry, which `git fetch --prune` needs to
//! resolve the tag or it deletes it as unresolvable). What it does
//! *not* prove is that a fresh cloner can then actually *use* those
//! tags through the full remote-helper read path.
//!
//! This scenario closes that end-to-end gap. It keeps the cloner
//! [`test_harness::Repo`] **alive** (rather than capturing a pure-data
//! snapshot and dropping the tempdir as the sibling scenarios do) so
//! cases can run live git commands — `git fetch --tags --prune`,
//! `git cat-file -t`, `git push <newtag>` — against it and assert on
//! the *post-interaction* state.
//!
//! Note on scope: the prune cases below assert the desirable end-to-end
//! property (both tags survive a real `git fetch --tags --prune`). In
//! this topology git can also resolve the annotated tag straight from
//! the grasp's object store, so these cases are *not* a tight pin on
//! the state-event-peel logic specifically — that regression is pinned
//! by `super::push_tag::state_event_annotated_tag_has_peeled_entry`,
//! which asserts the `^{}` entry directly in the published event.
//!
//! Covered interactions, from the perspective of someone who cloned and
//! had nothing to do with the original push:
//!
//! 1. `git fetch --tags --prune` keeps both the lightweight and the annotated
//!    tag (the prune-deletes-annotated-tag regression).
//! 2. The annotated tag on the clone is a real **tag object** that dereferences
//!    to the seed commit — not a lightweight tag masquerading at the tag-object
//!    oid.
//! 3. The clone (logged in as the maintainer) can push a **brand-new** tag back
//!    through the same `nostr://` remote, and it lands in the state event and
//!    on the grasps' bare repos — without leaving a `refs/remotes/origin/<tag>`
//!    stray. ngit only accepts branch/tag pushes from maintainers
//!    (non-maintainers may push only `pr/*` proposal refs; see
//!    `src/bin/git_remote_nostr/push.rs:307`), so this models a maintainer
//!    pushing from a fresh checkout, not an arbitrary cloner.
//!
//! ## rstest discipline
//!
//! Unlike the snapshot-only sibling scenarios, this one's cases run
//! commands, so they must not race each other on the shared clone's
//! working tree. The interactions are split so that the read-only
//! interactions (1, 2) run against the clone as captured immediately
//! after the initial clone + a single `git fetch --tags --prune`, and
//! the mutating interaction (3) is performed once during fixture setup
//! with its observable side-effects captured into the `Snapshot` —
//! keeping every `#[rstest]` case a read-only assertion on captured
//! data, consistent with the rest of the suite.

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

const DEFAULT_BRANCH: &str = "main";

/// Lightweight tag pushed by the publisher before the cloner arrives.
const LIGHTWEIGHT_TAG: &str = "v-light";

/// Annotated tag pushed by the publisher before the cloner arrives.
const ANNOTATED_TAG: &str = "v-annot";

/// A *new* tag created and pushed back from the fresh clone (logged in
/// as the maintainer) — proves the clone-then-push round-trip works.
const CLONER_TAG: &str = "v-cloned";

/// Captured side-effects of: publisher announces + pushes main + two
/// tags; a fresh user clones over `nostr://`, runs
/// `git fetch --tags --prune`, then pushes a new annotated tag back.
///
/// Holds the live [`Harness`] and cloner [`test_harness::Repo`] so
/// post-clone git interactions already happened during setup and their
/// results are captured here as owned data.
struct Snapshot {
    /// Cloner's working tree captured **after** `git clone` +
    /// `git fetch --tags --prune`. Both original tags must still be
    /// present at the right oids; no `refs/remotes/origin/<tag>`.
    cloner_after_fetch_prune: RepoSnapshot,
    /// `git cat-file -t <annotated-tag-oid>` on the clone — must be
    /// `"tag"` (an annotated tag object), proving the clone reconstructed
    /// the object, not a bare ref.
    annotated_object_type: String,
    /// `git rev-parse <annotated-tag>^{}` on the clone — the commit the
    /// annotated tag dereferences to. Must equal the seed commit.
    annotated_peeled_commit: String,
    /// Cloner's working tree captured **after** it pushed `CLONER_TAG`
    /// back through the nostr remote. Must hold `refs/tags/v-cloned`
    /// and (negatively) no `refs/remotes/origin/v-cloned`.
    cloner_after_push: RepoSnapshot,
    /// The kind-30618 state event on grasp1 **after** the cloner's
    /// tag push — must now also name `refs/tags/v-cloned`.
    state_event_after_cloner_push: Event,
    /// Direct `git clone http://grasp1/...git` taken **after** the
    /// cloner's push — proves the new tag reached the grasp's bare repo.
    grasp1_clone_after_cloner_push: RepoSnapshot,
    /// Seed commit oid — both original tags resolve (or peel) to this.
    commit_oid: String,
    /// Annotated tag's own tag-object oid.
    annotated_tag_oid: String,
    /// The cloner-pushed tag's tag-object oid (it's annotated too).
    cloner_tag_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("clone_interact_tag fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

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
    let display_name = "git clone interact tag test";
    let identifier = "git-clone-interact-tag-test";

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

    let seed_filename = "README.md";
    let seed_content = "hello, clone-interact tag scenario!\n";
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
            "test repo for git clone tag-interaction assertions".to_string(),
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

    // ---------- publisher: push main + both tags ------------------------
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

    publisher
        .nostr_push(["-u", "origin", DEFAULT_BRANCH])
        .await
        .context("git push -u origin main")?;

    require_success(
        "git tag v-light",
        &publisher
            .git(["tag", LIGHTWEIGHT_TAG])
            .output()
            .await
            .context("failed to spawn git tag (lightweight)")?,
    )?;
    require_success(
        "git tag -a v-annot",
        &publisher
            .git(["tag", "-a", ANNOTATED_TAG, "-m", "release annotated tag"])
            .output()
            .await
            .context("failed to spawn git tag -a (annotated)")?,
    )?;
    let annotated_tag_oid = publisher
        .rev_parse(ANNOTATED_TAG)
        .await
        .context("git rev-parse v-annot")?;
    anyhow::ensure!(
        annotated_tag_oid != commit_oid,
        "annotated tag {ANNOTATED_TAG} rev-parsed to the commit oid — \
         setup did not create an annotated tag",
    );

    publisher
        .nostr_push(["origin", LIGHTWEIGHT_TAG, ANNOTATED_TAG])
        .await
        .context("git push origin v-light v-annot")?;

    // ---------- a fresh user clones over nostr:// -----------------------
    //
    // `Harness::clone_url` clones via the git-remote-nostr helper into a
    // live `Repo` we keep alive for the rest of setup so we can run
    // post-clone git interactions against it.
    let cloner = harness
        .clone_url(&nostr_url)
        .await
        .context("git clone <nostr-url> for fresh cloner")?;

    // Interaction 1: `git fetch --tags --prune`. This is the operation
    // that *deletes* an annotated tag whose `^{}` peel is missing from
    // the published state — git cannot resolve the tag-object oid to a
    // commit, decides the ref is unresolvable, and prunes it. If the
    // state event carries the peel (the fix under regression), both
    // tags survive.
    require_success(
        "git fetch --tags --prune",
        &cloner
            .git(["fetch", "--tags", "--prune", "origin"])
            .output()
            .await
            .context("failed to spawn git fetch --tags --prune")?,
    )?;
    let cloner_after_fetch_prune = cloner
        .snapshot()
        .context("capturing cloner snapshot after fetch --prune")?;

    // Interaction 2: inspect the annotated tag as an object. `cat-file
    // -t <tag-oid>` must report `tag`; `rev-parse <name>^{}` must peel
    // to the seed commit.
    let annotated_object_type = cloner
        .git(["cat-file", "-t", &annotated_tag_oid])
        .output()
        .await
        .context("failed to spawn git cat-file -t on annotated tag oid")
        .map(|o| {
            require_success("git cat-file -t", &o)?;
            Ok::<_, anyhow::Error>(String::from_utf8_lossy(&o.stdout).trim().to_string())
        })??;
    let annotated_peeled_commit = cloner
        .rev_parse(&format!("{ANNOTATED_TAG}^{{}}"))
        .await
        .context("git rev-parse v-annot^{} on clone")?;

    // Interaction 3: a maintainer with a *fresh nostr clone* pushes a
    // brand-new annotated tag back. ngit only accepts branch/tag pushes
    // from accounts listed as maintainers on the announcement
    // (`src/bin/git_remote_nostr/push.rs:307` — non-maintainers may only
    // push `pr/*` proposal refs), so the clone must be logged in as the
    // publisher to exercise the tag round-trip. Writing the publisher's
    // nsec into the clone's local config is the harness equivalent of
    // `ngit account login` on a fresh checkout.
    require_success(
        "git config nostr.nsec (login as maintainer on clone)",
        &cloner
            .git(["config", "--local", "nostr.nsec", &nsec])
            .output()
            .await
            .context("failed to write publisher nsec into clone config")?,
    )?;

    require_success(
        "git tag -a v-cloned (on clone)",
        &cloner
            .git([
                "tag",
                "-a",
                CLONER_TAG,
                "-m",
                "tag created from a fresh clone",
            ])
            .output()
            .await
            .context("failed to spawn git tag -a v-cloned")?,
    )?;
    let cloner_tag_oid = cloner
        .rev_parse(CLONER_TAG)
        .await
        .context("git rev-parse v-cloned")?;

    cloner
        .nostr_push(["origin", CLONER_TAG])
        .await
        .context("git push origin v-cloned (from clone)")?;

    let cloner_after_push = cloner
        .snapshot()
        .context("capturing cloner snapshot after pushing v-cloned")?;

    // ---------- observe the cloner's push -------------------------------
    //
    // The clone is logged in as the publisher, so the state event the
    // push emits is authored by the publisher's key — same coordinate as
    // the original push, replaced in place.
    let filter = || Filter::new().author(pubkey).kind(Kind::Custom(STATE_KIND));
    let grasp1_state = grasp1.events(filter()).await?;
    let state_event_after_cloner_push = pick_state_event(&grasp1_state, identifier)
        .context("no state event with the expected `d` tag on grasp1 after cloner push")?
        .clone();

    let host1 = harness
        .fresh_repo()
        .context("fresh_repo for direct grasp1 clone after cloner push")?;
    let grasp1_clone_after_cloner_push =
        clone_via_http_and_snapshot(host1.dir(), &grasp1_clone_url)
            .await
            .context("direct grasp1 clone after cloner push")?;

    Ok(Snapshot {
        cloner_after_fetch_prune,
        annotated_object_type,
        annotated_peeled_commit,
        cloner_after_push,
        state_event_after_cloner_push,
        grasp1_clone_after_cloner_push,
        commit_oid,
        annotated_tag_oid,
        cloner_tag_oid,
    })
}

// ---------- cases -----------------------------------------------------------

/// **The prune regression.** After a fresh clone + `git fetch --tags
/// --prune`, the lightweight tag is still present at the seed commit.
/// (Lightweight tags resolve trivially, so this is the control against
/// the annotated case below.)
#[rstest]
#[tokio::test]
async fn lightweight_tag_survives_fetch_prune(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{LIGHTWEIGHT_TAG}");
    let oid = oid_at(&s.cloner_after_fetch_prune, &tag_ref, "cloner after prune")?;
    assert_eq!(
        *oid, s.commit_oid,
        "after git fetch --tags --prune the clone's {tag_ref} ({oid}) is not \
         the seed commit ({})",
        s.commit_oid,
    );
    Ok(())
}

/// End-to-end: after `git fetch --tags --prune`, the *annotated* tag is
/// still present at its tag-object oid on a fresh clone — a real cloner
/// can fetch-and-prune without losing the annotated tag. (This asserts
/// the desirable end-to-end outcome; the tight pin on the state-event
/// `^{}` peel that makes the tag resolvable lives in
/// `super::push_tag::state_event_annotated_tag_has_peeled_entry`.)
#[rstest]
#[tokio::test]
async fn annotated_tag_survives_fetch_prune(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{ANNOTATED_TAG}");
    let oid = oid_at(&s.cloner_after_fetch_prune, &tag_ref, "cloner after prune")?;
    assert_eq!(
        *oid, s.annotated_tag_oid,
        "after git fetch --tags --prune the clone's {tag_ref} ({oid}) is not \
         the annotated tag-object oid ({}) — the annotated tag did not \
         survive a fetch-and-prune on a fresh clone",
        s.annotated_tag_oid,
    );
    Ok(())
}

/// The fetch + prune did not leave a `refs/remotes/origin/<tag>` stray
/// on the clone for either original tag.
#[rstest]
#[tokio::test]
async fn fetch_prune_leaves_no_tag_remote_tracking_ref(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_no_tag_remote_tracking_refs(&s.cloner_after_fetch_prune, "cloner after prune");
    Ok(())
}

/// `git cat-file -t <annotated-tag-oid>` on the clone reports `tag` —
/// the clone reconstructed a real annotated tag object, not a
/// lightweight tag sitting at the tag-object oid.
#[rstest]
#[tokio::test]
async fn cloned_annotated_tag_is_a_tag_object(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.annotated_object_type, "tag",
        "git cat-file -t on the clone's annotated tag oid reported {:?}, \
         expected \"tag\" — the annotated tag did not round-trip as an object",
        s.annotated_object_type,
    );
    Ok(())
}

/// The clone's annotated tag dereferences (`^{}`) to the seed commit —
/// the object the clone holds is wired to the right commit.
#[rstest]
#[tokio::test]
async fn cloned_annotated_tag_peels_to_seed_commit(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.annotated_peeled_commit, s.commit_oid,
        "clone's {ANNOTATED_TAG}^{{}} ({}) is not the seed commit ({})",
        s.annotated_peeled_commit, s.commit_oid,
    );
    Ok(())
}

/// The cloner's own `git push origin v-cloned` left `refs/tags/v-cloned`
/// locally at the new tag-object oid and (negatively) no
/// `refs/remotes/origin/v-cloned`.
#[rstest]
#[tokio::test]
async fn cloner_push_keeps_tag_local_only(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{CLONER_TAG}");
    let oid = oid_at(&s.cloner_after_push, &tag_ref, "cloner after push")?;
    assert_eq!(
        *oid, s.cloner_tag_oid,
        "after pushing, cloner's {tag_ref} ({oid}) is not the new tag-object \
         oid ({})",
        s.cloner_tag_oid,
    );
    let stray = format!("refs/remotes/origin/{CLONER_TAG}");
    assert!(
        !s.cloner_after_push.refs.contains_key(&stray),
        "cloner's push of {CLONER_TAG} wrote a remote-tracking ref {stray} — \
         tags must not land in git's remote-tracking branch namespace",
    );
    Ok(())
}

/// After the clone's push, the kind-30618 state event names
/// `refs/tags/v-cloned` at the new tag-object oid — the
/// clone-then-push round-trip reached the nostr surface.
#[rstest]
#[tokio::test]
async fn state_event_records_cloner_pushed_tag(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{CLONER_TAG}");
    let value = tag_value(&s.state_event_after_cloner_push, &tag_ref)
        .with_context(|| format!("state event after cloner push missing a {tag_ref} tag"))?;
    assert_eq!(
        value, s.cloner_tag_oid,
        "state event {tag_ref} = {value}, expected the cloner's new \
         tag-object oid {}",
        s.cloner_tag_oid,
    );
    Ok(())
}

/// The cloner-pushed annotated tag also carries its `^{}` peel in the
/// state event — so the *next* cloner won't lose it to `fetch --prune`
/// either. Proves the peel-emission path is not publisher-specific.
#[rstest]
#[tokio::test]
async fn state_event_records_cloner_pushed_tag_peel(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let peeled_ref = format!("refs/tags/{CLONER_TAG}^{{}}");
    let value = tag_value(&s.state_event_after_cloner_push, &peeled_ref)
        .with_context(|| format!("state event missing the peeled {peeled_ref} entry"))?;
    assert_eq!(
        value, s.commit_oid,
        "state event {peeled_ref} = {value}, expected the seed commit {}",
        s.commit_oid,
    );
    Ok(())
}

/// A direct `git clone http://grasp1/...git` taken after the cloner's
/// push carries `refs/tags/v-cloned` — the new tag reached the grasp's
/// bare git repo, not just the nostr state event.
#[rstest]
#[tokio::test]
async fn grasp_bare_repo_has_cloner_pushed_tag(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let tag_ref = format!("refs/tags/{CLONER_TAG}");
    let oid = oid_at(
        &s.grasp1_clone_after_cloner_push,
        &tag_ref,
        "grasp1 clone after cloner push",
    )?;
    assert_eq!(
        *oid, s.cloner_tag_oid,
        "grasp1 bare repo's {tag_ref} ({oid}) is not the cloner's new \
         tag-object oid ({})",
        s.cloner_tag_oid,
    );
    Ok(())
}

// ---------- helpers ---------------------------------------------------------

/// Assert the snapshot holds no `refs/remotes/*/<tagname>` ref for any
/// of this scenario's tags.
fn assert_no_tag_remote_tracking_refs(snap: &RepoSnapshot, label: &str) {
    for refname in snap.refs.keys() {
        let is_tag_tracking = refname.starts_with("refs/remotes/")
            && (refname.ends_with(&format!("/{LIGHTWEIGHT_TAG}"))
                || refname.ends_with(&format!("/{ANNOTATED_TAG}"))
                || refname.ends_with(&format!("/{CLONER_TAG}")));
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

/// Poll for `path` to exist, with a short ceiling.
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
