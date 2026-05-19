//! Migrated regression coverage for
//! `git-remote-nostr list`'s **state-event** selection logic in
//! `src/bin/git_remote_nostr/list.rs:38-110`.
//!
//! Replaces these legacy `#[serial]` PTY tests from
//! `tests/legacy/git_remote_nostr/list.rs`:
//!
//! 1. `without_state_announcement::lists_head_and_2_branches_...
//!    _from_git_server`
//! 2. `with_state_announcement::when_announcement_matches_git_server::...`
//! 3. `with_state_announcement::when_state_event_references_oids_not_on_git_server::falls_back_to_git_server_state`
//! 4. `...::when_newer_relay_state_has_missing_oid_but_older_relay_state_is_resolvable::uses_older_resolvable_state_event`
//!    — exercises the cross-relay candidate ordering: when the newest
//!    candidate is unresolvable but an older candidate on a different
//!    relay can resolve every OID, `list.rs:79-90` should fall back to
//!    the older one rather than degrading to the bare-repo state. The
//!    legacy test built this with two grasps + two divergent git
//!    servers; this migration uses one grasp plus a vanilla repo relay
//!    (via [`PublishRepoOpts::extra_repo_relays`]) — the vanilla relay
//!    is the only practical way to land an unresolvable state event
//!    because GRASP gates kind-30618 publishes against its own git
//!    data.
//! 5. `with_state_announcement::when_announcement_doesnt_match_git_server::anouncement_state_is_used`
//!
//! Open-proposal listing (legacy case 6,
//! `when_there_are_open_proposals::open_proposal_listed_in_prs_namespace`)
//! lives in `tests/list_pr.rs` and its patch-kind sibling
//! `tests/list_patch.rs` — the underlying code path in `list.rs:247-291`
//! diverges sharply between PR-kind (read `c` tag, fetch one OID) and
//! patch-kind (rebuild commits by applying every patch), so the file
//! split matches the kind split.
//!
//! ## What's actually being driven
//!
//! Each test runs `git ls-remote nostr://<announcement>` from a freshly
//! cloned repo and asserts on the parsed `<oid>\t<refname>` lines. That
//! transport invocation is the moral equivalent of the legacy `send_line
//! "list"` over PTY: `git` shells out to `git-remote-nostr` and asks it
//! to enumerate refs. The advertised set is the function of the
//! state-event + git-server state logic under test. Parsing avoids
//! asserting on exact stdout strings (banned by the harness rules in
//! `docs/architecture/test-harness.md`).
//!
//! ## Helper layering
//!
//! - [`Harness::publish_repo`] gives us a maintainer-signed announcement with
//!   the grasp's URL on it, an `origin` remote on the publisher, and an initial
//!   `main` push. That handles the "ws://grasp/serves kind-30617 + kind-30618 +
//!   bare repo with main pointing somewhere" precondition every legacy test
//!   depended on.
//! - [`Harness::publish_state_event`] fabricates and publishes a *replacement*
//!   kind-30618 directly to either the grasp's relay surface or, via
//!   [`PublishStateEventTarget::RelayUrl`], to a vanilla relay listed in the
//!   announcement. Tests 3 and 5 use the grasp target; test 4
//!   (`uses_older_resolvable_state_event`) uses the vanilla target so the
//!   published event isn't gated against the grasp's bare-repo contents.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use test_harness::{
    CloneLogin, Harness, PublishRepoOpts, PublishStateEventOpts, PublishStateEventTarget,
    PublishedRepo, Repo,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Spin up one vanilla relay + one grasp, publish the maintainer repo,
/// then return both the maintainer-side [`Repo`] and the
/// [`PublishedRepo`] handle. Every test starts from the same precondition
/// — one less code-path divergence to keep track of when reading
/// regressions.
async fn setup() -> Result<(Harness, Repo, PublishedRepo)> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("list-state maintainer".into()),
            identifier: Some("list-state-repo".into()),
            ..Default::default()
        })
        .await?;

    Ok((harness, publisher, published))
}

