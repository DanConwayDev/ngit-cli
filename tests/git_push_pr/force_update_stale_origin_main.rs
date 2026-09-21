//! A confirmed maintainer's local default branch can include upstream changes
//! learned from another Git server before the Nostr destination catches up.
//! Even when it tracks the other remote, use the advanced local branch as the
//! PR base for both initial publication and a force-pushed rewrite. A new PR
//! requires --force before excluding those unpublished local-default commits.
//! The complementary fork_origin cases ensure contributors and proposals equal
//! to local default do not lose their proposed changes.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts,
    event_branch_name_tag, tag_value,
};
use tokio::sync::OnceCell;

/// Identifier for the test repo — distinct from the other git_push_pr
/// scenarios to avoid cross-test relay pollution on the shared vanilla relay.
const IDENTIFIER: &str = "git-push-pr-force-update-stale-origin-main";

/// Feature branch name; pushed as `pr/feature`.
const BRANCH: &str = "feature";

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

struct Snapshot {
    /// The KIND_PULL_REQUEST_UPDATE event produced by the force push.
    pr_update_event: Event,

    /// Total KIND_PULL_REQUEST events by the contributor on the GRASP after
    /// both pushes. Must equal 1 (assertion 1).
    pr_count: usize,

    /// Total KIND_PULL_REQUEST_UPDATE events by the contributor on the GRASP
    /// after both pushes. Must equal 1 (assertion 1).
    pr_update_count: usize,

    /// The stale `origin/main` OID (= `published.initial_oid`). The update's
    /// `merge-base` must NOT equal this (assertion 2).
    stale_origin_main_oid: String,

    /// The advanced local `main` tip (= `gitlab/main`). The update's
    /// `merge-base` must equal this (assertion 2).
    advanced_main_oid: String,

