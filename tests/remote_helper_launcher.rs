//! Regression tests for the `git-remote-nostr` compatibility launcher.
//!
//! The launcher must locate the sibling `ngit` executable, forward
//! arguments unchanged behind the hidden internal entry point, keep
//! stdout clean for git's remote-helper protocol, and propagate the
//! child's exit status. Full protocol behavior (clone/fetch/push over
//! `nostr://` remotes) is covered by the harness-based integration
//! suites; these tests focus on the launcher contract itself.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tempfile::TempDir;

const LAUNCHER: &str = env!("CARGO_BIN_EXE_git-remote-nostr");

/// Run a freshly written executable, retrying briefly on ETXTBSY.
///
/// Tests here run as parallel threads of one process. While one test
/// still holds the write fd from `fs::copy`/`fs::write`, another test
/// may fork; the forked child inherits that fd until its own exec
/// completes, so exec'ing the fresh file can transiently fail with
/// "text file busy" — either in our spawn or inside the launcher when
/// it execs a freshly written fake `ngit`. Bounded deadline, then the
/// last result is returned for the caller's assertions.
fn run_fresh_executable(command: &mut Command) -> Result<Output> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let result = command.output();
        let busy = match &result {
            Err(err) => err.raw_os_error() == Some(26),
            Ok(output) => {
                !output.status.success()
                    && String::from_utf8_lossy(&output.stderr).contains("Text file busy")
            }
        };
        if !busy || Instant::now() >= deadline {
            return result.context("failed to run launcher");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An empty directory to use as the entire `PATH`, proving that
/// resolution cannot silently fall back to a `PATH`-provided binary.
fn empty_path_dir() -> Result<TempDir> {
    TempDir::new().context("failed to allocate empty PATH dir")
}

#[test]
fn version_resolves_sibling_ngit_without_path() -> Result<()> {
    let path_dir = empty_path_dir()?;
    let output = Command::new(LAUNCHER)
        .env("PATH", path_dir.path())
        .arg("--version")
        .output()
        .context("failed to run launcher")?;

    assert!(output.status.success(), "launcher exited with failure");
    let stdout = String::from_utf8(output.stdout)?;
    let expected_prefix = format!("v{}", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout
            .to_lowercase()
            .starts_with(&expected_prefix.to_lowercase()),
        "--version output changed: {stdout:?}"
    );
    Ok(())
}

#[test]
fn no_arguments_prints_usage() -> Result<()> {
    let path_dir = empty_path_dir()?;
    let output = Command::new(LAUNCHER)
        .env("PATH", path_dir.path())
        .output()
        .context("failed to run launcher")?;

    assert!(output.status.success(), "launcher exited with failure");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.to_lowercase().starts_with("nostr plugin for git"),
        "usage output changed: {stdout:?}"
    );
    Ok(())
}

/// Copy the launcher into `dir` so its sibling lookup sees whatever
/// `ngit` the test placed (or deliberately did not place) beside it.
fn install_launcher_copy(dir: &TempDir) -> Result<PathBuf> {
    let dest = dir
        .path()
        .join(format!("git-remote-nostr{}", std::env::consts::EXE_SUFFIX));
    fs::copy(LAUNCHER, &dest).context("failed to copy launcher")?;
    Ok(dest)
}

/// A fake sibling `ngit` used to observe exactly what the launcher
/// invokes it with.
#[cfg(unix)]
fn install_fake_ngit(dir: &TempDir, script_body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.path().join("ngit");
    fs::write(&path, format!("#!/bin/sh\n{script_body}\n"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn arguments_reach_internal_handler_unchanged() -> Result<()> {
    let dir = TempDir::new()?;
    let launcher = install_launcher_copy(&dir)?;
    install_fake_ngit(&dir, r#"printf '%s\n' "$@""#)?;
    let path_dir = empty_path_dir()?;

    let output = run_fresh_executable(Command::new(launcher).env("PATH", path_dir.path()).args([
        "origin",
        "nostr://npub1example/repo",
        "extra arg with spaces",
    ]))?;

    assert!(output.status.success(), "launcher exited with failure");
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec![
            "__git-remote-nostr",
            "origin",
            "nostr://npub1example/repo",
            "extra arg with spaces",
        ],
        "launcher altered the forwarded arguments"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn falls_back_to_path_resolution_without_sibling() -> Result<()> {
    let launcher_dir = TempDir::new()?;
    let launcher = install_launcher_copy(&launcher_dir)?;
    let path_dir = TempDir::new()?;
    install_fake_ngit(&path_dir, r#"printf '%s\n' "$@""#)?;

    let output = run_fresh_executable(
        Command::new(launcher)
            .env("PATH", path_dir.path())
            .args(["origin", "nostr://npub1example/repo"]),
    )?;

    assert!(output.status.success(), "launcher exited with failure");
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec!["__git-remote-nostr", "origin", "nostr://npub1example/repo"],
        "PATH-resolved ngit did not receive the forwarded arguments"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn child_exit_code_is_propagated() -> Result<()> {
    let dir = TempDir::new()?;
    let launcher = install_launcher_copy(&dir)?;
    install_fake_ngit(&dir, "exit 42")?;
    let path_dir = empty_path_dir()?;

    let output = run_fresh_executable(
        Command::new(launcher)
            .env("PATH", path_dir.path())
            .arg("--version"),
    )?;

    assert_eq!(
        output.status.code(),
        Some(42),
        "child exit code not propagated"
    );
    Ok(())
}

#[test]
fn missing_ngit_errors_on_stderr_without_stdout() -> Result<()> {
    let dir = TempDir::new()?;
    let launcher = install_launcher_copy(&dir)?;
    let path_dir = empty_path_dir()?;

    let output = run_fresh_executable(
        Command::new(launcher)
            .env("PATH", path_dir.path())
            .arg("--version"),
    )?;

    assert!(!output.status.success(), "expected failure without ngit");
    assert!(
        output.stdout.is_empty(),
        "launcher wrote protocol-corrupting stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("ngit"),
        "stderr should point at the missing ngit executable: {stderr:?}"
    );
    Ok(())
}