/// `(filename, content)` → branch off `main`, commit, return the resulting
/// oid. Caller is responsible for `git checkout main` afterwards if they
/// need to push more state from `main`.
async fn commit_on_branch(repo: &Repo, branch: &str, file: &str, content: &str) -> Result<String> {
    git_ok(repo, ["checkout", "-b", branch], "git checkout -b").await?;
    std::fs::write(repo.dir().join(file), content).with_context(|| format!("write {file}"))?;
    git_ok(repo, ["add", file], "git add").await?;
    git_ok(
        repo,
        ["commit", "-m", &format!("add {file}"), "--no-gpg-sign"],
        "git commit",
    )
    .await?;
    rev_parse(repo, "HEAD").await
}

/// `git push -u origin <branch>` via [`Repo::nostr_push`]. Pulled out of
/// every test because the "push the branch we just committed" line was
/// getting noisy.
///
/// `Repo::nostr_push` (not a raw `git push`) is mandatory here because the
/// push goes through `git-remote-nostr`, which publishes an
/// auto-generated kind-30618 state event. See `test_harness::clock` for the
/// timing rule that helper enforces, and the previously-flaky
/// `state_event_takes_precedence_over_advanced_git_server_state` regression
/// for why it matters.
async fn push_branch(repo: &Repo, branch: &str) -> Result<()> {
    repo.nostr_push(["-u", "origin", branch])
        .await
        .with_context(|| format!("git push origin {branch}"))?;
    Ok(())
}

/// `git rev-parse <rev>` → oid hex.
async fn rev_parse(repo: &Repo, rev: &str) -> Result<String> {
    let out = repo
        .git(["rev-parse", rev])
        .output()
        .await
        .with_context(|| format!("git rev-parse {rev}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git rev-parse {rev} exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git rev-parse stdout not utf-8")?
        .trim()
        .to_string())
}

