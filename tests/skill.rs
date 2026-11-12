use std::fs;

use anyhow::{Context, Result};
use ngit::agent_guidance;
use test_harness::Harness;

async fn harness() -> Result<Harness> {
    Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await
}

#[tokio::test]
async fn status_works_without_a_nostr_remote_or_login() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let output = repo.ngit(["skill", "status", "--json"]).output().await?;

    assert!(
        output.status.success(),
        "skill status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["command_status"], "ok");
    assert_eq!(json["installed"], false);
    assert_eq!(json["is_maintainer"], serde_json::Value::Null);
    assert_eq!(json["reminders_enabled"], true);
    Ok(())
}

#[tokio::test]
async fn install_and_update_work_without_a_nostr_remote_or_login() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let setup = repo.ngit(["skill", "install", "--json"]).output().await?;
    assert!(
        setup.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let setup_json: serde_json::Value = serde_json::from_slice(&setup.stdout)?;
    assert_eq!(setup_json["command_status"], "ok");
    assert_eq!(setup_json["action"], "installed");
    assert_eq!(setup_json["changes_uncommitted"], true);
    let changed_files = setup_json["changed_files"]
        .as_array()
        .context("skill install changed_files is not an array")?;
    assert_eq!(changed_files.len(), 2);
    assert!(
        changed_files
            .iter()
            .any(|path| path == agent_guidance::SKILL_PATH)
    );
    assert!(
        changed_files
            .iter()
            .any(|path| path == agent_guidance::CLAUDE_SKILL_PATH)
    );
    assert!(repo.dir().join(".agents/skills/ngit/SKILL.md").is_file());
    assert!(repo.dir().join(".claude/skills/ngit/SKILL.md").is_file());
    assert!(!repo.dir().join(".agents/ngit-guidance.json").exists());
    assert!(!repo.dir().join("AGENTS.md").exists());
    assert!(!repo.dir().join("CLAUDE.md").exists());
    assert!(!repo.dir().join(".agents/skills/ngit/reference").exists());
    assert!(!repo.dir().join(".claude/skills/ngit/reference").exists());

    let status = repo.ngit(["skill", "status", "--json"]).output().await?;
    assert!(status.status.success());
    let json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(json["installed_version"], json["bundled_version"]);
    assert_eq!(json["update_available"], false);
    assert_eq!(
        json["managed_files"],
        serde_json::json!([
            ".agents/skills/ngit/SKILL.md",
            ".claude/skills/ngit/SKILL.md"
        ])
    );

    let custom = "# Maintainer wording\n\nUse ngit for collaboration.\n";
    fs::write(repo.dir().join("AGENTS.md"), custom)?;
    let update = repo.ngit(["skill", "upgrade"]).output().await?;
    assert!(
        update.status.success(),
        "skill update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(fs::read_to_string(repo.dir().join("AGENTS.md"))?, custom);
    Ok(())
}

#[tokio::test]
async fn ignored_skill_paths_are_not_reported_as_uncommitted() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    fs::write(repo.dir().join(".gitignore"), ".claude/\n")?;
    repo.git_ok(["add", ".gitignore"], "stage the ignore rule")
        .await?;
    repo.git_ok(
        ["commit", "-m", "ignore Claude files"],
        "commit the ignore rule",
    )
    .await?;

    let install = repo.ngit(["skill", "install", "--json"]).output().await?;

    assert!(
        install.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&install.stdout)?;
    assert_eq!(json["command_status"], "ok");
    assert_eq!(json["changes_uncommitted"], true);
    let changed_files = json["changed_files"]
        .as_array()
        .context("skill install changed_files is not an array")?;
    assert_eq!(changed_files.len(), 1);
    assert!(changed_files.iter().all(|path| {
        path.as_str()
            .is_some_and(|path| path.starts_with(".agents/skills/ngit/"))
    }));
    assert!(repo.dir().join(".claude/skills/ngit/SKILL.md").is_file());
    Ok(())
}

#[tokio::test]
async fn install_updates_existing_claude_without_creating_agents_files() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    fs::write(repo.dir().join("CLAUDE.md"), "# Existing Claude policy\n")?;

    let install = repo.ngit(["skill", "install"]).output().await?;

    assert!(
        install.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(repo.dir().join(".agents/skills/ngit/SKILL.md").is_file());
    assert!(repo.dir().join(".claude/skills/ngit/SKILL.md").is_file());
    assert!(!repo.dir().join("AGENTS.md").exists());
    assert!(fs::read_to_string(repo.dir().join("CLAUDE.md"))?.contains("`ngit --help`"));
    Ok(())
}

