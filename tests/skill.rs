use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result};

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "ngit-skill-cli-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        git2::Repository::init(&path)?;
        Ok(Self { path })
    }

    fn ngit(&self, args: &[&str]) -> Result<std::process::Output> {
        Command::new(assert_cmd::cargo::cargo_bin!("ngit"))
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .output()
            .context("failed to run ngit")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn status_works_without_a_nostr_remote_or_login() -> Result<()> {
    let repo = TestRepo::new()?;
    let output = repo.ngit(&["skill", "--status", "--json"])?;

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

#[test]
fn install_and_update_work_without_a_nostr_remote_or_login() -> Result<()> {
    let repo = TestRepo::new()?;
    let setup = repo.ngit(&["skill"])?;
    assert!(
        setup.status.success(),
        "skill install failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    assert!(repo.path().join(".agents/ngit-guidance.json").is_file());

    let update = repo.ngit(&["skill"])?;
    assert!(
        update.status.success(),
        "skill update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    Ok(())
}

#[test]
fn opt_out_is_reported_by_status() -> Result<()> {
    let repo = TestRepo::new()?;
    let opt_out = repo.ngit(&["skill", "--opt-out"])?;
    assert!(
        opt_out.status.success(),
        "skill opt-out failed: {}",
        String::from_utf8_lossy(&opt_out.stderr)
    );

    let status = repo.ngit(&["skill", "--status", "--json"])?;
    assert!(status.status.success());
    let json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(json["reminders_enabled"], false);
    Ok(())
}
