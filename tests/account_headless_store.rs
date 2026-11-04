//! Production file-backend policy: no NGITTEST or OS-store-disabling redirect.
#![cfg(target_os = "linux")]

use std::{path::PathBuf, process::Output, time::Duration};

use anyhow::{Context, Result};
use nostr::prelude::{Keys, ToBech32};
use serde_json::{Value, json};
use test_harness::{Harness, repo::Repo};

struct Probe {
    harness: Harness,
    repo: Repo,
}

impl Probe {
    async fn new() -> Result<Self> {
        let harness = Harness::builder(
            env!("CARGO_BIN_EXE_ngit"),
            env!("CARGO_BIN_EXE_git-remote-nostr"),
        )
        .build()
        .await?;
        let repo = harness.fresh_repo()?;
        std::fs::create_dir_all(harness.home().join("runtime"))?;
        std::fs::write(harness.home().join(".gitconfig"), "")?;
        Ok(Self { harness, repo })
    }

    fn credentials(&self) -> PathBuf {
        self.harness
            .home()
            .join(".local/share/ngit/credentials.json")
    }

    async fn run(&self, args: &[&str], failing_bus: bool) -> Result<Output> {
        let home = self.harness.home();
        let mut command = self.repo.ngit(args);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NGIT_SECRET_STORAGE", "file")
            .kill_on_drop(true);
        if failing_bus {
            // A configured but inaccessible OS backend must not be treated as
            // absent. This is a real failed transport, not an ngit test switch.
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!(
                    "unix:path={}",
                    home.join("runtime/unavailable-bus").display()
                ),
            );
        }
        tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .context("headless account command exceeded deadline")?
            .context("spawn account command")
    }

    fn stored(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(self.credentials())?)?)
    }

    fn snapshot(&self) -> Result<Vec<Vec<u8>>> {
        [
            self.credentials(),
            self.harness.home().join(".local/share/ngit/accounts.json"),
            self.harness.home().join(".gitconfig"),
        ]
        .iter()
        .map(|path| std::fs::read(path).map_err(Into::into))
        .collect()
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn production_headless_key_retention_aliasing_and_explicit_deletion() -> Result<()> {
    let probe = Probe::new().await?;
    let key = probe.harness.home().join("synthetic-key");
    std::fs::write(&key, format!("{:064}", 1))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
    success(
        probe
            .run(
                &[
                    "account",
                    "login",
                    "--nsec-file",
                    key.to_str().unwrap(),
                    "--profile",
                    "synthetic",
                    "--offline",
                    "--json",
                ],
                false,
            )
            .await?,
    );
    let before = probe.snapshot()?;
    assert!(
        !probe
            .run(
                &["account", "logout", "--profile", "synthetic", "--json"],
                false
            )
            .await?
            .status
            .success()
    );
    assert!(
        probe.snapshot()? == before,
        "refused logout changed credentials, registry, or Git config"
    );
    success(
        probe
            .run(
                &[
                    "account",
                    "logout",
                    "--global",
                    "--keep-credentials",
                    "--json",
                ],
                false,
            )
            .await?,
    );
    assert_eq!(std::fs::read(probe.credentials())?, before[0]);
    success(
        probe
            .run(
                &[
                    "account",
                    "login",
                    "synthetic",
                    "--profile",
                    "second",
                    "--offline",
                    "--json",
                ],
                false,
            )
            .await?,
    );
    let before_failure = probe.snapshot()?;
    assert!(
        !probe
            .run(
                &[
                    "account",
                    "logout",
                    "--profile",
                    "second",
                    "--forget",
                    "--json"
                ],
                true
            )
            .await?
            .status
            .success()
    );
    assert!(
        probe.snapshot()? == before_failure,
        "failed deletion changed credentials, registry, or Git config"
    );
    let config = std::fs::read_to_string(probe.harness.home().join(".gitconfig"))?;
    assert!(!config.contains("nsec"));
    assert_eq!(
        std::fs::metadata(probe.credentials())?.permissions().mode() & 0o777,
        0o600
    );
    success(
        probe
            .run(
                &[
                    "account",
                    "logout",
                    "--profile",
                    "second",
                    "--forget",
                    "--json",
                ],
                false,
            )
            .await?,
    );
    let remaining = probe.stored()?;
    assert!(
        remaining
            .as_object()
            .unwrap()
            .keys()
            .all(|key| key.starts_with("nostr/forgotten-profile:"))
    );
    assert!(
        !probe
            .run(
                &["account", "login", "synthetic", "--offline", "--json"],
                false
            )
            .await?
            .status
            .success()
    );
    Ok(())
}

#[tokio::test]
async fn production_headless_remote_deletion_preserves_other_named_sessions() -> Result<()> {
    let probe = Probe::new().await?;
    let user = Keys::generate().public_key().to_bech32()?;
    let app = Keys::generate();
    let record = |signer: Keys| -> Result<String> {
        Ok(serde_json::to_string(&json!({
            "version": 1, "type": "bunker", "npub": user,
            "bunker_uri": format!("bunker://{}?relay=ws%3A%2F%2F127.0.0.1%3A1", signer.public_key().to_hex()),
            "client_nsec": app.secret_key().to_bech32()?,
        }))?)
    };
    let first = record(Keys::generate())?;
    let second = record(Keys::generate())?;
    let data = json!({
        "nostr/signer-alias:work": first,
        "nostr/signer-alias:personal": second,
        "nostr/alias:work": user,
        "nostr/alias:personal": user,
        "nostr/alias-credential:work": "signer-alias:work",
        "nostr/alias-credential:personal": "signer-alias:personal",
    });
    std::fs::create_dir_all(probe.credentials().parent().unwrap())?;
    std::fs::write(probe.credentials(), serde_json::to_vec(&data)?)?;
    std::fs::write(
        probe.harness.home().join(".local/share/ngit/accounts.json"),
        serde_json::to_vec(
            &json!({"accounts": [user], "aliases": {"work": user, "personal": user}}),
        )?,
    )?;
    std::fs::write(
        probe.harness.home().join(".gitconfig"),
        "[nostr]\n signer = work\n",
    )?;
    let saved = std::fs::read(probe.credentials())?;
    success(
        probe
            .run(
                &["account", "logout", "--keep-credentials", "--json"],
                false,
            )
            .await?,
    );
    assert!(std::fs::read(probe.credentials())? == saved);
    // Restore only the public selection to exercise deletion of this fixture.
    std::fs::write(
        probe.harness.home().join(".gitconfig"),
        "[nostr]\n signer = work\n",
    )?;
    let before = probe.snapshot()?;
    assert!(
        !probe
            .run(&["account", "logout", "--profile", "work", "--json"], true)
            .await?
            .status
            .success()
    );
    assert!(
        probe.snapshot()? == before,
        "refused logout changed credentials, registry, or Git config"
    );
    // An existing default socket is a configured backend even without an
    // explicit bus address. A refused connection must preserve the same state.
    let socket = probe.harness.home().join("runtime/bus");
    drop(std::os::unix::net::UnixListener::bind(&socket)?);
    assert!(
        !probe
            .run(&["account", "logout", "--profile", "work", "--json"], false)
            .await?
            .status
            .success()
    );
    assert!(
        probe.snapshot()? == before,
        "failed default bus changed saved state"
    );
    std::fs::remove_file(socket)?;
    success(
        probe
            .run(&["account", "logout", "--profile", "work", "--json"], false)
            .await?,
    );
    let remaining = probe.stored()?;
    assert!(remaining.get("nostr/signer-alias:work").is_none());
    assert!(remaining.get("nostr/alias:work").is_none());
    assert_eq!(remaining["nostr/signer-alias:personal"], second);
    assert_eq!(
        remaining["nostr/alias-credential:personal"],
        "signer-alias:personal"
    );
    success(
        probe
            .run(&["account", "logout", "--all", "--json"], false)
            .await?,
    );
    assert!(
        probe
            .stored()?
            .as_object()
            .unwrap()
            .keys()
            .all(|key| key.starts_with("nostr/forgotten-profile:"))
    );
    Ok(())
}
