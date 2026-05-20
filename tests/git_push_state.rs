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
//! ## rstest discipline
//!
//! Modelled on `tests/send_patch.rs` — every `#[case]` is a read-only
//! assertion on a captured `Snapshot`. **Unlike** `send_patch.rs` the
//! fixture is shared across cases via a `tokio::sync::OnceCell` because
//! this test's setup is expensive (two grasps, two clones, three relay
//! REQs) and every case asserts on the same captured event data — no
//! case writes anything back. Setup still runs lazily on first case
//! access, so `cargo test --test git_push_state -- some_case` still
//! exercises the full setup path.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{Harness, clock};
use tokio::sync::OnceCell;

/// `STATE_KIND` (`Kind::Custom(30618)`) mirrored locally to keep the test
/// crate free of an ngit-lib dep.
const STATE_KIND: u16 = 30618;

/// The single source of truth for the default branch name across this
/// test. `Repo::init` runs `git init -b main` so the local repo always
/// uses `main`.
const DEFAULT_BRANCH: &str = "main";

/// Captured side-effects of the manual-announcement / `git push` flow.
/// All assertion cases read fields off this struct; no case writes
/// anything back. Owned `String`s / `Event`s / `PathBuf`s so the
/// `Harness` and its tempdirs can drop after the fixture returns
/// without invalidating anything we'll later assert on.
struct Snapshot {
    /// Hex oid of `refs/heads/main` on the publisher after the seed
    /// commit and before the push. Every other oid in the snapshot
    /// should equal this.
    local_oid: String,
    /// `refs/remotes/origin/main` on the publisher *after*
    /// `git push -u origin main`.
    local_remote_tracking_oid: String,
    /// Value of `branch.main.merge` in the publisher's local git
    /// config after `git push -u`.
    upstream_merge_cfg: String,
    /// `refs/heads/main` in a fresh `git clone <nostr-url>` working tree.
    cloned_via_nostr_oid: String,
    /// `refs/heads/main` in a fresh `git clone http://grasp1/.../...git`
    /// working tree.
    cloned_via_grasp1_oid: String,
    /// `refs/heads/main` in a fresh `git clone http://grasp2/.../...git`
    /// working tree.
    cloned_via_grasp2_oid: String,
    /// The kind-30618 state event as the *first grasp* sees it (used as
    /// the canonical version for tag-content assertions; grasp2 and the
    /// vanilla relay must report the same event id).
    state_event_grasp1: Event,
    /// `state_event_grasp1`'s twin on the second grasp. Asserted equal
    /// by id to `state_event_grasp1`.
    state_event_grasp2: Event,
    /// `state_event_grasp1`'s twin on the vanilla standard relay listed
    /// in the announcement. Asserted equal by id to
    /// `state_event_grasp1`.
    state_event_vanilla: Event,
    /// `refs/heads/<DEFAULT_BRANCH>` — convenience constant the cases
    /// would otherwise have to rebuild.
    branch_ref: String,
}

