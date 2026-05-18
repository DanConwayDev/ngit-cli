//! Lighthouse test: end-to-end announce → push → clone over a `nostr://` URL
//! whose git server is a real `ngit-grasp` subprocess.
//!
//! This closes the loop that `tests/init_grasp.rs` could only half-prove.
//! `init_grasp` shows the kind-30617 announcement reaches the user's relay
//! and a bare repo materialises on the grasp's filesystem, but the
//! announcement stays in **purgatory** there — `init.rs:1195` short-circuits
//! the post-init `git push` under `NGITTEST=TRUE`, so the grasp's relay DB
//! never sees it and a REQ against the grasp returns nothing.
//!
//! This test issues the push manually after `ngit init`. The push:
//!
//! 1. Streams pack data to the grasp's git smart-http endpoint (`http://127.0.0.1:<port>/<npub>/<identifier>.git`).
//! 2. Publishes a kind-30618 repo-state event to the grasp's relay surface.
//! 3. **Graduates the announcement** out of purgatory and into the relay DB —
//!    the same step that would happen on a real GRASP server when a real client
//!    first pushes refs after announcing.
//!
//! Once graduation happens, REQs against the grasp return the announcement
//! and the state event, the bare repo on disk has `refs/heads/main`
//! pointing at the committed oid, and a fresh `git clone nostr://...`
//! resolves the same URL end-to-end and reproduces the committed file.
//!
//! ## Cloner identity
//!
//! The clone is driven from a second `fresh_repo()` rather than reusing the
//! publisher's working tree. Two reasons:
//!
//! 1. A `git clone <url> <subdir>` from the publisher's tempdir would succeed
//!    even if `git-remote-nostr` had silently fallen back to a local-disk
//!    lookup; running from a *different* tempdir proves the bytes travelled
//!    through the grasp.
//! 2. Separate harness-env-configured `Command`s mean we never accidentally
//!    inherit the publisher's `.git/config` (origin URL, nostr.nsec, etc.). The
//!    cloner has only the harness env vars and `PATH` — the same view a real
//!    first-time clone would see.

use std::path::Path;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::Harness;

/// 30618 — `Kind::GitRepoState`. There's no `nostr-sdk` const for this in
/// 0.44; the project itself spells it `Kind::Custom(30618)` in
/// `src/lib/client.rs::STATE_KIND`.
const KIND_REPO_STATE: u16 = 30618;