    /// The single-commit PR tip OID after the force reset. The update's `c`
    /// tag must equal this (assertion 3).
    update_tip_oid: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("force_update_stale_origin_main fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

// ---------------------------------------------------------------------------
// Arrange + act + capture
// ---------------------------------------------------------------------------

async fn capture_snapshot() -> Result<Snapshot> {
    // --- 1. Harness: relay + GRASP + a non-nostr ("gitlab") git server -------
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .with_vanilla_git_server("gitlab")
    .build()
    .await?;

    // --- 2. Maintainer publishes the repo ------------------------------------
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("stale-origin maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    let gitlab_url = harness.vanilla_git_server("gitlab").url().to_string();

    // --- 3. Maintainer seeds + advances the gitlab remote's main -------------
    //
    // Push the current main to gitlab, then add one commit and push again so
    // `gitlab/main` is one commit ahead of `published.initial_oid`. This is the
    // "canonical default branch advanced on a non-nostr remote" the contributor
    // will pull.
    publisher
        .git_ok(
            ["remote", "add", "gitlab", &gitlab_url],
            "git remote add gitlab (publisher)",
        )
        .await?;
    publisher
        .git_ok(["push", "gitlab", "main"], "git push gitlab main (seed)")
        .await?;
    std::fs::write(publisher.dir().join("gitlab-main.md"), "content\n")
        .context("failed to write gitlab-main.md on publisher side")?;
    publisher
        .git_ok(["add", "gitlab-main.md"], "git add gitlab-main.md")
        .await?;
    publisher
        .git_ok(
            ["commit", "-m", "advance main on gitlab", "--no-gpg-sign"],
            "git commit advance main on gitlab",
        )
        .await?;
    publisher
        .git_ok(["push", "gitlab", "main"], "git push gitlab main (advance)")
        .await?;
    let advanced_main_oid = publisher.rev_parse("HEAD").await?;

    // --- 4. Clone as the confirmed maintainer
    // -------------------------------------
    let contributor = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing after AsContributor login")?;
    let contributor_keys =
        Keys::parse(&contributor_nsec).context("contributor nostr.nsec is not a valid key")?;
    let contributor_pubkey = contributor_keys.public_key();

    // Sanity: the freshly-cloned origin/main is the baseline fork point.
    let stale_origin_main_oid = contributor.rev_parse("refs/remotes/origin/main").await?;
    if stale_origin_main_oid != published.initial_oid {
        bail!(
            "setup invariant violated: origin/main ({stale_origin_main_oid}) != initial_oid \
             ({}) right after clone",
            published.initial_oid
        );
    }

    // --- 4b. Contributor pulls the advanced main from gitlab into LOCAL main -
    //
    // After this, local `main` == gitlab/main == advanced_main_oid, while the
    // nostr `origin/main` remote-tracking ref stays at the stale baseline. This
    // is the exact condition the fix addresses.
    contributor
        .git_ok(
            ["remote", "add", "gitlab", &gitlab_url],
            "git remote add gitlab (contributor)",
        )
        .await?;
    contributor
        .git_ok(["fetch", "gitlab"], "git fetch gitlab")
        .await?;
    contributor
        .git_ok(["checkout", "main"], "git checkout main")
        .await?;
    contributor
        .git_ok(
            ["reset", "--hard", "gitlab/main"],
            "git reset --hard gitlab/main",
        )
        .await?;

    contributor
        .git_ok(
            ["branch", "--set-upstream-to=gitlab/main", "main"],
            "track maintainer's other publishing remote",
        )
        .await?;

    let local_main_oid = contributor.rev_parse("main").await?;
    if local_main_oid != advanced_main_oid {
        bail!(
            "setup invariant violated: local main ({local_main_oid}) != advanced gitlab main \
             ({advanced_main_oid}) after pull"
        );
    }
    // The whole point: origin/main is still stale relative to local main.
    if advanced_main_oid == stale_origin_main_oid {
        bail!(
            "setup invariant violated: advanced_main_oid == stale_origin_main_oid \
             ({advanced_main_oid}) — the gitlab advance did not move main"
        );
    }

    // --- 5. Contributor: pr/feature off advanced main + original PR ----------
    contributor
        .git_ok(
            ["checkout", "-b", &format!("pr/{BRANCH}")],
            &format!("git checkout -b pr/{BRANCH}"),
        )
        .await?;
    std::fs::write(contributor.dir().join("t1.md"), "some content\n")
        .context("failed to write t1.md")?;
    contributor
        .git_ok(["add", "t1.md"], "git add t1.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t1.md", "--no-gpg-sign"],
            "git commit t1.md",
        )
        .await?;
    std::fs::write(contributor.dir().join("t2.md"), "some content\n")
        .context("failed to write t2.md")?;
    contributor
        .git_ok(["add", "t2.md"], "git add t2.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t2.md", "--no-gpg-sign"],
            "git commit t2.md",
        )
        .await?;
    contributor
        .nostr_push_expecting_failure(["-u", "origin", &format!("pr/{BRANCH}")])
        .await?;
    assert!(
        harness
            .grasp("repo")
            .events(
                Filter::new()
                    .author(contributor_pubkey)
                    .kind(KIND_PULL_REQUEST)
            )
            .await?
            .is_empty()
    );
    contributor
        .nostr_push(["-f", "-u", "origin", &format!("pr/{BRANCH}")])
        .await
        .context("first nostr_push -u origin pr/feature failed")?;

    // --- 6. Contributor: force-reset to a single simplified commit -----------
    //
    // Reset pr/feature back to the advanced local main tip, then add one
    // commit (the simplified version of the change). This mirrors the user's
    // `git reset` to a single commit off the tip of (advanced) main.
    contributor
        .git_ok(
            ["reset", "--hard", "main"],
            "git reset --hard main (simplify)",
        )
        .await?;
    std::fs::write(contributor.dir().join("t3.md"), "simplified change\n")
        .context("failed to write t3.md")?;
    contributor
        .git_ok(["add", "t3.md"], "git add t3.md")
        .await?;
    contributor
        .git_ok(
            ["commit", "-m", "add t3.md (simplified)", "--no-gpg-sign"],
            "git commit t3.md",
        )
        .await?;
    let update_tip_oid = contributor.rev_parse("HEAD").await?;

    // The single PR commit's parent is the advanced main tip — the merge-base
    // we expect the fix to record.
    let single_commit_parent = contributor.rev_parse("HEAD~1").await?;
    if single_commit_parent != advanced_main_oid {
        bail!(
            "setup invariant violated: parent of the single PR commit ({single_commit_parent}) \
             != advanced_main_oid ({advanced_main_oid})"
        );
    }

    // --- 7. Force push — the act under test ----------------------------------
    contributor
        .nostr_push(["-f", "origin", &format!("pr/{BRANCH}")])
        .await
        .context("nostr_push -f origin pr/feature (force push) failed")?;

    // --- 8. Capture events ---------------------------------------------------
    let pr_count = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST),
        )
        .await?
        .into_iter()
        .filter(|e| event_branch_name_tag(e).as_deref() == Some(BRANCH))
        .count();

    let pr_update_events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?;
    let pr_update_count = pr_update_events.len();
    let pr_update_event = pr_update_events.into_iter().next().context(
        "no KIND_PULL_REQUEST_UPDATE authored by contributor found on GRASP \
         after force push with stale origin/main",
    )?;

    Ok(Snapshot {
        pr_update_event,
        pr_count,
        pr_update_count,
        stale_origin_main_oid,
        advanced_main_oid,
        update_tip_oid,
    })
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Assertion 1: exactly one KIND_PULL_REQUEST and one KIND_PULL_REQUEST_UPDATE
/// exist on the GRASP after both pushes.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn one_pr_one_update(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        s.pr_count, 1,
        "expected exactly one KIND_PULL_REQUEST on GRASP; got {} — did the force push \
         emit a new PR instead of an update?",
        s.pr_count,
    );
    assert_eq!(
        s.pr_update_count, 1,
        "expected exactly one KIND_PULL_REQUEST_UPDATE on GRASP; got {}",
        s.pr_update_count,
    );
    Ok(())
}

/// A confirmed maintainer's advanced local main remains the fork point even
/// when it tracks another publishing remote and the destination has not caught
/// up.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn merge_base_is_advanced_main_not_stale_origin(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_ne!(
        s.advanced_main_oid, s.stale_origin_main_oid,
        "setup invariant: advanced_main_oid should differ from stale_origin_main_oid",
    );
    assert_eq!(
        tag_value(&s.pr_update_event, "merge-base").as_deref(),
        Some(s.advanced_main_oid.as_str()),
        "merge-base tag should equal the advanced local main tip ({}), not the stale \
         origin/main ({}); got {:?} — the fork point was computed against a stale single \
         remote's default branch",
        s.advanced_main_oid,
        s.stale_origin_main_oid,
        tag_value(&s.pr_update_event, "merge-base"),
    );
    Ok(())
}

/// Assertion 3: the update event's `c` tag equals the single-commit tip OID
/// produced by the force reset.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn update_c_tag_is_new_tip(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.pr_update_event, "c").as_deref(),
        Some(s.update_tip_oid.as_str()),
        "update event `c` tag should equal the single-commit tip OID after the force reset; \
         got {:?}, want {:?}",
        tag_value(&s.pr_update_event, "c"),
        s.update_tip_oid,
    );
    Ok(())
}
