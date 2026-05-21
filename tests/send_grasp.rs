//! Lighthouse port of the legacy `ngit_send` "no cover letter" group:
//!
//! - `when_no_cover_letter_flag_set_with_range_of_head_2_sends_2_patches_without_cover_letter::no_cover_letter_event`
//! - `when_no_cover_letter_flag_set_with_range_of_head_2_sends_2_patches_without_cover_letter::two_patch_events`
//!
//! In the legacy world both used a hand-rolled mock relay and asserted on
//! `relay.events` after a hardcoded relay shutdown. Here the contributor
//! runs through the real `ngit send` against the publisher's GRASP-announced
//! repo, and we query the grasp + the user's default relay over actual
//! websocket REQs to count the resulting events.
//!
//! Flow:
//!
//! 1. Harness: one vanilla nostr relay (the user's default-set) + one
//!    `ngit-grasp` subprocess (git server + repo-relay for the published repo).
//! 2. `harness.publish_repo(...)` — mints a maintainer, commits a README, runs
//!    `ngit init`, pushes. The grasp now has the announcement out of purgatory
//!    and a bare repo with `refs/heads/main`.
//! 3. `harness.clone_published_repo(..., CloneLogin::AsContributor { ... })` —
//!    fresh tempdir, `git clone nostr://...`, then `ngit account create
//!    --local` so the cloner signs as a brand-new contributor identity. This is
//!    the realistic shape of "someone other than the maintainer submits a
//!    proposal".
//! 4. Contributor makes a feature branch with 2 commits ahead of main.
//! 5. `ngit send HEAD~2 --no-cover-letter`.
//! 6. Assert: exactly 2 kind-`GitPatch` events authored by the contributor show
//!    up on the grasp's relay (repo-relay) and on the vanilla user relay
//!    (contributor's write set). Assert: zero of those events carry the `t
//!    cover-letter` tag.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{CloneLogin, Harness, PublishRepoOpts};

#[tokio::test]
async fn send_two_patches_without_cover_letter() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    // --- 1. maintainer publishes the repo ------------------------------------
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("send-test maintainer".into()),
            identifier: Some("send-test-repo".into()),
            ..Default::default()
        })
        .await?;

    // --- 2. contributor clones + creates their own account ------------------
    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "send-test contributor".into(),
            },
        )
        .await?;

    // Pull the contributor's pubkey out of their .git/config so we can
    // filter for events they signed.
    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from cloned repo after AsContributor login")?;
    let contributor_keys =
        Keys::parse(&contributor_nsec).context("contributor nostr.nsec is not a valid key")?;
    let contributor_pubkey = contributor_keys.public_key();

    // --- 3. feature branch with two extra commits ----------------------------
    //
    // `ngit send HEAD~2 --no-cover-letter` will turn these into two patch
    // events. Mirrors the legacy `prep_git_repo` setup (t3.md / t4.md) so
    // anyone diffing the two harnesses can match commits one-to-one.
    run_git(&contributor, &["checkout", "-b", "feature"]).await?;

    std::fs::write(contributor.dir().join("t3.md"), "some content\n").context("write t3.md")?;
    run_git(&contributor, &["add", "t3.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t3.md", "--no-gpg-sign"],
    )
    .await?;

    std::fs::write(contributor.dir().join("t4.md"), "some content\n").context("write t4.md")?;
    run_git(&contributor, &["add", "t4.md"]).await?;
    run_git(
        &contributor,
        &["commit", "-m", "add t4.md", "--no-gpg-sign"],
    )
    .await?;

    // --- 4. ngit send --------------------------------------------------------
    let send_output = contributor
        .ngit(["send", "HEAD~2", "--no-cover-letter"])
        .output()
        .await
        .context("failed to spawn ngit send")?;
    assert!(
        send_output.status.success(),
        "ngit send exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        send_output.status,
        String::from_utf8_lossy(&send_output.stdout),
        String::from_utf8_lossy(&send_output.stderr),
    );

    // --- 5. assertions on both relays ----------------------------------------
    //
    // Patches fan out to the repo's relays (here: the grasp's relay
    // surface) plus the user's write relays (here: the vanilla "default").
    // Both must carry the two patches.
    for (label, events) in [
        (
            "grasp repo-relay",
            harness
                .grasp("repo")
                .events(
                    Filter::new()
                        .author(contributor_pubkey)
                        .kind(Kind::GitPatch),
                )
                .await?,
        ),
        (
            "vanilla user relay",
            harness
                .relay("default")
                .events(
                    Filter::new()
                        .author(contributor_pubkey)
                        .kind(Kind::GitPatch),
                )
                .await?,
        ),
    ] {
        let patches: Vec<&Event> = events.iter().filter(|e| !is_cover_letter(e)).collect();
        let cover_letters: Vec<&Event> = events.iter().filter(|e| is_cover_letter(e)).collect();

        assert_eq!(
            patches.len(),
            2,
            "expected exactly two patch events on {label} (excluding cover letters); \
             got {}: {events:?}",
            patches.len(),
        );
        assert!(
            cover_letters.is_empty(),
            "expected no cover-letter events on {label} when --no-cover-letter is set; \
             got {}: {cover_letters:?}",
            cover_letters.len(),
        );
    }

    Ok(())
}

/// Patch events carry the `["t", "cover-letter"]` tag when, and only when,
/// they're the cover letter for a patch series. Same predicate as the
/// legacy `is_cover_letter` helper in `tests/legacy/ngit_send.rs`.
fn is_cover_letter(event: &Event) -> bool {
    event.kind == Kind::GitPatch
        && event.tags.iter().any(|t| {
            let s = t.as_slice();
            s.first().map(String::as_str) == Some("t")
                && s.get(1).map(String::as_str) == Some("cover-letter")
        })
}

fn expect_ok(label: &str, out: std::process::Output) -> Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// Spawn `git <args>` inside the given repo, awaiting completion. Bail with
/// captured stdout/stderr on non-zero exit. Keeps the body of the test
/// readable when chaining 5 git commands in a row.
async fn run_git(repo: &test_harness::Repo, args: &[&str]) -> Result<()> {
    let label = format!("git {}", args.join(" "));
    expect_ok(
        &label,
        repo.git(args)
            .output()
            .await
            .with_context(|| format!("failed to spawn `{label}`"))?,
    )
}
