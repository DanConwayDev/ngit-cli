//! Local account selection and logout must not silently act on a global
//! account.
use anyhow::{Context, Result};
use nostr::prelude::{Filter, Keys, Kind, ToBech32};
use test_harness::Harness;

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn logout_respects_explicit_and_implicit_scopes() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    for scope in [None, Some("--local"), Some("--global")] {
        let repo = harness.fresh_repo()?;
        let global_config = repo.dir().join("global.gitconfig");
        let global_nsec = Keys::generate().secret_key().to_bech32()?;
        let local_nsec = Keys::generate().secret_key().to_bech32()?;
        for (local, nsec) in [(false, &global_nsec), (true, &local_nsec)] {
            let mut args = vec!["account", "login", "--offline", "--nsec", nsec];
            if local {
                args.push("--local");
            } else {
                args.extend(["--alias", "global-account"]);
            }
            let output = repo
                .ngit(args)
                .env_remove("NGITTEST")
                .env("GIT_CONFIG_GLOBAL", &global_config)
                .output()
                .await?;
            assert_success(&output);
        }
        let global_before = std::fs::read(&global_config)?;
        let mut args = vec!["account", "logout"];
        args.extend(scope);
        let output = repo
            .ngit(&args)
            .env_remove("NGITTEST")
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .output()
            .await?;
        assert_success(&output);
        let global = git2::Config::open(&global_config)?;
        if scope == Some("--global") {
            assert!(global.get_string("nostr.nsec").is_err());
            assert_eq!(
                repo.config("nostr.nsec").await?.as_deref(),
                Some(local_nsec.as_str())
            );
        } else {
            assert_eq!(repo.config("nostr.nsec").await?, None);
            assert_eq!(std::fs::read(&global_config)?, global_before);
            // Repeating explicit local logout is a no-op, not a global logout.
            let output = repo
                .ngit(["account", "logout", "--local"])
                .env_remove("NGITTEST")
                .env("GIT_CONFIG_GLOBAL", &global_config)
                .output()
                .await?;
            assert_success(&output);
            assert_eq!(std::fs::read(&global_config)?, global_before);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_uses_local_login_and_rejects_incomplete_local_credentials() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_vanilla_git_server("host")
    .build()
    .await?;
    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let global_config = repo.dir().join("global.gitconfig");
    let global_keys = Keys::generate();
    let mut config = git2::Config::open(&global_config)?;
    config.set_str("nostr.nsec", &global_keys.secret_key().to_bech32()?)?;
    config.set_str("nostr.npub", &global_keys.public_key().to_bech32()?)?;
    config.set_str("nostr.signer", "global-account")?;
    config.set_str(
        "nostr.signer-alias.global-account",
        &global_keys.public_key().to_bech32()?,
    )?;
    // An incomplete legacy bunker login used to be skipped in favour of global
    // keys.
    repo.git_ok(
        ["config", "--local", "--unset", "nostr.nsec"],
        "remove local key",
    )
    .await?;
    repo.git_ok(
        [
            "config",
            "--local",
            "nostr.bunker-uri",
            &format!("bunker://{}", state.keys.public_key()),
        ],
        "configure incomplete bunker",
    )
    .await?;
    let clone_url = harness.vanilla_git_server("host").url().to_string();
    let relay_url = harness.relay("default").url().to_string();
    let args = [
        "init",
        "--name",
        "Scoped",
        "--additional-clone",
        &clone_url,
        "--additional-relay",
        &relay_url,
    ];
    let output = repo
        .ngit(args)
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bunker-app-key"));
    assert!(
        harness
            .relay("default")
            .events(Filter::new().kind(Kind::GitRepoAnnouncement))
            .await?
            .is_empty()
    );

    let output = repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--nsec",
            &state.keys.secret_key().to_bech32()?,
        ])
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await?;
    assert_success(&output);
    let output = repo
        .ngit(args)
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await?;
    assert_success(&output);
    let announcements = harness
        .relay("default")
        .events(Filter::new().kind(Kind::GitRepoAnnouncement))
        .await?;
    let event = announcements
        .first()
        .context("init must publish an announcement")?;
    assert_eq!(announcements.len(), 1);
    assert_eq!(event.pubkey, state.keys.public_key());
    assert_eq!(
        config.get_string("nostr.nsec")?,
        global_keys.secret_key().to_bech32()?
    );
    Ok(())
}

#[tokio::test]
async fn local_logout_outside_a_repository_preserves_global_login() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let outside = tempfile::tempdir()?;
    let global_config = outside.path().join("gitconfig");
    git2::Config::open(&global_config)?
        .set_str("nostr.nsec", &Keys::generate().secret_key().to_bech32()?)?;
    let before = std::fs::read(&global_config)?;
    let output = repo
        .ngit(["account", "logout", "--local"])
        .current_dir(outside.path())
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await?;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&global_config)?, before);
    let output = repo
        .ngit(["account", "logout"])
        .current_dir(outside.path())
        .env_remove("NGITTEST")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .await?;
    assert_success(&output);
    assert!(
        git2::Config::open(&global_config)?
            .get_string("nostr.nsec")
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn profile_and_legacy_alias_select_the_same_stored_account() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    for flag in ["--profile", "--alias"] {
        let output = repo
            .ngit([
                "account",
                "login",
                "--local",
                "--offline",
                "--nsec",
                &keys.secret_key().to_bech32()?,
                flag,
                "work",
            ])
            .output()
            .await?;
        assert_success(&output);
        assert_eq!(repo.config("nostr.signer").await?.as_deref(), Some("work"));
        assert_eq!(
            repo.config("nostr.npub").await?.as_deref(),
            Some(npub.as_str())
        );
    }
    Ok(())
}