/// A repository can keep a single canonical `skills/ngit/SKILL.md`, and
/// formerly `skills/ngit/reference/*.md`, that the agent-specific discovery
/// locations symlink to, so the copies cannot drift. Install must follow those
/// symlinks, update the canonical skill once, unlink the `reference` symlinks
/// without touching their target, and leave the result uncommitted.
#[cfg(unix)]
#[tokio::test]
async fn install_updates_a_canonical_skill_copy_shared_by_symlinks() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;

    let canonical = repo.dir().join("skills/ngit/SKILL.md");
    fs::create_dir_all(canonical.parent().context("canonical skill has a parent")?)?;
    fs::write(
        &canonical,
        "---\nname: ngit\nversion: \"0.1\"\n---\n\nstale canonical skill body\n",
    )?;
    let canonical_reference = repo.dir().join("skills/ngit/reference");
    fs::create_dir_all(&canonical_reference)?;
    // The repository owns this directory; the discovery locations only link
    // to it.
    fs::write(canonical_reference.join("prs.md"), "placeholder\n")?;
    for relative in [
        ".agents/skills/ngit/SKILL.md",
        ".claude/skills/ngit/SKILL.md",
    ] {
        let path = repo.dir().join(relative);
        fs::create_dir_all(path.parent().context("skill path has a parent")?)?;
        std::os::unix::fs::symlink("../../../skills/ngit/SKILL.md", &path)?;
    }
    for dir in [".agents/skills/ngit", ".claude/skills/ngit"] {
        let reference = repo.dir().join(format!("{dir}/reference"));
        fs::create_dir_all(reference.parent().context("skill dir has a parent")?)?;
        std::os::unix::fs::symlink("../../../skills/ngit/reference", &reference)?;
    }
    repo.git_ok(["add", "-A"], "stage the canonical skill layout")
        .await?;
    repo.git_ok(
        ["commit", "-m", "add the canonical skill layout"],
        "commit the canonical skill layout",
    )
    .await?;
    let original_head = repo
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing before skill install")?
        .clone();

    let install = repo.ngit(["skill", "install", "--json"]).output().await?;
    assert!(
        install.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    assert!(!fs::read_to_string(&canonical)?.contains("stale canonical skill body"));
    for dir in [".agents/skills/ngit", ".claude/skills/ngit"] {
        assert!(
            fs::symlink_metadata(repo.dir().join(format!("{dir}/reference"))).is_err(),
            "install kept the `{dir}/reference` symlink"
        );
    }
    assert!(
        canonical_reference.join("prs.md").is_file(),
        "install followed a reference symlink into a repository-owned directory"
    );
    for relative in [
        ".agents/skills/ngit/SKILL.md",
        ".claude/skills/ngit/SKILL.md",
    ] {
        assert!(
            fs::symlink_metadata(repo.dir().join(relative))?
                .file_type()
                .is_symlink(),
            "install replaced the `{relative}` symlink"
        );
        assert_eq!(
            fs::read_to_string(repo.dir().join(relative))?,
            fs::read_to_string(&canonical)?
        );
    }

    let status = repo.ngit(["skill", "status", "--json"]).output().await?;
    assert!(status.status.success());
    let json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(json["installed"], true);
    assert_eq!(json["installed_version"], json["bundled_version"]);
    assert_eq!(json["update_available"], false);

    let install_json: serde_json::Value = serde_json::from_slice(&install.stdout)?;
    assert_eq!(install_json["command_status"], "ok");
    assert_eq!(install_json["changes_uncommitted"], true);
    assert!(
        install_json["changed_files"]
            .as_array()
            .context("canonical install changed_files is not an array")?
            .iter()
            .any(|path| path == "skills/ngit/SKILL.md")
    );
    assert_eq!(
        repo.snapshot()?
            .refs
            .get("refs/heads/main")
            .context("refs/heads/main missing after skill install")?,
        &original_head,
        "skill install moved HEAD"
    );
    let cached = repo.git(["diff", "--cached", "--quiet"]).output().await?;
    assert!(cached.status.success(), "skill install modified the index");

    // The shared canonical targets are left for the repository's normal
    // validation and commit workflow.
    let changed = repo.git(["diff", "--name-only"]).output().await?;
    let names = String::from_utf8(changed.stdout)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name == "skills/ngit/SKILL.md"),
        "no skill in {names:?}"
    );
    Ok(())
}

