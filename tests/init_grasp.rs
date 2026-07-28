//! Lighthouse test: `ngit init --grasp-server <url>` publishes the kind 30617
//! repo-announcement event and the grasp accepts it (creating the
//! corresponding bare repository on disk).
//!
//! Flow:
//!
//! 1. Harness starts one vanilla relay (`default` role) and one `ngit-grasp`
//!    subprocess (`repo` role). The vanilla relay receives everything the user
//!    publishes to their own relay-list (kind 0, kind 10002, and — because
//!    grasp-derived relays are added to the user's write-list during init — a
//!    copy of the kind 30617 too). ngit-grasp receives a copy on its own relay
//!    endpoint as part of the same publish fan-out.
//! 2. `ngit account create --local --name "..."` generates fresh keys and
//!    publishes user metadata to the default-set relay (the vanilla one —
//!    ngit-grasp would reject kind 0 / 10002).
//! 3. A benign `--allow-empty` commit on `main` gives the working tree an oid;
//!    without it `git_repo.get_head_commit` (called inside ngit init) has
//!    nothing to read.
//! 4. `ngit init --name "..." --grasp-server <grasp.url()> -d` builds the
//!    announcement with the grasp's clone + relay tags, signs it, and publishes
//!    it to the user's relays + the grasp's relay.
//!
//! ## Why we *don't* REQ the grasp for the announcement
//!
//! `ngit-grasp` routes new announcements to **purgatory** rather than the
//! relay database — see
//! `ngit-grasp/src/nostr/builder.rs::AnnouncementResult::AcceptPurgatory`,
//! which returns `status: true` (client sees OK) with the relay DB write
//! deliberately skipped. The announcement only graduates to the DB once
//! its git data arrives via smart-http. Under `NGITTEST=TRUE`,
//! `src/bin/ngit/sub_commands/init.rs:1195` short-circuits the post-init
//! `git push`, so the announcement stays in purgatory and a REQ against
//! the grasp returns nothing.
//!
//! The bare repository on the grasp's filesystem is the observable side
//! effect of an *accepted* announcement: `add_to_purgatory` calls
//! `ensure_bare_repository` (see
//! `ngit-grasp/src/nostr/policy/announcement.rs:282`). If `lists_service`
//! had rejected the announcement, the directory would not exist.
//!
//! Asserting on the bare repo plus on the user-relay copy of the event
//! together pin down both halves of the chain: ngit published an event
//! with the right content (the vanilla relay shows it) and the grasp
//! accepted it (the directory exists).

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::Harness;

#[tokio::test]
async fn init_with_grasp_server_publishes_announcement_and_creates_bare_repo() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let repo = harness.fresh_repo()?;
    let display_name = "lighthouse init grasp";
    let identifier = "lighthouse-init-grasp";

    // --- step 1: account create -----------------------------------------------
    //
    // No `--relay` argument → the new account's metadata goes to whatever
    // `relay_default_set` resolves to inside ngit, which the harness has
    // populated with the vanilla "default" relay. ngit-grasp rejects
    // non-repo events, so the kind 0 / kind 10002 publishes would be no-ops
    // against it anyway.
    let create_output = repo
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

    let nsec = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    let keys = Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")?;
    let pubkey = keys.public_key();
    let npub = pubkey
        .to_bech32()
        .context("failed to bech32-encode the new account's public key")?;

    // --- step 2: a real commit on main ---------------------------------------
    //
    // libgit2 reads HEAD via `get_head_commit` inside `ngit init`; a fresh
    // `git init` has an unborn ref, so the simplest fix is one
    // `--allow-empty` commit.
    let commit_output = repo
        .git(["commit", "--allow-empty", "-m", "init", "--no-gpg-sign"])
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
    let initial_oid = repo
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after initial commit")?
        .clone();

    // --- step 3: ngit init ----------------------------------------------------
    //
    // `--name` + `--identifier` together satisfy the name half of
    // `validate_fresh`; `--grasp-server` satisfies the server half; `-d`
    // is a global flag that opts the rest of the form into non-interactive
    // defaults (specifically, it short-circuits the GRASP-server-selection
    // prompt for any blank fields).
    let grasp_url = harness.grasp("repo").url().to_string();
    let init_output = repo
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

    // `-d` selects init defaults; adopting repository guidance remains an
    // explicit follow-up action.
    for path in [
        "AGENTS.md",
        "CLAUDE.md",
        ".agents/skills/ngit/SKILL.md",
        ".claude/skills/ngit/SKILL.md",
        ".agents/ngit-guidance.json",
    ] {
        assert!(
            !repo.dir().join(path).exists(),
            "ngit init -d unexpectedly installed {path}"
        );
    }
    let init_stderr = String::from_utf8_lossy(&init_output.stderr);
    assert!(
        init_stderr.contains("ngit skill"),
        "init did not suggest explicit guidance setup: {init_stderr}"
    );
    assert_eq!(
        repo.snapshot()?
            .refs
            .get("refs/heads/main")
            .context("refs/heads/main missing after init")?,
        &initial_oid,
        "ngit init -d unexpectedly changed Git history"
    );

    // --- assertion 1: the announcement reached the user's relay --------------
    //
    // `send_events` fan-outs to the user's `relay-list` writes (the vanilla
    // "default" relay here) and the repo's announced relays. The vanilla
    // relay therefore receives a copy and stores it normally.
    let vanilla_announcements = harness
        .relay("default")
        .events(Filter::new().author(pubkey).kind(Kind::GitRepoAnnouncement))
        .await?;
    assert_eq!(
        vanilla_announcements.len(),
        1,
        "expected exactly one kind 30617 event on the vanilla relay, got {}: {:?}",
        vanilla_announcements.len(),
        vanilla_announcements,
    );

    let announcement = &vanilla_announcements[0];
    let d_tags: Vec<&str> = announcement
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("d") {
                s.get(1).map(String::as_str)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        d_tags,
        vec![identifier],
        "expected a single d tag matching --identifier; got {d_tags:?}",
    );

    // --- assertion 2: the grasp accepted the announcement -------------------
    //
    // The relay-DB query path is unusable here: under `NGITTEST=TRUE` the
    // post-init `git push` is skipped (init.rs:1195), so the announcement
    // never graduates from purgatory into the relay's database — REQs
    // return nothing. The observable proof that the grasp accepted the
    // announcement (passed `lists_service`, was added to purgatory) is the
    // bare repository that `add_to_purgatory` creates at
    // `<git_data_path>/<npub>/<identifier>.git`. If the announcement had
    // been rejected, the directory would not exist.
    let bare_repo = harness
        .grasp("repo")
        .git_data_path()
        .join(&npub)
        .join(format!("{identifier}.git"));
    assert!(
        bare_repo.is_dir(),
        "expected ngit-grasp to have created the bare repo at {} \
         (announcement was probably rejected; re-run with the grasp's \
         stderr piped to inherit() in test_harness/src/grasp.rs to \
         diagnose)",
        bare_repo.display(),
    );

    // Sanity check: it's actually a bare repo, not just an empty dir.
    assert!(
        bare_repo.join("HEAD").is_file(),
        "expected a HEAD file inside the bare repo at {}; got {:?}",
        bare_repo.display(),
        std::fs::read_dir(&bare_repo)
            .map(|d| d
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .collect::<Vec<_>>())
            .unwrap_or_default(),
    );

    let skill = repo.ngit(["skill", "install"]).output().await?;
    assert!(
        skill.status.success(),
        "maintainer skill install failed: {}",
        String::from_utf8_lossy(&skill.stderr)
    );
    assert!(repo.dir().join(".agents/skills/ngit/SKILL.md").is_file());
    assert!(repo.dir().join(".claude/skills/ngit/SKILL.md").is_file());
    assert!(!repo.dir().join(".agents/ngit-guidance.json").exists());
    assert!(!repo.dir().join("AGENTS.md").exists());
    assert!(!repo.dir().join("CLAUDE.md").exists());
    let skill_oid = repo
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after skill install")?
        .clone();
    assert_ne!(
        skill_oid, initial_oid,
        "maintainer skill install did not create its dedicated commit"
    );

    let second_init = repo
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
        .await?;
    assert!(
        second_init.status.success(),
        "repeat init failed: {}",
        String::from_utf8_lossy(&second_init.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&second_init.stderr).contains("ngit skill"),
        "repeat init suggested an already installed repository skill"
    );

    Ok(())
}

