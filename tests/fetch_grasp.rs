//! Lighthouse test: `git fetch` over a `nostr://` URL retrieves commits
//! pushed to the grasp **after** the initial clone.
//!
//! Symmetric counterpart to `tests/clone_grasp.rs`. Clone proves the read
//! path works at announcement time; fetch proves it keeps working as the
//! publisher advances refs. Together they pin down both halves of the
//! single-grasp happy path.
//!
//! Flow:
//!
//! 1. Publisher: `ngit account create`, commit on `main`, branch `vnext` with
//!    its own commit, `ngit init --grasp-server <url>`, then push both
//!    branches.
//! 2. Cloner: fresh repo, `git clone <nostr-url>` into a subdir.
//! 3. Publisher: add a second commit on `main`, push.
//! 4. Cloner: `git fetch origin` from inside the cloned tree.
//! 5. Assert that the cloner's `refs/remotes/origin/main` advanced to the new
//!    oid and `refs/remotes/origin/vnext` matches the publisher's.
//!
//! ## Why both branches at clone time?
//!
//! Legacy `tests/legacy/git_remote_nostr/fetch.rs::fetch_downloads_*` drove
//! the remote helper directly via PTY with two `fetch <oid> <ref>` lines.
//! In a real git workflow that's what `git fetch` does behind the scenes
//! once the remote-tracking refs already exist. To exercise the same
//! "fetch multiple branches" surface from the harness we just clone a repo
//! that already has both, then assert post-clone refs and post-fetch
//! refs separately. No PTY needed.

use std::path::Path;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::Harness;