#[tokio::test]
async fn announce_push_then_clone_via_nostr_url_over_grasp() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let publisher = harness.fresh_repo()?;
    let display_name = "lighthouse clone grasp";
    let identifier = "lighthouse-clone-grasp";

    // --- step 1: account create ----------------------------------------------
    let create_output = publisher
        .ngit(["account", "create", "--local", "--name", display_name])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    assert!(
        create_output.status.success(),
        "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        create_output.status,
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr),
    );

    let nsec = publisher
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    let keys = Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")?;
    let pubkey = keys.public_key();
    let npub = pubkey
        .to_bech32()
        .context("failed to bech32-encode the new account's public key")?;

    // --- step 2: a real commit with content ----------------------------------
    //
    // `init_grasp.rs` uses `--allow-empty`; here we want a tree that hashes
    // to something specific so the post-clone assertion can verify the
    // bytes round-tripped.
    std::fs::write(publisher.dir().join("README.md"), "hello, grasp!\n")
        .context("failed to write README.md in publisher repo")?;

    let add_output = publisher
        .git(["add", "README.md"])
        .output()
        .await
        .context("failed to spawn git add")?;
    assert!(
        add_output.status.success(),
        "git add exited non-zero ({:?})\nstderr: {}",
        add_output.status,
        String::from_utf8_lossy(&add_output.stderr),
    );

    let commit_output = publisher
        .git(["commit", "-m", "initial", "--no-gpg-sign"])
        .output()
        .await
        .context("failed to spawn git commit")?;
    assert!(
        commit_output.status.success(),
        "git commit exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        commit_output.status,
        String::from_utf8_lossy(&commit_output.stdout),
        String::from_utf8_lossy(&commit_output.stderr),
    );

    let head_before_push = publisher.snapshot()?;
    let main_oid = head_before_push
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after initial commit")?
        .clone();

    // --- step 3: ngit init ---------------------------------------------------
    let grasp_url = harness.grasp("repo").url().to_string();
    let init_output = publisher
        .ngit([
            "init",
            "--name",
            display_name,
            "--identifier",
            identifier,
            "--grasp-server",
            &grasp_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init")?;
    assert!(
        init_output.status.success(),
        "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        init_output.status,
        String::from_utf8_lossy(&init_output.stdout),
        String::from_utf8_lossy(&init_output.stderr),
    );

    // Pull the nostr:// clone URL straight out of init's stdout. Catches
    // regressions in URL construction more honestly than re-deriving the
    // shape from `npub` + `ws://...` + `identifier` here.
    let init_stdout = String::from_utf8_lossy(&init_output.stdout);
    let clone_url = extract_clone_url(&init_stdout).with_context(|| {
        format!(
            "no `clone url:` line in ngit init stdout. \
             full stdout was:\n{init_stdout}"
        )
    })?;
    assert!(
        clone_url.starts_with("nostr://"),
        "expected nostr:// clone URL from init stdout; got {clone_url}",
    );
    assert!(
        clone_url.contains(&npub),
        "clone URL {clone_url} does not contain the publisher's npub {npub}",
    );

    // --- step 4: the new bit — push refs through git-remote-nostr ------------
    //
    // git invokes `git-remote-nostr` via PATH (augmented by the harness)
    // and the helper:
    //   - uploads pack data to the grasp's smart-http endpoint
    //   - publishes a kind-30618 state event
    //   - causes the grasp to graduate the announcement out of purgatory
    let push_output = publisher
        .git(["push", "-u", "origin", "main"])
        .output()
        .await
        .context("failed to spawn git push")?;
    assert!(
        push_output.status.success(),
        "git push exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        push_output.status,
        String::from_utf8_lossy(&push_output.stdout),
        String::from_utf8_lossy(&push_output.stderr),
    );

    // --- assertion 1: the announcement has graduated -------------------------
    //
    // Before the push the kind-30617 was in the grasp's purgatory and a REQ
    // returned nothing. After the push it's in the relay DB and visible to
    // any client.
    let grasp = harness.grasp("repo");
    let announcements = grasp
        .events(Filter::new().author(pubkey).kind(Kind::GitRepoAnnouncement))
        .await?;
    assert_eq!(
        announcements.len(),
        1,
        "expected exactly one kind 30617 on the grasp after push; got {}: {:?}",
        announcements.len(),
        announcements,
    );
    let announcement = &announcements[0];
    let d_tag = announcement
        .tags
        .iter()
        .find_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("d") {
                s.get(1).cloned()
            } else {
                None
            }
        })
        .context("announcement on grasp is missing its d tag")?;
    assert_eq!(
        d_tag, identifier,
        "announcement d tag {d_tag:?} does not match --identifier {identifier:?}",
    );

    // --- assertion 2: a state event was published to the grasp ---------------
    let state_events = grasp
        .events(
            Filter::new()
                .author(pubkey)
                .kind(Kind::Custom(KIND_REPO_STATE)),
        )
        .await?;
    assert!(
        !state_events.is_empty(),
        "expected at least one kind {KIND_REPO_STATE} state event on the grasp after push; got 0",
    );

    // --- assertion 3: bare repo on disk has refs/heads/main pointing here ----
    let bare_repo_path = grasp
        .git_data_path()
        .join(&npub)
        .join(format!("{identifier}.git"));
    assert!(
        bare_repo_path.is_dir(),
        "expected the grasp's bare repo at {} after push",
        bare_repo_path.display(),
    );
    let bare_oid = read_bare_ref_oid(&bare_repo_path, "refs/heads/main")
        .with_context(|| format!("reading refs/heads/main from {}", bare_repo_path.display()))?;
    assert_eq!(
        bare_oid, main_oid,
        "bare repo's refs/heads/main ({bare_oid}) does not match publisher's local oid ({main_oid})",
    );

    // --- step 5 + assertion 4: clone over nostr:// works end-to-end ---------
    //
    // A second harness-managed repo gives us:
    //   - harness env (NGITTEST=TRUE, NGIT_RELAY_DEFAULT_SET, etc.)
    //   - PATH augmented with the dir holding git-remote-nostr
    //   - an isolated tempdir, separate from the publisher's
    //
    // `fresh_repo` runs `git init` inside that tempdir, but `git clone`
    // into a sibling subdir doesn't care about that — it creates the
    // clone target itself.
    let cloner = harness.fresh_repo()?;
    let clone_dir_name = "cloned";
    let clone_target = cloner.dir().join(clone_dir_name);

    let clone_output = cloner
        .git(["clone", &clone_url, clone_dir_name])
        .output()
        .await
        .context("failed to spawn git clone")?;
    assert!(
        clone_output.status.success(),
        "git clone exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        clone_output.status,
        String::from_utf8_lossy(&clone_output.stdout),
        String::from_utf8_lossy(&clone_output.stderr),
    );

    assert!(
        clone_target.join(".git").is_dir(),
        "git clone reported success but .git is missing at {}",
        clone_target.display(),
    );

    let cloned_readme = clone_target.join("README.md");
    let cloned_contents = std::fs::read_to_string(&cloned_readme).with_context(|| {
        format!(
            "README.md missing from clone at {} — clone went through but checkout did not produce the working tree",
            cloned_readme.display()
        )
    })?;
    assert_eq!(
        cloned_contents, "hello, grasp!\n",
        "cloned README.md does not match the publisher's file",
    );

    let cloned_oid = read_local_ref_oid(&clone_target, "refs/heads/main")
        .with_context(|| format!("reading HEAD from clone at {}", clone_target.display()))?;
    assert_eq!(
        cloned_oid, main_oid,
        "cloned refs/heads/main ({cloned_oid}) does not match publisher's ({main_oid})",
    );

    Ok(())
}

/// Pull the first `nostr://...` URL printed after a `clone url:` /
/// `your clone URL:` label in `ngit init` stdout. Matches both casings
/// init.rs prints (depending on the co-maintainer code path).
fn extract_clone_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("clone url:") {
            let rest = line[idx + "clone url:".len()..].trim();
            if rest.starts_with("nostr://") {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Resolve `<refname>` inside a bare repository to its commit OID hex.
fn read_bare_ref_oid(bare_path: &Path, refname: &str) -> Result<String> {
    let repo = git2::Repository::open_bare(bare_path)
        .with_context(|| format!("open_bare {}", bare_path.display()))?;
    let reference = repo
        .find_reference(refname)
        .with_context(|| format!("find_reference {refname}"))?;
    let oid = reference
        .target()
        .with_context(|| format!("reference {refname} has no direct target"))?;
    Ok(oid.to_string())
}

/// Same as [`read_bare_ref_oid`] but for a normal (non-bare) working repo.
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