#[tokio::test]
async fn init_defaults_preserves_staged_changes_without_installing_guidance() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let repo = harness.fresh_repo()?;

    let create = repo
        .ngit(["account", "create", "--local", "--name", "staged init"])
        .output()
        .await?;
    assert!(
        create.status.success(),
        "account creation failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    let commit = repo
        .git(["commit", "--allow-empty", "-m", "init", "--no-gpg-sign"])
        .output()
        .await?;
    assert!(
        commit.status.success(),
        "initial commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let initial_oid = repo
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after initial commit")?
        .clone();

    std::fs::write(repo.dir().join("pending.txt"), "keep staged\n")?;
    let add = repo.git(["add", "pending.txt"]).output().await?;
    assert!(add.status.success(), "failed to stage pending.txt");

    let grasp_url = harness.grasp("repo").url().to_string();
    let init = repo
        .ngit([
            "init",
            "--name",
            "staged init",
            "--identifier",
            "staged-init",
            "--grasp-server",
            &grasp_url,
            "-d",
        ])
        .output()
        .await?;
    assert!(
        init.status.success(),
        "ngit init should leave guidance as an explicit follow-up: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let suggestion = String::from_utf8_lossy(&init.stderr);
    assert!(
        suggestion.contains("ngit skill"),
        "missing explicit guidance setup suggestion: {suggestion}"
    );
    assert!(
        !repo.dir().join(".agents/ngit-guidance.json").exists(),
        "guidance state was written despite the staged-index preflight failure"
    );
    assert_eq!(
        repo.snapshot()?
            .refs
            .get("refs/heads/main")
            .context("refs/heads/main missing after init")?,
        &initial_oid,
        "init created a guidance commit despite staged changes"
    );
    let cached = repo.git(["diff", "--cached", "--quiet"]).output().await?;
    assert!(
        !cached.status.success(),
        "the pre-existing staged change was unexpectedly cleared"
    );

    Ok(())
}

#[tokio::test]
async fn init_honors_repository_skill_reminder_opt_out() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let repo = harness.fresh_repo()?;

    let create = repo
        .ngit(["account", "create", "--local", "--name", "opted out"])
        .output()
        .await?;
    assert!(create.status.success());
    let commit = repo
        .git(["commit", "--allow-empty", "-m", "init", "--no-gpg-sign"])
        .output()
        .await?;
    assert!(commit.status.success());
    let opt_out = repo.ngit(["skill", "opt-out", "--local"]).output().await?;
    assert!(opt_out.status.success());

    let grasp_url = harness.grasp("repo").url().to_string();
    let init = repo
        .ngit([
            "init",
            "--name",
            "opted out",
            "--identifier",
            "opted-out",
            "--grasp-server",
            &grasp_url,
            "-d",
        ])
        .output()
        .await?;
    assert!(
        init.status.success(),
        "ngit init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&init.stderr).contains("ngit skill"),
        "init ignored the repository skill reminder opt-out"
    );

    Ok(())
}