#[tokio::test]
async fn reinstalling_committed_guidance_reports_no_repository_changes() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let install = repo.ngit(["skill", "install"]).output().await?;
    assert!(install.status.success());
    repo.git_ok(["add", "-A"], "stage installed guidance")
        .await?;
    repo.git_ok(["commit", "-m", "install guidance"], "commit guidance")
        .await?;
    let original_head = repo
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing before reinstall")?
        .clone();

    fs::remove_dir_all(repo.dir().join(".agents/skills/ngit"))?;
    fs::remove_dir_all(repo.dir().join(".claude/skills/ngit"))?;
    let reinstall = repo.ngit(["skill", "install", "--json"]).output().await?;

    assert!(
        reinstall.status.success(),
        "skill reinstall failed: {}",
        String::from_utf8_lossy(&reinstall.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&reinstall.stdout)?;
    assert_eq!(json["command_status"], "ok");
    assert_eq!(json["action"], "installed");
    assert_eq!(json["changed_files"], serde_json::json!([]));
    assert_eq!(json["changes_uncommitted"], false);
    assert_eq!(
        repo.snapshot()?
            .refs
            .get("refs/heads/main")
            .context("refs/heads/main missing after reinstall")?,
        &original_head,
        "skill reinstall moved HEAD"
    );
    let status = repo.git(["status", "--porcelain"]).output().await?;
    assert!(
        String::from_utf8(status.stdout)?.trim().is_empty(),
        "restored guidance did not match the committed files"
    );
    Ok(())
}

/// `ngit skill install --global` writes the shared discovery layout into the
/// user's home directory (the harness sandbox) without touching the current
/// repository, and `skill status` reports the user scope.
#[tokio::test]
async fn global_install_targets_the_user_home_without_touching_the_repository() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;

    let install = repo
        .ngit(["skill", "install", "--global", "--json"])
        .output()
        .await?;
    assert!(
        install.status.success(),
        "global skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&install.stdout)?;
    assert_eq!(json["command_status"], "ok");
    assert_eq!(json["action"], "installed");
    assert_eq!(json["scope"], "global");
    assert_eq!(json["changes_uncommitted"], false);

    let home = harness.home();
    assert!(home.join(".agents/skills/ngit/SKILL.md").is_file());
    assert!(home.join(".claude/skills/ngit/SKILL.md").is_file());
    for dir in [".agents/skills/ngit", ".claude/skills/ngit"] {
        assert_eq!(
            fs::read_to_string(home.join(dir).join("SKILL.md"))?,
            agent_guidance::bundled_skill()
        );
        assert!(!home.join(dir).join("reference").exists());
    }
    assert!(!repo.dir().join(".agents").exists());
    assert!(!repo.dir().join(".claude").exists());

    let status = repo.ngit(["skill", "status", "--json"]).output().await?;
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(status_json["scopes"]["global"]["installed"], true);
    assert_eq!(
        status_json["scopes"]["global"]["installed_version"],
        status_json["scopes"]["global"]["bundled_version"]
    );
    assert_eq!(status_json["scopes"]["system"]["installed"], false);
    assert_eq!(status_json["scopes"]["repository"]["installed"], false);
    Ok(())
}

/// `ngit skill install --system` uses the single canonical directory and
/// honors `NGIT_SYSTEM_SKILLS_DIR` so non-root users can target it too.
#[tokio::test]
async fn system_install_uses_the_overridden_canonical_directory() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let system_dir = tempfile::tempdir().context("create system skills directory")?;

    let mut command = repo.ngit(["skill", "install", "--system", "--json"]);
    command.env("NGIT_SYSTEM_SKILLS_DIR", system_dir.path());
    let install = command.output().await?;
    assert!(
        install.status.success(),
        "system skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&install.stdout)?;
    assert_eq!(json["command_status"], "ok");
    assert_eq!(json["scope"], "system");
    assert_eq!(json["changes_uncommitted"], false);

    assert!(system_dir.path().join("ngit/SKILL.md").is_file());
    assert_eq!(
        fs::read_to_string(system_dir.path().join("ngit/SKILL.md"))?,
        agent_guidance::bundled_skill()
    );
    assert!(!system_dir.path().join("ngit/reference").exists());
    assert!(!system_dir.path().join(".agents").exists());
    assert!(!harness.home().join(".agents/skills/ngit").exists());
    assert!(!repo.dir().join(".agents").exists());
    Ok(())
}

