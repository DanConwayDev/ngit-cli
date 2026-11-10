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
    bus: Option<(String, tokio::process::Child)>,
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
        Ok(Self {
            harness,
            repo,
            bus: None,
        })
    }

    async fn with_bus() -> Result<Self> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut probe = Self::new().await?;
        let config = probe.harness.home().join("bus.conf");
        // No service directories: an installed keyring on the test host must
        // never be activated or mistaken for this test's private service.
        std::fs::write(
            &config,
            r#"<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen><policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/></policy></busconfig>"#,
        )?;
        let mut child = tokio::process::Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address=1")
            .arg(format!("--config-file={}", config.display()))
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut address = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut address),
        )
        .await??;
        anyhow::ensure!(
            !address.trim().is_empty(),
            "private bus did not publish an address"
        );
        probe.bus = Some((address.trim().to_string(), child));
        Ok(probe)
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
        } else if let Some((address, _)) = &self.bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        tokio::time::timeout(Duration::from_secs(60), command.output())
            .await
            .with_context(|| {
                format!(
                    "account command {} exceeded deadline",
                    args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
                )
            })?
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

#[tokio::test]
async fn production_bus_without_secret_service_supports_file_account_lifecycle() -> Result<()> {
    let probe = Probe::with_bus().await?;
    let keys = Keys::generate();
    let key = probe.harness.home().join("synthetic-key");
    std::fs::write(&key, keys.secret_key().to_bech32()?)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
    for policy in ["auto", "file"] {
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
                        "--secret-storage",
                        policy,
                        "--offline",
                        "--json",
                    ],
                    false,
                )
                .await?,
        );
        let output = probe
            .run(&["account", "whoami", "--offline", "--json"], false)
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            document["accounts"][0]["npub"],
            keys.public_key().to_bech32()?
        );
        assert_eq!(document["accounts"][0]["active"], true);
        assert_eq!(document["unavailable_accounts"], json!([]));
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
        assert_eq!(probe.snapshot()?, before);
        success(
            probe
                .run(&["account", "logout", "--all", "--forget", "--json"], false)
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
    }
    Ok(())
}

#[track_caller]
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
    let failure = probe
        .run(
            &[
                "account",
                "logout",
                "--profile",
                "second",
                "--forget",
                "--json",
            ],
            true,
        )
        .await?;
    assert!(!failure.status.success());
    let diagnostic = String::from_utf8_lossy(&failure.stderr);
    assert!(diagnostic.contains("unlock it or restore its session service"));
    assert!(!diagnostic.contains("credential backend initialization failed"));
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
    remote_deletion_preserves_other_named_sessions(Probe::new().await?).await
}

#[tokio::test]
async fn production_bus_without_service_remote_deletion_preserves_other_named_sessions()
-> Result<()> {
    remote_deletion_preserves_other_named_sessions(Probe::with_bus().await?).await
}

