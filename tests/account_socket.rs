//! Socket fallback must not replace a selected account or its signing backend.
#![cfg(unix)]

use std::{
    path::Path,
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use nostr::prelude::*;
use serde_json::{Value, json};
use test_harness::{Harness, PublishRepoOpts, Repo};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

struct Socket {
    state: Arc<SocketState>,
    task: tokio::task::JoinHandle<()>,
}

struct SocketState {
    keys: Keys,
    other: Keys,
    locked: AtomicBool,
    signed: AtomicUsize,
    connections: AtomicUsize,
    lock_after: AtomicUsize,
    stalled: AtomicBool,
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Socket {
    async fn start(path: &Path) -> Result<Self> {
        let listener = UnixListener::bind(path)?;
        let state = Arc::new(SocketState {
            keys: Keys::generate(),
            other: Keys::generate(),
            locked: AtomicBool::new(false),
            signed: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            lock_after: AtomicUsize::new(usize::MAX),
            stalled: AtomicBool::new(false),
        });
        let server = state.clone();
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let state = server.clone();
                let connection = state.connections.fetch_add(1, Ordering::SeqCst) + 1;
                clients.spawn(async move {
                    if state.stalled.load(Ordering::SeqCst) {
                        std::future::pending::<()>().await;
                    }
                    let (read, mut write) = stream.into_split();
                    write
                        .write_all(b"{\"supported_methods\":[]}\n")
                        .await
                        .unwrap();
                    let mut lines = BufReader::new(read).lines();
                    while let Some(line) = lines.next_line().await.unwrap() {
                        let message: Value = serde_json::from_str(&line).unwrap();
                        let Some(method) = message["method"].as_str() else {
                            continue;
                        };
                        let params = &message["params"];
                        let requested = if method == "get_public_key" { params[0].as_str() } else { params[1].as_str() };
                        let chosen = match requested {
                            None => Some(&state.keys),
                            Some(npub) if npub == state.keys.public_key().to_hex() => Some(&state.keys),
                            Some(npub) if npub == state.other.public_key().to_hex() => Some(&state.other),
                            _ => None,
                        };
                        let response = if state.locked.load(Ordering::SeqCst) || connection > state.lock_after.load(Ordering::SeqCst) {
                            json!({"id": message["id"], "error": {"code": 10, "message": "locked"}})
                        } else if let Some(keys) = chosen {
                            let result = match method {
                                "get_public_key" => json!(keys.public_key().to_hex()),
                                "sign_event" => {
                                    assert_eq!(params[1], keys.public_key().to_hex());
                                    let unsigned = UnsignedEvent::from_json(params[0].to_string()).unwrap();
                                    state.signed.fetch_add(1, Ordering::SeqCst);
                                    serde_json::to_value(keys.sign_event(unsigned).unwrap()).unwrap()
                                }
                                _ => panic!("unexpected socket method: {method}"),
                            };
                            json!({"id": message["id"], "result": result})
                        } else {
                            json!({"id": message["id"], "error": {"code": 4, "message": "account not found"}})
                        };
                        if write
                            .write_all(format!("{response}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Ok(Self { state, task })
    }
}

async fn fixture() -> Result<(Harness, Repo, Socket)> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let (mut repo, _) = harness.publish_repo(PublishRepoOpts::default()).await?;
    for key in [
        "nostr.nsec",
        "nostr.npub",
        "nostr.signer",
        "nostr.bunker-uri",
        "nostr.bunker-app-key",
    ] {
        repo.git(["config", "--unset-all", key]).output().await?;
    }
    repo.set_env("NGIT_SECRET_STORAGE", "file");
    repo.set_env(
        "NGIT_KEYRING_FILE",
        repo.dir().join("credentials.json").to_str().unwrap(),
    );
    let path = repo.dir().join("signer.sock");
    let socket = Socket::start(&path).await?;
    repo.git_ok(
        ["config", "nostr.signer-socket", path.to_str().unwrap()],
        "select isolated socket",
    )
    .await?;
    Ok((harness, repo, socket))
}

async fn run(repo: &Repo, args: &[&str]) -> Result<Output> {
    tokio::time::timeout(
        Duration::from_secs(30),
        repo.ngit(args).kill_on_drop(true).output(),
    )
    .await
    .context("account command exceeded its deadline")?
    .context("launch ngit")
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn run_ok(repo: &Repo, args: &[&str]) -> Result<Output> {
    let output = run(repo, args).await?;
    success(&output);
    Ok(output)
}

async fn inventory(repo: &Repo) -> Result<Value> {
    let output = run_ok(repo, &["account", "whoami", "--offline", "--json"]).await?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn login(repo: &Repo, keys: &Keys, profile: &str) -> Result<()> {
    run_ok(
        repo,
        &[
            "account",
            "login",
            "--local",
            "--profile",
            profile,
            "--offline",
            "--nsec",
            &keys.secret_key().to_bech32()?,
        ],
    )
    .await?;
    Ok(())
}

async fn socket_profile(repo: &Repo, npub: &str, profile: &str) -> Result<()> {
    run_ok(
        repo,
        &[
            "account",
            "login",
            npub,
            "--socket",
            "--profile",
            profile,
            "--no-activate",
            "--offline",
        ],
    )
    .await?;
    Ok(())
}

fn aliases_for<'a>(state: &'a Value, npub: &str) -> &'a Vec<Value> {
    state["accounts"]
        .as_array()
        .expect("missing accounts")
        .iter()
        .find(|account| account["npub"] == npub)
        .expect("missing account")["aliases"]
        .as_array()
        .expect("missing aliases")
}

async fn issue(repo: &Repo, subject: &str, selector: Option<&str>) -> Result<Output> {
    let mut args = vec![
        "issue",
        "create",
        "--subject",
        subject,
        "--body",
        "socket selection regression",
        "--json",
    ];
    if let Some(selector) = selector {
        args.extend(["--signer", selector]);
    }
    run(repo, &args).await
}

async fn signed_issue(
    harness: &Harness,
    repo: &Repo,
    subject: &str,
    selector: Option<&str>,
) -> Result<PublicKey> {
    success(&issue(repo, subject, selector).await?);
    let events = harness
        .grasp("repo")
        .events(Filter::new().kind(Kind::GitIssue))
        .await?;
    let event = events
        .iter()
        .find(|event| {
            event
                .tags
                .iter()
                .any(|tag| tag.as_slice() == ["subject", subject])
        })
        .context("issue was not published")?;
    event.verify()?;
    Ok(event.pubkey)
}

#[tokio::test]
async fn login_over_socket_and_logout_restore_fallback() -> Result<()> {
    let (harness, repo, socket) = fixture().await?;
    assert_eq!(
        signed_issue(&harness, &repo, "socket-first", None).await?,
        socket.state.keys.public_key()
    );
    let keys = Keys::generate();
    login(&repo, &keys, "work").await?;
    let count = socket.state.signed.load(Ordering::SeqCst);
    assert_eq!(
        signed_issue(&harness, &repo, "profile-selected", None).await?,
        keys.public_key()
    );
    assert_eq!(socket.state.signed.load(Ordering::SeqCst), count);
    assert_eq!(inventory(&repo).await?["socket"]["active"], false);
    let output = run(
        &repo,
        &["account", "logout", "--keep-credentials", "--json"],
    )
    .await?;
    success(&output);
    let state: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(state["socket"]["active"], true);
    assert_eq!(
        state["socket"]["npub"],
        socket.state.keys.public_key().to_bech32()?
    );
    assert_eq!(repo.config("nostr.signer").await?, None);
    assert_eq!(
        signed_issue(&harness, &repo, "socket-again", None).await?,
        socket.state.keys.public_key()
    );
    Ok(())
}

#[tokio::test]
async fn same_identity_profile_keeps_its_own_backend() -> Result<()> {
    let (harness, repo, socket) = fixture().await?;
    login(&repo, &socket.state.keys, "work").await?;
    assert_eq!(
        signed_issue(&harness, &repo, "same-identity", None).await?,
        socket.state.keys.public_key()
    );
    assert_eq!(socket.state.signed.load(Ordering::SeqCst), 0);
    // A broken explicit profile must not fall back, even to the same identity.
    repo.git_ok(
        ["config", "nostr.signer", "missing-profile"],
        "break selection",
    )
    .await?;
    assert!(!issue(&repo, "must-not-sign", None).await?.status.success());
    assert_eq!(socket.state.signed.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn npub_prefers_non_active_socket_account_and_only_falls_back_when_absent() -> Result<()> {
    let (harness, repo, socket) = fixture().await?;
    let npub = socket.state.other.public_key().to_bech32()?;
    // The identity is usable without any ngit credentials.
    assert_eq!(
        signed_issue(&harness, &repo, "socket-only-npub", Some(&npub)).await?,
        socket.state.other.public_key()
    );
    login(&repo, &socket.state.other, "work").await?;
    let count = socket.state.signed.load(Ordering::SeqCst);
    assert_eq!(
        signed_issue(&harness, &repo, "socket-first", Some(&npub)).await?,
        socket.state.other.public_key()
    );
    assert!(socket.state.signed.load(Ordering::SeqCst) > count);
    assert_eq!(repo.config("nostr.signer").await?.as_deref(), Some("work"));
    run_ok(&repo, &["account", "login", &npub, "--local", "--offline"]).await?;
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some(npub.as_str())
    );
    assert_eq!(
        signed_issue(&harness, &repo, "saved-npub", None).await?,
        socket.state.other.public_key()
    );
    assert_eq!(inventory(&repo).await?["socket"]["active"], true);
    socket.state.locked.store(true, Ordering::SeqCst);
    let output = issue(&repo, "locked", Some(&npub)).await?;
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        error["error"]
            .as_str()
            .context("missing error")?
            .contains("locked")
    );
    // A named local-key profile still works while the same socket is locked.
    assert_eq!(
        signed_issue(&harness, &repo, "profile-while-locked", Some("work")).await?,
        socket.state.other.public_key()
    );
    socket.state.locked.store(false, Ordering::SeqCst);
    repo.git_ok(["config", "nostr.signer-socket", "false"], "disable socket")
        .await?;
    assert_eq!(
        signed_issue(&harness, &repo, "disabled-fallback", Some(&npub)).await?,
        socket.state.other.public_key()
    );
    repo.git_ok(
        [
            "config",
            "nostr.signer-socket",
            repo.dir().join("signer.sock").to_str().unwrap(),
        ],
        "restore socket",
    )
    .await?;
    let outside = Keys::generate();
    // Replacing a profile with another identity is intentionally forbidden.
    login(&repo, &outside, "outside").await?;
    assert_eq!(
        signed_issue(
            &harness,
            &repo,
            "not-found-fallback",
            Some(&outside.public_key().to_bech32()?)
        )
        .await?,
        outside.public_key()
    );
    Ok(())
}

#[tokio::test]
async fn socket_profiles_pin_an_account_and_logout_removes_only_the_binding() -> Result<()> {
    let (harness, repo, socket) = fixture().await?;
    let output = run(&repo, &["account", "logout", "--json"]).await?;
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        error["error"]
            .as_str()
            .context("missing error")?
            .contains("nostr.signer-socket false")
    );
    let npub = socket.state.other.public_key().to_bech32()?;
    // socket is now an ordinary profile name. Save without changing fallback.
    socket_profile(&repo, &npub, "socket").await?;
    assert_eq!(repo.config("nostr.signer").await?, None);
    assert_eq!(
        inventory(&repo).await?["socket"]["npub"],
        socket.state.keys.public_key().to_bech32()?
    );
    login(&repo, &socket.state.other, "work").await?;
    run_ok(
        &repo,
        &["account", "login", "socket", "--local", "--offline"],
    )
    .await?;
    assert_eq!(
        repo.config("nostr.signer").await?.as_deref(),
        Some("socket")
    );
    assert_eq!(inventory(&repo).await?["socket"]["npub"], npub);
    assert_eq!(
        signed_issue(&harness, &repo, "pinned-profile", None).await?,
        socket.state.other.public_key()
    );
    repo.git_ok(
        ["config", "nostr.signer-socket", "false"],
        "disable profile backend",
    )
    .await?;
    assert!(!issue(&repo, "strict-profile", None).await?.status.success());
    // Removal needs no live signer and does not touch its keys.
    run_ok(&repo, &["account", "logout", "--json"]).await?;
    assert_eq!(repo.config("nostr.signer").await?, None);
    let state = inventory(&repo).await?;
    assert_eq!(state["socket"]["active"], false);
    assert!(aliases_for(&state, &npub).contains(&json!("work")));
    assert_eq!(
        signed_issue(&harness, &repo, "retained-local-profile", Some("work")).await?,
        socket.state.other.public_key()
    );
    let stored: Value =
        serde_json::from_slice(&std::fs::read(repo.dir().join("credentials.json"))?)?;
    assert!(stored.get("nostr/alias:socket").is_none());
    Ok(())
}

#[tokio::test]
async fn inventory_reuses_socket_results_and_bounds_total_probe_time() -> Result<()> {
    let (_harness, repo, socket) = fixture().await?;
    let npub = socket.state.other.public_key().to_bech32()?;
    login(&repo, &socket.state.other, "work").await?;
    for alias in ["desktop", "backup"] {
        socket_profile(&repo, &npub, alias).await?;
    }
    repo.git_ok(["config", "nostr.signer", &npub], "select socket identity")
        .await?;
    let before = socket.state.connections.load(Ordering::SeqCst);
    socket.state.lock_after.store(before + 1, Ordering::SeqCst);

    // One successful probe must serve the scope, status, aliases and signers,
    // even if a subsequent connection would fail.
    let state = inventory(&repo).await?;
    assert_eq!(socket.state.connections.load(Ordering::SeqCst), before + 1);
    assert_eq!(state["socket"]["active"], true);
    assert_eq!(state["socket"]["npub"], npub);
    for alias in ["desktop", "backup", "work"] {
        assert!(aliases_for(&state, &npub).contains(&json!(alias)));
    }

    // Failures are shared too, but each invocation takes a fresh snapshot.
    let state = inventory(&repo).await?;
    assert_eq!(socket.state.connections.load(Ordering::SeqCst), before + 2);
    assert_eq!(state["socket"]["active"], false);
    assert!(
        state["socket"]["error"]
            .as_str()
            .unwrap()
            .contains("locked")
    );
    assert!(aliases_for(&state, &npub).contains(&json!("work")));

    // A stalled daemon consumes one shared budget, even for distinct accounts.
    // The timeout is a test deadline; no wall-clock sleep drives the fixture.
    repo.git_ok(
        [
            "config",
            "nostr.signer",
            &socket.state.keys.public_key().to_bech32()?,
        ],
        "select another socket identity",
    )
    .await?;
    socket.state.stalled.store(true, Ordering::SeqCst);
    let state = tokio::time::timeout(Duration::from_secs(5), inventory(&repo))
        .await
        .context("inventory exceeded its shared probe budget")??;
    assert_eq!(socket.state.connections.load(Ordering::SeqCst), before + 3);
    assert_eq!(state["socket"]["active"], false);
    assert!(
        state["socket"]["error"]
            .as_str()
            .unwrap()
            .contains("budget exhausted")
    );
    Ok(())
}

#[tokio::test]
async fn unavailable_socket_profile_retains_its_metadata_and_error() -> Result<()> {
    let (_harness, repo, socket) = fixture().await?;
    let npub = socket.state.other.public_key().to_bech32()?;
    socket_profile(&repo, &npub, "desktop").await?;
    assert!(aliases_for(&inventory(&repo).await?, &npub).contains(&json!("desktop")));
    socket.state.locked.store(true, Ordering::SeqCst);
    let state = inventory(&repo).await?;
    let profile = state["unavailable_profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["profile"] == "desktop")
        .expect("saved profile vanished");
    assert_eq!(profile["npub"], npub);
    assert_eq!(profile["type"], "socket");
    assert!(profile["error"].as_str().unwrap().contains("locked"));
    let account = state["unavailable_accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["npub"] == npub)
        .expect("missing unavailable account");
    assert!(account["error"].as_str().unwrap().contains("locked"));

    // An alternative local backend must not hide the failed socket profile.
    login(&repo, &socket.state.other, "work").await?;
    let state = inventory(&repo).await?;
    assert!(aliases_for(&state, &npub).contains(&json!("work")));
    assert!(
        state["unavailable_profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|profile| profile["profile"] == "desktop")
    );
    run_ok(
        &repo,
        &["account", "logout", "--profile", "desktop", "--json"],
    )
    .await?;
    assert!(
        inventory(&repo).await?["unavailable_profiles"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}
