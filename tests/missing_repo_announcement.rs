use anyhow::Result;
use nostr_sdk::prelude::*;
use test_harness::Harness;

#[tokio::test]
async fn push_missing_announcement_identifies_remote_and_searched_relays() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    repo.git_ok(
        ["commit", "--allow-empty", "-m", "initial"],
        "create initial commit",
    )
    .await?;
    let author = Keys::generate().public_key();
    let url = format!("nostr://{}/missing-repository", author.to_bech32()?);
    repo.git_ok(["remote", "add", "origin", &url], "add missing repository")
        .await?;
    let output = repo
        .nostr_push_expecting_failure(["origin", "HEAD"])
        .await?;
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("30617:"), "{error}");
    assert!(
        error.contains("couldn't find the repository specified by Git remote \"origin\""),
        "{error}"
    );
    assert!(error.contains(harness.relay("default").url()), "{error}");
    assert!(error.contains(&url), "{error}");
    assert!(
        error.contains("Nostr relays that responded but didn't return this repository (1):"),
        "{error}"
    );
    assert!(error.contains("git remote -v"), "{error}");
    assert!(!error.contains("couldn't be checked"), "{error}");
    assert!(!error.contains("retrying may help"), "{error}");
    assert!(!error.contains("ngit init"), "{error}");
    assert!(!error.contains("Nip19Coordinates"), "{error}");

    let output = repo
        .nostr_push_expecting_failure(["--quiet", &url, "HEAD"])
        .await?;
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("specified by the supplied URL"), "{error}");
    assert!(error.contains(&url), "{error}");
    assert!(!error.contains("Git remote"), "{error}");
    assert!(!error.contains("30617:"), "{error}");
    Ok(())
}

#[tokio::test]
async fn missing_announcement_identifies_config_and_explicit_override() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    let repo = harness.fresh_repo()?;
    let address = Nip19Coordinate {
        coordinate: Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: Keys::generate().public_key(),
            identifier: "missing-configured-repository".into(),
        },
        relays: vec![],
    }
    .to_bech32()?;
    repo.git_ok(["config", "nostr.repo", &address], "set repository address")
        .await?;
    let output = repo.ngit(["--quiet", "issue", "list"]).output().await?;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("specified by Git setting `nostr.repo`"),
        "{error}"
    );
    assert!(error.contains(&address), "{error}");
    assert!(
        error.contains("git config --show-origin --get nostr.repo"),
        "{error}"
    );
    assert!(error.contains(harness.relay("default").url()), "{error}");
    assert!(!error.contains("30617:"), "{error}");

    let url = format!(
        "nostr://{}/missing-override",
        Keys::generate().public_key().to_bech32()?
    );
    let output = repo
        .ngit(["--quiet", "--repo", &url, "issue", "list"])
        .output()
        .await?;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("specified by `--repo`"), "{error}");
    assert!(error.contains(&url), "{error}");
    assert!(!error.contains(&address), "{error}");
    assert!(!error.contains("git remote -v"), "{error}");
    Ok(())
}

#[tokio::test]
async fn missing_announcement_groups_completed_and_incomplete_relay_searches() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    // Keep the port bound throughout: TCP connections can reach this address,
    // but no websocket handshake or search completes. No port reuse race.
    let unresponsive = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let unresponsive_url = format!("ws://{}", unresponsive.local_addr()?);
    let responding_url = harness.relay("default").url();
    for include_responding_relay in [true, false] {
        let mut repo = harness.fresh_repo()?;
        let relays = if include_responding_relay {
            format!("{responding_url};{unresponsive_url}")
        } else {
            unresponsive_url.clone()
        };
        repo.set_env("NGIT_RELAY_DEFAULT_SET", relays);
        repo.git_ok(
            ["commit", "--allow-empty", "-m", "initial"],
            "create commit",
        )
        .await?;
        let url = format!(
            "nostr://{}/missing",
            Keys::generate().public_key().to_bech32()?
        );
        repo.git_ok(["remote", "add", "origin", &url], "add remote")
            .await?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            repo.nostr_push_expecting_failure(["--quiet", "origin", "HEAD"]),
        )
        .await??;
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("Nostr relays that couldn't be checked (1):"),
            "{error}"
        );
        assert!(
            error.contains(&format!(
                "{unresponsive_url} — timed out waiting for a response"
            )),
            "{error}"
        );
        assert!(
            error.contains("Some Nostr relays couldn't be checked, so retrying may help."),
            "{error}"
        );
        let responded = "Nostr relays that responded but didn't return this repository (1):";
        if include_responding_relay {
            assert!(
                error.contains(&format!("{responded}\n  {responding_url}")),
                "{error}"
            );
            assert!(error.contains("couldn't find the repository"), "{error}");
        } else {
            assert!(!error.contains(responded), "{error}");
            assert!(error.contains("couldn't check the repository"), "{error}");
            assert!(!error.contains("couldn't find the repository"), "{error}");
        }
    }
    Ok(())
}