async fn remote_deletion_preserves_other_named_sessions(probe: Probe) -> Result<()> {
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
    if probe.bus.is_none() {
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
    }
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

/// Runs against a real, private Secret Service, not ngit's debug redirects.
/// Explicit opt-in keeps the ordinary suite independent of desktop packages.
#[tokio::test]
#[ignore = "requires gnome-keyring-daemon; run in the account credential-store VM"]
async fn production_secret_service_mixed_storage_and_locked_deletion() -> Result<()> {
    use std::{os::unix::fs::PermissionsExt, process::Stdio};

    use tokio::io::AsyncWriteExt;

    let probe = Probe::with_bus().await?;
    let home = probe.harness.home();
    std::fs::set_permissions(home.join("runtime"), std::fs::Permissions::from_mode(0o700))?;
    let address = &probe.bus.as_ref().unwrap().0;
    let mut daemon_command = tokio::process::Command::new("gnome-keyring-daemon");
    daemon_command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_RUNTIME_DIR", home.join("runtime"))
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .args(["--foreground", "--unlock", "--components=secrets"])
        .arg("--control-directory")
        .arg(home.join("runtime/keyring"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut daemon = daemon_command
        .spawn()
        .context("start private Secret Service")?;
    daemon
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-test-password")
        .await?;
    let bus = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let proxy = zbus::fdo::DBusProxy::new(&bus).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if proxy
                .name_has_owner("org.freedesktop.secrets".try_into()?)
                .await?
            {
                return anyhow::Ok(());
            }
            anyhow::ensure!(
                daemon.try_wait()?.is_none(),
                "private keyring exited before readiness"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("Secret Service did not become ready")??;

    let alpha = Keys::generate();
    let beta = Keys::generate();
    for (profile, key, policy) in [("alpha", &alpha, "auto"), ("beta", &beta, "file")] {
        let key_path = home.join(profile);
        std::fs::write(&key_path, key.secret_key().to_bech32()?)?;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        success(
            probe
                .run(
                    &[
                        "account",
                        "login",
                        "--nsec-file",
                        key_path.to_str().unwrap(),
                        "--profile",
                        profile,
                        "--secret-storage",
                        policy,
                        "--offline",
                        "--json",
                    ],
                    false,
                )
                .await?,
        );
        if profile == "alpha" {
            assert!(
                !probe.credentials().exists(),
                "auto login unexpectedly fell back to a file"
            );
        }
    }
    let stored = probe.stored()?;
    assert!(
        stored
            .get(format!("nostr/{}", alpha.public_key().to_bech32()?).as_str())
            .is_none()
    );
    assert!(
        stored
            .get(format!("nostr/{}", beta.public_key().to_bech32()?).as_str())
            .is_some()
    );
    for (profile, keys) in [("alpha", &alpha), ("beta", &beta)] {
        let output = probe
            .run(
                &["--signer", profile, "account", "export-keys", "--json"],
                false,
            )
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["npub"], keys.public_key().to_bech32()?);
        assert!(
            result["nsec"] == keys.secret_key().to_bech32()?,
            "exported credential differs"
        );
    }
    let before = probe.snapshot()?;
    assert!(
        !probe
            .run(&["account", "logout", "--all", "--json"], false)
            .await?
            .status
            .success()
    );
    assert_eq!(
        probe.snapshot()?,
        before,
        "identity-key refusal changed saved state"
    );

    let reply = bus
        .call_method(
            Some("org.freedesktop.secrets"),
            "/org/freedesktop/secrets",
            Some("org.freedesktop.Secret.Service"),
            "ReadAlias",
            &"default",
        )
        .await?;
    let collection: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    anyhow::ensure!(
        collection.as_str() != "/",
        "keyring has no default collection"
    );
    let reply = bus
        .call_method(
            Some("org.freedesktop.secrets"),
            "/org/freedesktop/secrets",
            Some("org.freedesktop.Secret.Service"),
            "Lock",
            &vec![collection.clone()],
        )
        .await?;
    let (locked, _): (
        Vec<zbus::zvariant::OwnedObjectPath>,
        zbus::zvariant::OwnedObjectPath,
    ) = reply.body().deserialize()?;
    assert!(locked.contains(&collection));
    let output = probe
        .run(
            &[
                "account",
                "login",
                "--nsec-file",
                home.join("beta").to_str().unwrap(),
                "--profile",
                "alpha",
                "--no-activate",
                "--secret-storage",
                "file",
                "--offline",
                "--json",
            ],
            false,
        )
        .await?;
    assert!(
        !output.status.success(),
        "locked OS profile was rebound to a different identity"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("choose a new profile name"));
    assert_eq!(
        probe.snapshot()?,
        before,
        "refused profile reassignment changed saved state"
    );
    let output = probe
        .run(&["account", "logout", "--all", "--forget", "--json"], false)
        .await?;
    assert!(
        !output.status.success(),
        "locked keyring was treated as empty"
    );
    assert_eq!(
        probe.snapshot()?,
        before,
        "locked-store failure changed saved state"
    );

    let retained = probe
        .run(
            &["account", "logout", "--all", "--keep-credentials", "--json"],
            false,
        )
        .await?;
    assert!(
        retained.status.success(),
        "{}",
        String::from_utf8_lossy(&retained.stderr)
    );
    let result: Value = serde_json::from_slice(&retained.stdout)?;
    assert!(
        result["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|account| account["active"] == false)
    );
    let after = probe.snapshot()?;
    assert_eq!(
        after[..2],
        before[..2],
        "retention changed credentials or index"
    );
    assert!(!std::fs::read_to_string(home.join(".gitconfig"))?.contains("signer ="));
    // Restore only this test's public selection for the subsequent deletion.
    std::fs::write(home.join(".gitconfig"), &before[2])?;

    // Re-open the persisted keyring as a new desktop session would.
    daemon.kill().await?;
    daemon.wait().await?;
    daemon_command.stdin(Stdio::piped());
    let mut reopened = daemon_command.spawn()?;
    reopened
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-test-password")
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result = bus
                .call_method(
                    Some("org.freedesktop.secrets"),
                    collection.as_str(),
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &("org.freedesktop.Secret.Collection", "Locked"),
                )
                .await;
            if let Ok(reply) = result {
                let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
                if !bool::try_from(value)? {
                    return anyhow::Ok(());
                }
            }
            anyhow::ensure!(reopened.try_wait()?.is_none(), "reopened keyring exited");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("keyring did not unlock")??;
    success(
        probe
            .run(&["account", "logout", "--all", "--forget", "--json"], false)
            .await?,
    );
    let output = probe
        .run(&["account", "whoami", "--offline", "--json"], false)
        .await?;
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["accounts"], json!([]));
    assert_eq!(result["unavailable_accounts"], json!([]));
    Ok(())
}
