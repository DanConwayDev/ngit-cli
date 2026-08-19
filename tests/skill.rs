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
    assert_eq!(setup_json["status"], "ok");
    assert!(repo.dir().join(".agents/skills/ngit/SKILL.md").is_file());
    assert!(repo.dir().join(".claude/skills/ngit/SKILL.md").is_file());
    assert!(!repo.dir().join(".agents/ngit-guidance.json").exists());
    assert!(!repo.dir().join("AGENTS.md").exists());
    assert!(!repo.dir().join("CLAUDE.md").exists());
    for (name, content) in agent_guidance::bundled_references() {
        assert_eq!(
            fs::read_to_string(repo.dir().join(".agents/skills/ngit/reference").join(name))?,
            *content
        );
        assert_eq!(
            fs::read_to_string(repo.dir().join(".claude/skills/ngit/reference").join(name))?,
            *content
        );
    }

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
    assert!(
        fs::read_to_string(repo.dir().join("CLAUDE.md"))?.contains(".claude/skills/ngit/SKILL.md")
    );
    Ok(())
}

/// ngit's own repository keeps single canonical `skills/ngit/SKILL.md` and
/// `skills/ngit/reference/*.md` files that the agent-specific discovery
/// locations symlink to, so the copies cannot drift. Install must follow those
/// symlinks, update the canonical files once, and commit them.
#[cfg(unix)]
#[tokio::test]
async fn install_updates_a_canonical_skill_copy_shared_by_symlinks() -> Result<()> {
    let harness = harness().await?;
    let repo = harness.fresh_repo()?;

    let canonical = repo.dir().join("skills/ngit/SKILL.md");
    fs::create_dir_all(canonical.parent().context("canonical skill has a parent")?)?;
    fs::write(
        &canonical,
        "---\nname: ngit\nversion: \"0.1\"\n---\n\nolder\n",
    )?;
    let canonical_reference = repo.dir().join("skills/ngit/reference");
    fs::create_dir_all(&canonical_reference)?;
    for (name, _) in agent_guidance::bundled_references() {
        fs::write(canonical_reference.join(name), "placeholder\n")?;
    }
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

    let install = repo.ngit(["skill", "install"]).output().await?;
    assert!(
        install.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    assert!(!fs::read_to_string(&canonical)?.contains("older"));
    assert!(!fs::read_to_string(canonical_reference.join("prs.md"))?.contains("placeholder"));
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
    for (name, content) in agent_guidance::bundled_references() {
        assert_eq!(
            fs::read_to_string(canonical_reference.join(name))?,
            *content
        );
    }

    let status = repo.ngit(["skill", "status", "--json"]).output().await?;
    assert!(status.status.success());
    let json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(json["installed"], true);
    assert_eq!(json["installed_version"], json["bundled_version"]);
    assert_eq!(json["update_available"], false);

    // The update is committed once, against the shared canonical targets.
    let committed = repo
        .git(["show", "--name-only", "--pretty=format:", "HEAD"])
        .output()
        .await?;
    let names = String::from_utf8(committed.stdout)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name == "skills/ngit/SKILL.md"),
        "no skill in {names:?}"
    );
    for (name, _) in agent_guidance::bundled_references() {
        let expected = format!("skills/ngit/reference/{name}");
        assert!(
            names.iter().any(|name| name == &expected),
            "missing {expected} in {names:?}"
        );
    }
    let worktree = repo.git(["status", "--porcelain"]).output().await?;
    assert!(String::from_utf8(worktree.stdout)?.trim().is_empty());
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