/// `--no-agent-file` declines the pointer a repository install would
/// otherwise add to existing instruction files.
#[tokio::test]
async fn install_respects_no_agent_file() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let agents = "# Local instructions\n";
    fs::write(repo.dir().join("AGENTS.md"), agents)?;

    let install = repo
        .ngit(["skill", "install", "--no-agent-file"])
        .output()
        .await?;
    assert!(
        install.status.success(),
        "skill install with --no-agent-file failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(repo.dir().join(".agents/skills/ngit/SKILL.md").is_file());
    assert_eq!(fs::read_to_string(repo.dir().join("AGENTS.md"))?, agents);
    Ok(())
}

#[tokio::test]
async fn opt_out_is_reported_by_status() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let missing_scope = repo.ngit(["skill", "opt-out"]).output().await?;
    assert!(
        !missing_scope.status.success(),
        "skill opt-out unexpectedly accepted no scope"
    );

    let opt_out = repo.ngit(["skill", "opt-out", "--local"]).output().await?;
    assert!(
        opt_out.status.success(),
        "skill opt-out failed: {}",
        String::from_utf8_lossy(&opt_out.stderr)
    );
    assert_eq!(
        repo.config("nostr.skill-reminders").await?.as_deref(),
        Some("false")
    );

    let status = repo.ngit(["skill", "status", "--json"]).output().await?;
    assert!(status.status.success());
    let json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(json["reminders_enabled"], false);
    Ok(())
}

#[tokio::test]
async fn global_opt_out_works_without_a_git_worktree() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let global_config = repo.dir().join(".gitconfig");
    let outside = repo.dir().join("outside");
    fs::create_dir(&outside)?;

    // Point every global-config resolution ngit could use at the same file:
    // the `GIT_CONFIG_GLOBAL` override, and libgit2's `$HOME` / XDG search
    // for when the override is absent.
    let mut command = repo.ngit(["skill", "opt-out", "--global"]);
    let output = command
        .current_dir(outside)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("HOME", repo.dir())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .await
        .context("failed to run global skill opt-out")?;
    assert!(
        output.status.success(),
        "global skill opt-out failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!git2::Config::open(&global_config)?.get_bool("nostr.skill-reminders")?);
    Ok(())
}

/// Earlier versions installed reference files beside the skill. Upgrading
/// such an install must delete them and leave the deletion for the repository
/// to commit.
#[tokio::test]
async fn upgrade_removes_a_retired_reference_installed_by_a_release_candidate() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;
    let install = repo.ngit(["skill", "install"]).output().await?;
    assert!(
        install.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let retired = [
        ".agents/skills/ngit/reference/repo-settings.md",
        ".claude/skills/ngit/reference/repo-settings.md",
    ];
    for relative in retired {
        let path = repo.dir().join(relative);
        fs::create_dir_all(path.parent().context("reference has a parent")?)?;
        fs::write(path, "superseded reference\n")?;
    }
    repo.git_ok(["add", "-A"], "stage the retired reference")
        .await?;
    repo.git_ok(
        ["commit", "-m", "add a release-candidate reference"],
        "commit the retired reference",
    )
    .await?;
    let original_head = repo
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing before skill upgrade")?
        .clone();

    let upgrade = repo.ngit(["skill", "upgrade", "--json"]).output().await?;
    assert!(
        upgrade.status.success(),
        "skill upgrade failed: {}",
        String::from_utf8_lossy(&upgrade.stderr)
    );

    for relative in retired {
        assert!(
            !repo.dir().join(relative).exists(),
            "{relative} survived the upgrade"
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&upgrade.stdout)?;
    assert_eq!(json["command_status"], "ok");
    assert_eq!(json["changes_uncommitted"], true);
    let changed_files = json["changed_files"]
        .as_array()
        .context("skill upgrade changed_files is not an array")?;
    for relative in retired {
        assert!(
            changed_files.iter().any(|path| path == relative),
            "{relative} missing from {changed_files:?}"
        );
    }
    let changed = repo.git(["diff", "--name-status"]).output().await?;
    let lines = String::from_utf8(changed.stdout)?;
    for relative in retired {
        assert!(
            lines
                .lines()
                .any(|line| line.starts_with('D') && line.ends_with(relative)),
            "{relative} uncommitted deletion missing from {lines}"
        );
    }
    assert_eq!(
        repo.snapshot()?
            .refs
            .get("refs/heads/main")
            .context("refs/heads/main missing after skill upgrade")?,
        &original_head,
        "skill upgrade moved HEAD"
    );
    let cached = repo.git(["diff", "--cached", "--quiet"]).output().await?;
    assert!(cached.status.success(), "skill upgrade modified the index");
    Ok(())
}