/// Bail on non-zero exit with captured stdout/stderr. Saves every test
/// from rolling its own error wrapper.
async fn git_ok<I, S>(repo: &Repo, args: I, label: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("spawn {label}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

/// Run `git ls-remote --symref nostr://<url>` from inside a fresh
/// logged-out clone and return the parsed result.
///
/// We could `ls-remote` directly against [`PublishedRepo::clone_url`]
/// without cloning first, but every legacy test for this surface ran
/// `list` from inside a `prep_git_repo()` (a fully populated working
/// tree with the nostr remote configured). Cloning matches that — and
/// the resulting `origin` remote is the cheapest way to spell "the
/// nostr URL" in the `git ls-remote` argv (`origin` resolves via the
/// remote helper just like `nostr://...` would).
async fn ls_remote_via_clone(
    harness: &Harness,
    published: &PublishedRepo,
) -> Result<LsRemoteOutput> {
    let clone = harness
        .clone_published_repo(published, CloneLogin::None)
        .await?;
    ls_remote(&clone, "origin").await
}

/// `git ls-remote --symref <remote>` parsed into a structured view of
/// what the remote (via `git-remote-nostr list`) advertised.
struct LsRemoteOutput {
    /// Symref lines (`ref: refs/heads/main\tHEAD`) keyed by the symref's
    /// own name (e.g. `"HEAD"`) → target ref (e.g. `"refs/heads/main"`).
    symrefs: BTreeMap<String, String>,
    /// `<refname>` → `<oid hex>` for every resolved ref.
    refs: BTreeMap<String, String>,
}

impl LsRemoteOutput {
    /// `refs/heads/*` only; the prefix is stripped from the key.
    fn heads(&self) -> BTreeMap<&str, &str> {
        self.refs
            .iter()
            .filter_map(|(k, v)| k.strip_prefix("refs/heads/").map(|name| (name, v.as_str())))
            .collect()
    }
}

async fn ls_remote(repo: &Repo, remote: &str) -> Result<LsRemoteOutput> {
    let out = repo
        .git(["ls-remote", "--symref", remote])
        .output()
        .await
        .with_context(|| format!("spawn git ls-remote --symref {remote}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git ls-remote --symref {remote} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8(out.stdout).context("ls-remote stdout not utf-8")?;
    let mut symrefs = BTreeMap::new();
    let mut refs = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Symref lines look like `ref: refs/heads/main\tHEAD` (note the
        // tab + leading `ref: ` marker — git ls-remote --symref prints
        // these ahead of the resolved oid).
        if let Some(rest) = line.strip_prefix("ref: ") {
            let (target, name) = rest
                .split_once('\t')
                .with_context(|| format!("malformed symref line: {line:?}"))?;
            symrefs.insert(name.to_string(), target.to_string());
            continue;
        }
        let (oid, name) = line
            .split_once('\t')
            .with_context(|| format!("malformed ref line: {line:?}"))?;
        refs.insert(name.to_string(), oid.to_string());
    }
    Ok(LsRemoteOutput { symrefs, refs })
}

/// Poll the grasp's relay until a kind-30618 maintainer-signed event
/// shows up advertising `ref_name == expected_oid`. Under parallel test
/// load the `git push` subprocess driving the remote helper can exit a
/// few milliseconds before the auto-generated state event has finished
/// propagating across the websocket inside the helper, so the bare
/// `push.await?` is not a strong enough barrier for a follow-up
/// `ls-remote` to observe the new ref. Tests that publish their own
/// state events via [`Harness::publish_state_event`] do not need this —
/// that helper waits for the publish's ACK before returning.
async fn wait_for_state_event_covering(
    harness: &Harness,
    repo: &PublishedRepo,
    ref_name: &str,
    expected_oid: &str,
) -> Result<()> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(10);
    let kind = nostr_sdk::Kind::Custom(30618);
    loop {
        let events = harness
            .grasp("repo")
            .events(
                nostr_sdk::Filter::new()
                    .kind(kind)
                    .author(repo.maintainer_keys.public_key()),
            )
            .await?;
        // Newest-first; once any candidate carries the right oid for the
        // ref we care about we're done. `list.rs` walks newest-first and
        // takes the first resolvable hit, so this matches the live
        // selection contract.
        let mut sorted = events.clone();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        let matches = sorted.iter().any(|e| {
            e.tags.iter().any(|t| {
                let s = t.as_slice();
                s.first().map(String::as_str) == Some(ref_name)
                    && s.get(1).map(String::as_str) == Some(expected_oid)
            })
        });
        if matches {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for kind-30618 from maintainer to advertise {ref_name}={expected_oid}; \
                 last seen {} events",
                sorted.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Legacy case 1: lists head + branches + commit ids from git server
//   (no state announcement present)
// ---------------------------------------------------------------------------

/// Folds legacy
/// `without_state_announcement::lists_head_and_2_branches_and_commit_ids_from_git_server`.
///
/// Setup: maintainer publishes the repo; commits `vnext` on top of a
/// separate branch and pushes it. No fabricated state event — we rely on
/// the state event that `publish_repo` + the post-`git push vnext` cycle
/// produced.
#[tokio::test]
async fn lists_head_and_branches_from_git_server_when_state_event_matches() -> Result<()> {
    let (harness, publisher, published) = setup().await?;

    // Add a second branch and push it; the post-push sync emits a fresh
    // kind-30618 covering both `main` and `vnext` (see
    // `src/bin/git_remote_nostr/push.rs:356` for the call site).
    let vnext_oid = commit_on_branch(&publisher, "vnext", "vnext.md", "vnext\n").await?;
    push_branch(&publisher, "vnext").await?;
    let main_oid = published.initial_oid.clone();

    // Wait for the auto-generated kind-30618 to actually surface on the
    // grasp's relay before we ls-remote against it. Under parallel test
    // load the `git push` subprocess can exit a few ms before the
    // `nostr-sdk` client inside `git-remote-nostr` finishes ACKing
    // the relay; without this barrier the subsequent clone races the
    // publish and `vnext` doesn't appear.
    wait_for_state_event_covering(&harness, &published, "refs/heads/vnext", &vnext_oid).await?;

    let ls = ls_remote_via_clone(&harness, &published).await?;

    assert_eq!(
        ls.symrefs.get("HEAD").map(String::as_str),
        Some("refs/heads/main"),
        "HEAD should be a symref to refs/heads/main",
    );
    let heads: HashMap<&str, &str> = ls.heads().into_iter().collect();
    assert_eq!(
        heads.get("main"),
        Some(&main_oid.as_str()),
        "refs/heads/main should resolve to the publisher's main tip",
    );
    assert_eq!(
        heads.get("vnext"),
        Some(&vnext_oid.as_str()),
        "refs/heads/vnext should resolve to the publisher's vnext tip",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy case 3: state event references OIDs that don't exist on the git server
// ---------------------------------------------------------------------------

/// Folds legacy
/// `when_state_event_references_oids_not_on_git_server::falls_back_to_git_server_state`.
///
/// Publish a state event whose `refs/heads/main` value is a plausible-looking
/// but **non-existent** OID. `list.rs:79-90` should reject that candidate
/// and fall back to the bare repo's actual `main` (the seed commit from
/// `publish_repo`).
#[tokio::test]
async fn falls_back_to_git_server_when_state_event_references_missing_oids() -> Result<()> {
    let (harness, _publisher, published) = setup().await?;

    let fake_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let real_main_oid = published.initial_oid.clone();

    let mut state = HashMap::new();
    state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
    state.insert("refs/heads/main".to_string(), fake_oid.clone());

    // The seed kind-30618 from `publish_repo`'s `git push` is already on
    // the grasp at this point; this second publish supersedes it because
    // it's signed by the same maintainer with the same `d` tag and a
    // newer `created_at`.
    harness
        .publish_state_event(
            &published,
            PublishStateEventOpts {
                state,
                ..Default::default()
            },
        )
        .await?;

    let ls = ls_remote_via_clone(&harness, &published).await?;

    let heads = ls.heads();
    assert!(
        !heads.values().any(|v| *v == fake_oid),
        "fake OID from the fabricated state event must NOT be advertised; got {heads:?}",
    );
    assert_eq!(
        heads.get("main").copied(),
        Some(real_main_oid.as_str()),
        "main should fall back to the bare repo's actual oid; got {heads:?}",
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy case 2: state event matches the git server's view
// ---------------------------------------------------------------------------

/// Folds legacy
/// `when_announcement_matches_git_server::lists_head_and_2_branches_and_commit_ids_announcement`.
///
/// Same shape as case 1, but with an *explicit* fabricated state event
/// covering both branches. Asserts that when the state event and the bare
/// repo agree, `list.rs` advertises the state event's view (which equals
/// the bare repo's view in this case — they only need to *not disagree*).
#[tokio::test]
async fn lists_branches_from_state_event_when_matches_git_server() -> Result<()> {
    let (harness, publisher, published) = setup().await?;

    let vnext_oid =
        commit_on_branch(&publisher, "example-branch", "example.md", "example\n").await?;
    push_branch(&publisher, "example-branch").await?;
    let main_oid = published.initial_oid.clone();

    // Replace the auto-generated state event with a maintainer-signed one
    // whose ref→oid map exactly matches what the bare repo has. `list.rs`
    // should walk newest-first, find this candidate resolvable, and use
    // it (rather than falling back).
    let mut state = HashMap::new();
    state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
    state.insert("refs/heads/main".to_string(), main_oid.clone());
    state.insert("refs/heads/example-branch".to_string(), vnext_oid.clone());

    harness
        .publish_state_event(
            &published,
            PublishStateEventOpts {
                state,
                ..Default::default()
            },
        )
        .await?;

    let ls = ls_remote_via_clone(&harness, &published).await?;
    let heads = ls.heads();
    assert_eq!(heads.get("main").copied(), Some(main_oid.as_str()));
    assert_eq!(
        heads.get("example-branch").copied(),
        Some(vnext_oid.as_str()),
    );
    assert_eq!(
        ls.symrefs.get("HEAD").map(String::as_str),
        Some("refs/heads/main"),
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy case 5: state event takes precedence even when bare repo has advanced
// ---------------------------------------------------------------------------

/// Folds legacy
/// `when_announcement_doesnt_match_git_server::anouncement_state_is_used`.
///
/// Publish a state event claiming `main` is at the *original* seed commit,
/// then push a real second commit onto `main` so the bare repo has
/// advanced past it. The state event remains resolvable (its referenced
/// OID exists on the bare repo, just isn't the tip), so `list.rs:79-90`
/// keeps it as the chosen candidate.
#[tokio::test]
async fn state_event_takes_precedence_over_advanced_git_server_state() -> Result<()> {
    let (harness, publisher, published) = setup().await?;

    let original_main_oid = published.initial_oid.clone();

    // Add a second branch with its own commit; the state event we
    // publish next claims `main` is still at `original_main_oid` and
    // `example-branch` is at `example_oid` — both real OIDs the bare
    // repo carries. The list output should reflect the *state event*,
    // not the bare repo's `main` advance below.
    let example_oid =
        commit_on_branch(&publisher, "example-branch", "example.md", "example\n").await?;
    push_branch(&publisher, "example-branch").await?;

    // Publish the fabricated state event *before* advancing main, while
    // the bare repo still agrees with it. Any ordering would work — the
    // OIDs are all real, so resolvability is satisfied either way — but
    // this matches the legacy test's choreography (publish state first,
    // then `git push` further commits).
    let mut state = HashMap::new();
    state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
    state.insert("refs/heads/main".to_string(), original_main_oid.clone());
    state.insert("refs/heads/example-branch".to_string(), example_oid.clone());
    harness
        .publish_state_event(
            &published,
            PublishStateEventOpts {
                state,
                ..Default::default()
            },
        )
        .await?;

    // Now advance main on the bare repo. The fabricated state event no
    // longer matches: `main` on the server is at `advanced_main_oid` but
    // the state event still says `original_main_oid`.
    git_ok(&publisher, ["checkout", "main"], "git checkout main").await?;
    std::fs::write(publisher.dir().join("commitx.md"), "some content\n")
        .context("write commitx.md")?;
    git_ok(&publisher, ["add", "commitx.md"], "git add commitx").await?;
    git_ok(
        &publisher,
        ["commit", "-m", "add commitx.md", "--no-gpg-sign"],
        "git commit commitx",
    )
    .await?;
    let advanced_main_oid = rev_parse(&publisher, "HEAD").await?;
    assert_ne!(advanced_main_oid, original_main_oid);
    // Push to `origin/main` via the nostr remote — this also publishes a
    // *newer* kind-30618 reflecting the advanced state. To preserve the
    // legacy regression contract ("state-event view wins over git-server
    // view"), we then immediately publish *another* state event signed
    // by the maintainer with the older OID, with a deliberately
    // forward-in-time created_at to beat the auto-generated one.
    //
    // Without this second publish the auto-generated state event from
    // the `git push` would be newest and the test would degenerate into
    // "newest state event wins" — already covered by case 2.
    push_branch(&publisher, "main").await?;
    let mut state = HashMap::new();
    state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
    state.insert("refs/heads/main".to_string(), original_main_oid.clone());
    state.insert("refs/heads/example-branch".to_string(), example_oid.clone());
    harness
        .publish_state_event(
            &published,
            PublishStateEventOpts {
                state,
                ..Default::default()
            },
        )
        .await?;

    let ls = ls_remote_via_clone(&harness, &published).await?;
    let heads = ls.heads();
    assert_eq!(
        heads.get("main").copied(),
        Some(original_main_oid.as_str()),
        "main should reflect the state event's view (original oid), not the \
         advanced bare-repo tip; got {heads:?}",
    );
    assert_eq!(
        heads.get("example-branch").copied(),
        Some(example_oid.as_str()),
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy case 4: newer relay's state event is unresolvable; fall back to
//   older relay's resolvable state event.
// ---------------------------------------------------------------------------

/// Folds legacy
/// `when_newer_relay_state_has_missing_oid_but_older_relay_state_is_resolvable::uses_older_resolvable_state_event`.
///
/// Two repo relays are listed on the kind-30617 announcement: the
/// grasp's own relay surface (which carries the auto-generated
/// kind-30618 from `publish_repo`'s `git push`, pointing at the real
/// `main` tip) and a separate vanilla relay registered under role
/// `"repo-extra"` (which receives a *newer* fabricated state event
/// pointing at an OID that doesn't exist on the bare repo).
///
/// Why a vanilla relay rather than two grasps? GRASP gates kind-30618
/// publishes against its own git data — there's no way to land an
/// intentionally-unresolvable state event on a grasp without first
/// pushing the matching git objects, which defeats the precondition
/// the test exists to exercise. A vanilla relay accepts the fabricated
/// event as-is. `list.rs:64-69` doesn't care which kind of relay a
/// candidate came from, only that the event's pubkey is in the
/// announcement's maintainers list.
///
/// `list.rs:79-90` should then walk newest-first, find the vanilla's
/// fabricated event unresolvable (its `refs/heads/main` OID is not on
/// any git server and not in the local repo), and fall back to the
/// older auto event on the grasp (whose `refs/heads/main` OID is the
/// bare repo's actual tip).
///
/// Asserts:
/// - The fake OID from the newer-but-unresolvable event must NOT be advertised
///   for any ref.
/// - `refs/heads/main` resolves to the publisher's real `main` tip (the
///   older-but-resolvable event's view).
#[tokio::test]
async fn uses_older_resolvable_state_event_from_different_relay() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_relay("repo-extra")
    .build()
    .await?;

    let extra_relay_url = harness.relay("repo-extra").url().to_string();

    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("list-state maintainer".into()),
            identifier: Some("list-state-fallback-repo".into()),
            extra_repo_relays: vec![extra_relay_url.clone()],
            ..Default::default()
        })
        .await?;

    let real_main_oid = published.initial_oid.clone();
    // Plausible-looking but synthetic OID — not the seed commit, not
    // any tag, not anything libgit2 would have on disk. The check at
    // `list.rs:81-85` walks `git_server_oids` (built from the per-git-
    // server `list_from_remotes` output) and the local repo, so this
    // value is unresolvable by construction.
    let fake_oid = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
    assert_ne!(fake_oid, real_main_oid);

    // Newer fabricated state event → vanilla repo relay only. The
    // grasp keeps the auto-generated state event (with the real OID)
    // that `publish_repo`'s `git push` produced.
    //
    // Note: the auto event is also pushed to the vanilla relay during
    // the same push (the announcement already lists vanilla as a repo
    // relay by then). That doesn't matter for the test contract —
    // `client.rs:859-873` keeps only the newest state event per relay,
    // and the fabricated event below has a strictly later
    // `created_at` than the auto event because `Repo::nostr_push`
    // inside `publish_repo` ticks one whole unix second after the
    // push completes.
    let mut state = HashMap::new();
    state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
    state.insert("refs/heads/main".to_string(), fake_oid.clone());
    harness
        .publish_state_event(
            &published,
            PublishStateEventOpts {
                state,
                target: PublishStateEventTarget::RelayUrl(extra_relay_url.clone()),
                ..Default::default()
            },
        )
        .await?;

    let ls = ls_remote_via_clone(&harness, &published).await?;
    let heads = ls.heads();

    assert!(
        !ls.refs.values().any(|v| *v == fake_oid),
        "fake OID from the newer-unresolvable state event on {extra_relay_url} \
         must NOT be advertised for any ref; got {refs:?}",
        refs = ls.refs,
    );
    assert_eq!(
        heads.get("main").copied(),
        Some(real_main_oid.as_str()),
        "main should fall back to the older-but-resolvable state event on \
         the grasp (real OID), not the newer-unresolvable one on the vanilla \
         relay (fake OID); got {heads:?}",
    );

    Ok(())
}