/// Global, lazily-initialised snapshot. The first `#[case]` to await
/// `snapshot()` runs the full fixture; subsequent cases see the same
/// `Arc`. `OnceCell::get_or_init` serialises the initializer so two
/// cases hitting it concurrently can't double-run setup.
static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("git_push_state fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

/// Drive the entire setup: harness, account, commit, manual
/// announcement, push, two clones, three state-event queries. Returns a
/// pure-data `Snapshot` so the harness can drop without invalidating
/// anything.
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
    let display_name = "git push state test";
    let identifier = "git-push-state-test";

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

    // Seed commit with a deterministic body so a misrouted clone is obvious.
    let seed_filename = "README.md";
    let seed_content = "hello, manual announcement!\n";
    std::fs::write(publisher.dir().join(seed_filename), seed_content)
        .context("failed to write seed file in publisher repo")?;
    require_success(
        "git add",
        &publisher
            .git(["add", seed_filename])
            .output()
            .await
            .context("failed to spawn git add")?,
    )?;
    require_success(
        "git commit",
        &publisher
            .git(["commit", "-m", "initial", "--no-gpg-sign"])
            .output()
            .await
            .context("failed to spawn git commit")?,
    )?;

    let branch_ref = format!("refs/heads/{DEFAULT_BRANCH}");
    let local_oid = publisher
        .snapshot()?
        .refs
        .get(&branch_ref)
        .with_context(|| format!("{branch_ref} missing after initial commit"))?
        .clone();

    // ---------- manual kind-30617 announcement --------------------------
    //
    // Modelled on `src/lib/repo_ref.rs::RepoRef::to_event` so the tag
    // shape is one production code paths already parse. The two grasps'
    // clone URLs use the `http://host:port/<npub>/<identifier>.git`
    // layout that ngit-grasp's announcement policy provisions
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

    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags(announcement_tags)
        .sign_with_keys(&keys)
        .context("failed to sign repo announcement")?;

    // Publish to all three surfaces. Each grasp's announcement policy
    // accepts because its own ws URL is in the relays tag and its own
    // clone URL is in the clone tag; the vanilla relay accepts
    // unconditionally.
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
    // emitted by `git push` below) lands in a strictly later unix
    // second and can't id-collide. See `test_harness::clock` for the
    // writeup.
    clock::tick_to_next_second().await;

    // ---------- wait for the grasps to materialise the bare repos -------
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

    // ---------- add the nostr:// remote and push ------------------------
    //
    // Build the nostr URL by hand to match the format
    // `NostrUrlDecoded`'s Display impl produces (and `parse_and_resolve`
    // accepts): `nostr://<npub>/<urlencoded-ws-relay>/<identifier>`.
    // Use the vanilla relay as the hint — the announcement is there,
    // the relay is reachable for the cloner, and a `ws://...` local URL
    // round-trips correctly through `parse_and_resolve`'s `ws://`-vs-
    // `wss://` branch.
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

    // `Repo::nostr_push` runs `git push <args>` then ticks one whole
    // unix second — see `test_harness::clock` for why bare `git push`
    // is forbidden against a nostr remote. `-u` writes the upstream
    // tracking config so subsequent `git push` calls in this repo work
    // without re-specifying the ref.
    publisher
        .nostr_push(["-u", "origin", DEFAULT_BRANCH])
        .await
        .context("git push -u origin main")?;

    // Capture local post-push state.
    let after_push = publisher.snapshot()?;
    let local_remote_tracking_oid = after_push
        .refs
        .get(&format!("refs/remotes/origin/{DEFAULT_BRANCH}"))
        .with_context(|| {
            format!(
                "refs/remotes/origin/{DEFAULT_BRANCH} missing after git push -u — \
                 push went through but git did not update the remote-tracking ref"
            )
        })?
        .clone();
    let upstream_merge_cfg = publisher
        .config(&format!("branch.{DEFAULT_BRANCH}.merge"))
        .await?
        .with_context(|| {
            format!(
                "branch.{DEFAULT_BRANCH}.merge missing — `git push -u` did \
                 not set upstream tracking"
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
    let cloned_via_nostr_oid =
        read_local_ref_oid(&cloner.dir().join(nostr_clone_subdir), &branch_ref)
            .with_context(|| format!("reading {branch_ref} from nostr clone"))?;

    // ---------- direct grasp clones (plain smart-http, no helper) -------
    //
    // Reusing `harness.fresh_repo()` because (a) it gives a tempdir
    // with a benign git identity already configured and (b) the test
    // crate doesn't depend on `tempfile` directly. We keep the `Repo`s
    // alive until the clones (and the ref reads) complete — dropping a
    // `Repo` deletes its tempdir, so binding to `_` or letting it fall
    // out of a closure would yank the host directory out from under
    // the in-flight `git clone`.
    let host1 = harness
        .fresh_repo()
        .context("fresh_repo for direct grasp1 clone")?;
    let host2 = harness
        .fresh_repo()
        .context("fresh_repo for direct grasp2 clone")?;
    let cloned_via_grasp1_oid =
        clone_via_http_and_read_ref(host1.dir(), &grasp1_clone_url, &branch_ref)
            .await
            .context("direct grasp1 clone")?;
    let cloned_via_grasp2_oid =
        clone_via_http_and_read_ref(host2.dir(), &grasp2_clone_url, &branch_ref)
            .await
            .context("direct grasp2 clone")?;

    // ---------- state-event queries ------------------------------------
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
        local_oid,
        local_remote_tracking_oid,
        upstream_merge_cfg,
        cloned_via_nostr_oid,
        cloned_via_grasp1_oid,
        cloned_via_grasp2_oid,
        state_event_grasp1,
        state_event_grasp2,
        state_event_vanilla,
        branch_ref,
    })
}

// ---------------------------------------------------------------------------
// case 1 — local refs/remotes/origin/<branch> matches local oid
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum LocalRefsCase {
    /// `refs/remotes/origin/main` advanced to the local commit oid.
    RemoteTrackingMatchesLocal,
    /// `branch.main.merge` was set by `-u` to `refs/heads/main`.
    UpstreamTrackingConfigSet,
}

#[rstest]
#[case::remote_tracking_matches_local(LocalRefsCase::RemoteTrackingMatchesLocal)]
#[case::upstream_tracking_config_set(LocalRefsCase::UpstreamTrackingConfigSet)]
#[tokio::test]
async fn publisher_local_refs(
    #[future] snapshot: Arc<Snapshot>,
    #[case] case: LocalRefsCase,
) -> Result<()> {
    let s = snapshot.await;
    match case {
        LocalRefsCase::RemoteTrackingMatchesLocal => {
            assert_eq!(
                s.local_remote_tracking_oid, s.local_oid,
                "publisher's refs/remotes/origin/{DEFAULT_BRANCH} ({}) does \
                 not match local {} ({})",
                s.local_remote_tracking_oid, s.branch_ref, s.local_oid,
            );
        }
        LocalRefsCase::UpstreamTrackingConfigSet => {
            assert_eq!(
                s.upstream_merge_cfg, s.branch_ref,
                "branch.{DEFAULT_BRANCH}.merge = {:?}, expected {:?}",
                s.upstream_merge_cfg, s.branch_ref,
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// case 2 — nostr:// clone reproduces the same ref
// ---------------------------------------------------------------------------

#[rstest]
#[tokio::test]
async fn nostr_clone_reproduces_local_ref(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.cloned_via_nostr_oid, s.local_oid,
        "nostr clone's {} ({}) does not match publisher's local ({})",
        s.branch_ref, s.cloned_via_nostr_oid, s.local_oid,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// case 3 — direct http clone of each grasp reproduces the same ref
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum DirectCloneCase {
    Grasp1,
    Grasp2,
}

#[rstest]
#[case::grasp1(DirectCloneCase::Grasp1)]
#[case::grasp2(DirectCloneCase::Grasp2)]
#[tokio::test]
async fn direct_grasp_clone_reproduces_local_ref(
    #[future] snapshot: Arc<Snapshot>,
    #[case] case: DirectCloneCase,
) -> Result<()> {
    let s = snapshot.await;
    let (label, oid) = match case {
        DirectCloneCase::Grasp1 => ("grasp1", &s.cloned_via_grasp1_oid),
        DirectCloneCase::Grasp2 => ("grasp2", &s.cloned_via_grasp2_oid),
    };
    assert_eq!(
        oid, &s.local_oid,
        "direct {label} clone's {} ({oid}) does not match publisher's local ({})",
        s.branch_ref, s.local_oid,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// case 4 — state event landed on all three relay surfaces with one id
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum StatePropagationCase {
    /// The grasp1 and grasp2 state-event ids agree — push converged
    /// across both grasps' relay surfaces.
    GraspsAgreeOnId,
    /// The grasp1 and vanilla-relay state-event ids agree — the push
    /// also published the state event to the non-grasp relay in the
    /// announcement.
    GraspAndVanillaAgreeOnId,
}

#[rstest]
#[case::grasps_agree_on_id(StatePropagationCase::GraspsAgreeOnId)]
#[case::grasp_and_vanilla_agree_on_id(StatePropagationCase::GraspAndVanillaAgreeOnId)]
#[tokio::test]
async fn state_event_propagates_to_all_announced_relays(
    #[future] snapshot: Arc<Snapshot>,
    #[case] case: StatePropagationCase,
) -> Result<()> {
    let s = snapshot.await;
    match case {
        StatePropagationCase::GraspsAgreeOnId => {
            assert_eq!(
                s.state_event_grasp1.id, s.state_event_grasp2.id,
                "state events on grasp1 ({}) and grasp2 ({}) differ — \
                 push did not converge across grasps",
                s.state_event_grasp1.id, s.state_event_grasp2.id,
            );
        }
        StatePropagationCase::GraspAndVanillaAgreeOnId => {
            assert_eq!(
                s.state_event_grasp1.id, s.state_event_vanilla.id,
                "state events on grasp1 ({}) and the vanilla relay ({}) \
                 differ — push did not publish to the non-grasp repo relay",
                s.state_event_grasp1.id, s.state_event_vanilla.id,
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// case 5 — state event tags carry HEAD + the default branch correctly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum StateTagCase {
    /// `HEAD` tag value is `"ref: refs/heads/main"` — see
    /// `src/lib/repo_state.rs::add_head`.
    HeadPointsAtDefaultBranch,
    /// `refs/heads/main` tag value matches the local oid.
    DefaultBranchRefMatchesLocalOid,
}

#[rstest]
#[case::head_points_at_default_branch(StateTagCase::HeadPointsAtDefaultBranch)]
#[case::default_branch_ref_matches_local_oid(StateTagCase::DefaultBranchRefMatchesLocalOid)]
#[tokio::test]
async fn state_event_tags(
    #[future] snapshot: Arc<Snapshot>,
    #[case] case: StateTagCase,
) -> Result<()> {
    let s = snapshot.await;
    match case {
        StateTagCase::HeadPointsAtDefaultBranch => {
            let head_value = tag_value(&s.state_event_grasp1, "HEAD")
                .context("state event missing a HEAD tag — required by add_head()")?;
            assert_eq!(
                head_value,
                format!("ref: {}", s.branch_ref),
                "state event HEAD tag {head_value:?} does not point at {}",
                s.branch_ref,
            );
        }
        StateTagCase::DefaultBranchRefMatchesLocalOid => {
            let branch_value =
                tag_value(&s.state_event_grasp1, &s.branch_ref).with_context(|| {
                    format!(
                        "state event missing a {} tag — the pushed ref",
                        s.branch_ref
                    )
                })?;
            assert_eq!(
                branch_value, s.local_oid,
                "state event {} tag {branch_value} does not match local oid {}",
                s.branch_ref, s.local_oid,
            );
        }
    }
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
            output.failed,
        );
    }
    Ok(())
}

/// `git clone http_url cloned-via-http` inside `host_dir`, then read the
/// requested ref out of the resulting working tree. Wraps the per-grasp
/// direct-clone boilerplate the fixture would otherwise repeat twice.
async fn clone_via_http_and_read_ref(
    host_dir: &Path,
    http_url: &str,
    refname: &str,
) -> Result<String> {
    // Plain smart-http clone — no nostr remote helper involved, so we
    // don't need the harness env. Pin GIT_CONFIG_* to `/dev/null` so a
    // developer's global git config can't influence the clone.
    let subdir = "cloned-via-http";
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(host_dir);
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd.args(["clone", http_url, subdir]);
    let out = cmd.output().await.context("failed to spawn direct clone")?;
    require_success("direct http clone", &out)?;
    read_local_ref_oid(&host_dir.join(subdir), refname)
}

/// Pick the (single) state event whose `d` tag matches `identifier`.
/// Returns `None` rather than panicking so the caller can produce a
/// pinpoint error message.
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
/// false. The label is included verbatim so the failure log points at
/// the command that failed.
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