#[tokio::test]
async fn fetch_advances_remote_tracking_refs_after_publisher_pushes() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let publisher = harness.fresh_repo()?;
    let display_name = "lighthouse fetch grasp";
    let identifier = "lighthouse-fetch-grasp";

    // --- step 1: account create -----------------------------------------------
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
    let npub = keys
        .public_key()
        .to_bech32()
        .context("failed to bech32-encode the new account's public key")?;

    // --- step 2: two branches with distinct content --------------------------
    std::fs::write(publisher.dir().join("README.md"), "main v1\n")
        .context("failed to write README.md")?;
    run_git_ok(&publisher, ["add", "README.md"], "git add README").await?;
    run_git_ok(
        &publisher,
        ["commit", "-m", "initial main", "--no-gpg-sign"],
        "git commit initial main",
    )
    .await?;

    run_git_ok(
        &publisher,
        ["checkout", "-b", "vnext"],
        "git checkout -b vnext",
    )
    .await?;
    std::fs::write(publisher.dir().join("vnext.md"), "vnext content\n")
        .context("failed to write vnext.md")?;
    run_git_ok(&publisher, ["add", "vnext.md"], "git add vnext.md").await?;
    run_git_ok(
        &publisher,
        ["commit", "-m", "vnext commit", "--no-gpg-sign"],
        "git commit vnext",
    )
    .await?;
    run_git_ok(&publisher, ["checkout", "main"], "git checkout main").await?;

    let snapshot_before_push = publisher.snapshot()?;
    let main_oid_v1 = snapshot_before_push
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after initial commit")?
        .clone();
    let vnext_oid = snapshot_before_push
        .refs
        .get("refs/heads/vnext")
        .context("refs/heads/vnext missing after vnext commit")?
        .clone();

    // --- step 3: ngit init --------------------------------------------------
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

    let init_stdout = String::from_utf8_lossy(&init_output.stdout);
    let clone_url = extract_clone_url(&init_stdout).with_context(|| {
        format!("no `clone url:` line in ngit init stdout. full stdout was:\n{init_stdout}")
    })?;
    assert!(
        clone_url.starts_with("nostr://"),
        "expected nostr:// clone URL; got {clone_url}",
    );
    assert!(
        clone_url.contains(&npub),
        "clone URL {clone_url} does not contain publisher's npub {npub}",
    );

    // --- step 4: push both branches at once ---------------------------------
    //
    // A single `git push origin main vnext` updates both refs over the
    // same wire session, matching how a real first-push from a working
    // repo behaves once `ngit init` has rewritten origin to nostr://.
    run_git_ok(
        &publisher,
        ["push", "-u", "origin", "main", "vnext"],
        "git push main+vnext",
    )
    .await?;

    // --- step 5: cloner clones, verifies both branches arrived ---------------
    let cloner = harness.fresh_repo()?;
    let clone_dir_name = "cloned";
    let clone_target = cloner.dir().join(clone_dir_name);
    run_git_ok(
        &cloner,
        ["clone", &clone_url, clone_dir_name],
        "git clone over nostr://",
    )
    .await?;

    assert!(
        clone_target.join(".git").is_dir(),
        "git clone succeeded but .git missing at {}",
        clone_target.display(),
    );

    let cloned_main_v1 = read_local_ref_oid(&clone_target, "refs/remotes/origin/main")
        .with_context(|| {
            format!(
                "reading refs/remotes/origin/main from clone at {}",
                clone_target.display()
            )
        })?;
    assert_eq!(
        cloned_main_v1, main_oid_v1,
        "clone's origin/main ({cloned_main_v1}) does not match publisher's ({main_oid_v1})",
    );
    let cloned_vnext = read_local_ref_oid(&clone_target, "refs/remotes/origin/vnext")
        .with_context(|| {
            format!(
                "reading refs/remotes/origin/vnext from clone at {}",
                clone_target.display()
            )
        })?;
    assert_eq!(
        cloned_vnext, vnext_oid,
        "clone's origin/vnext ({cloned_vnext}) does not match publisher's ({vnext_oid})",
    );

    // --- step 6: publisher advances main with a second commit ----------------
    std::fs::write(publisher.dir().join("README.md"), "main v2\n")
        .context("failed to overwrite README.md for second commit")?;
    run_git_ok(&publisher, ["add", "README.md"], "git add v2").await?;
    run_git_ok(
        &publisher,
        ["commit", "-m", "second main", "--no-gpg-sign"],
        "git commit second main",
    )
    .await?;
    run_git_ok(
        &publisher,
        ["push", "origin", "main"],
        "git push second main",
    )
    .await?;

    let main_oid_v2 = publisher
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after second commit")?
        .clone();
    assert_ne!(
        main_oid_v2, main_oid_v1,
        "second commit did not advance refs/heads/main",
    );

    // --- step 7: cloner fetches, sees the advance ----------------------------
    //
    // The clone target is a separate working tree from `cloner.dir()`.
    // `Repo::git` runs commands in `cloner.dir()`, so steer git with `-C`
    // at the clone target to keep the env intact (harness vars, PATH with
    // git-remote-nostr).
    let clone_target_str = clone_target
        .to_str()
        .context("clone target path is not utf-8")?;
    let fetch_output = cloner
        .git(["-C", clone_target_str, "fetch", "origin"])
        .output()
        .await
        .context("failed to spawn git fetch")?;
    assert!(
        fetch_output.status.success(),
        "git fetch exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        fetch_output.status,
        String::from_utf8_lossy(&fetch_output.stdout),
        String::from_utf8_lossy(&fetch_output.stderr),
    );

    let cloned_main_v2 = read_local_ref_oid(&clone_target, "refs/remotes/origin/main")
        .with_context(|| {
            format!(
                "reading refs/remotes/origin/main from clone at {} after fetch",
                clone_target.display()
            )
        })?;
    assert_eq!(
        cloned_main_v2, main_oid_v2,
        "clone's origin/main did not advance after fetch: \
         got {cloned_main_v2}, publisher's is {main_oid_v2}",
    );

    // vnext should be unchanged.
    let cloned_vnext_after = read_local_ref_oid(&clone_target, "refs/remotes/origin/vnext")
        .with_context(|| {
            format!(
                "reading refs/remotes/origin/vnext from clone at {} after fetch",
                clone_target.display()
            )
        })?;
    assert_eq!(
        cloned_vnext_after, vnext_oid,
        "vnext should not have moved; got {cloned_vnext_after}, expected {vnext_oid}",
    );

    Ok(())
}

/// Run a `git` subprocess inside `repo`, asserting it exited successfully.
/// Centralises the verbose stdout/stderr dump on failure so the call sites
/// stay readable.
async fn run_git_ok<I, S>(repo: &test_harness::Repo, args: I, label: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn {label}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(())
}

/// Pull the first `nostr://...` URL printed after a `clone url:` /
/// `your clone URL:` label in `ngit init` stdout. Case-insensitive prefix
/// match, same shape as `tests/clone_grasp.rs::extract_clone_url`.
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

/// Resolve `<refname>` inside a normal (non-bare) working repo to its
/// commit OID hex. Same helper shape as `tests/clone_grasp.rs`.
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
