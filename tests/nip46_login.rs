use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use ngit::{
    cli_interactor::Printer,
    login::{fresh::listen_for_remote_signer, nbunksec},
};
use nostr::{
    event::{EventBuilder, Kind},
    filter::Filter,
    key::{Keys, PublicKey},
    nips::nip46::{
        NostrConnectEventBuilder, NostrConnectMessage, NostrConnectRequest, NostrConnectResponse,
        NostrConnectUri, ResponseResult,
    },
    prelude::{ToBech32, event::FinalizeEvent},
    types::RelayUrl,
};
use nostr_connect::signer::{
    NostrConnectKeys, NostrConnectRemoteSigner, NostrConnectSignerActions,
};
use nostr_sdk::{
    client::{Client, ClientNotification},
    local_relay::LocalRelayBuilder,
};
use tempfile::NamedTempFile;
use test_harness::{Harness, repo::Repo};
use tokio::{sync::Mutex, task::JoinHandle};

/// Upper bound on waiting for an observable condition on the local relay.
const READY_DEADLINE: Duration = Duration::from_secs(10);
/// Resend/poll interval within a `READY_DEADLINE`-bounded loop.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

struct ApproveAll;

impl NostrConnectSignerActions for ApproveAll {
    fn approve(&self, _public_key: &PublicKey, _request: &NostrConnectRequest) -> bool {
        true
    }
}

#[derive(Default)]
struct RequestCounts {
    get_public_key: AtomicUsize,
    sign_event: AtomicUsize,
}

struct CountRequests(Arc<RequestCounts>);

impl NostrConnectSignerActions for CountRequests {
    fn approve(&self, _public_key: &PublicKey, request: &NostrConnectRequest) -> bool {
        match request {
            NostrConnectRequest::GetPublicKey => {
                self.0.get_public_key.fetch_add(1, Ordering::SeqCst);
            }
            NostrConnectRequest::SignEvent(_) => {
                self.0.sign_event.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        true
    }
}

async fn relay_client(relay_url: &RelayUrl) -> Client {
    let client = Client::default();
    client
        .add_relay(relay_url.clone())
        .await
        .expect("relay should be added to helper client");
    client.connect().await;
    client
}

async fn send_nip46_message(client: &Client, from: &Keys, to: PublicKey, msg: NostrConnectMessage) {
    let event = NostrConnectEventBuilder::new(to, msg)
        .finalize(from)
        .expect("NIP-46 event should finalize");
    client
        .send_event(&event)
        .await
        .expect("NIP-46 event should publish");
}

/// Wait until the remote signer's relay subscription is provably live.
///
/// NIP-46 events are ephemeral, so a request published before the signer
/// subscribes is dropped by the relay and never redelivered. The spawned
/// `serve` task subscribes asynchronously, which races the client's single
/// `connect` request. Pinging from a throwaway key until any response comes
/// back proves the subscription is registered, making the subsequent
/// single-shot handshake deterministic.
async fn wait_until_signer_ready(relay_url: &RelayUrl, signer_pubkey: PublicKey) {
    let probe_keys = Keys::generate();
    let client = relay_client(relay_url).await;
    client
        .subscribe(
            Filter::new()
                .pubkey(probe_keys.public_key())
                .kind(Kind::NostrConnect)
                .limit(0),
        )
        .await
        .expect("probe subscription should register");
    let mut notifications = client.notifications();
    let deadline = tokio::time::Instant::now() + READY_DEADLINE;
    loop {
        send_nip46_message(
            &client,
            &probe_keys,
            signer_pubkey,
            NostrConnectMessage::request(&NostrConnectRequest::Ping),
        )
        .await;
        let pong = tokio::time::timeout(PROBE_INTERVAL, async {
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    if event.kind == Kind::NostrConnect && event.pubkey == signer_pubkey {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        if matches!(pong, Ok(true)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "remote signer should answer a ping before the readiness deadline"
        );
    }
}

/// Resend the signer's `connect` response until the task is aborted.
///
/// In the nostrconnect:// flow the remote signer emits a single ephemeral
/// `connect` response when `serve` starts, which is lost if the client has
/// not subscribed yet. Resending until pairing completes closes that race;
/// the client ignores duplicates once paired.
fn spawn_connect_response_resender(
    relay_url: RelayUrl,
    signer_keys: Keys,
    app_pubkey: PublicKey,
    secret: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = relay_client(&relay_url).await;
        loop {
            let response =
                NostrConnectResponse::with_result(ResponseResult::ConnectSecret(secret.clone()));
            send_nip46_message(
                &client,
                &signer_keys,
                app_pubkey,
                NostrConnectMessage::response("readiness-resend", response),
            )
            .await;
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    })
}

async fn run_nbunksec_login(
    repo: &Repo,
    encoded: &str,
    credentials: &Path,
    alias: Option<&str>,
) -> Result<std::process::Output> {
    let mut args = vec![
        "account",
        "login",
        "--local",
        "--offline",
        "--nbunksec",
        encoded,
    ];
    if let Some(alias) = alias {
        args.extend(["--alias", alias]);
    }
    let mut command = repo.ngit(args);
    command
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials);
    command.kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .context("remote-signer login did not finish before its deadline")?
        .context("failed to spawn remote-signer login")
}

async fn export_stored_signer(
    repo: &Repo,
    credentials: &Path,
) -> Result<nbunksec::BunkerConnection> {
    let output = repo
        .ngit(["account", "export-keys", "--json"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials)
        .output()
        .await?;
    assert!(
        output.status.success(),
        "stored signer export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    nbunksec::decode(
        document["nbunksec"]
            .as_str()
            .context("stored remote signer export omitted nbunksec")?,
    )
}

async fn assert_login_persists_remote_signer_pubkey(connect_from_client_uri: bool) {
    let relay = LocalRelayBuilder::default().build();
    let relay_url = relay.url().await;
    relay.run().await.expect("local relay should start");

    let app_keys = Keys::generate();
    let signer_keys = Keys::generate();
    let user_keys = Keys::generate();
    assert_ne!(signer_keys.public_key(), user_keys.public_key());

    let (login_uri, remote_signer) = if connect_from_client_uri {
        let uri = NostrConnectUri::client(app_keys.public_key(), [relay_url.clone()], "ngit test");
        let signer = NostrConnectRemoteSigner::from_uri(
            uri.clone(),
            NostrConnectKeys {
                signer: signer_keys.clone(),
                user: user_keys.clone(),
            },
            None,
        )
        .expect("remote signer should accept client URI");
        (uri, signer)
    } else {
        let signer = NostrConnectRemoteSigner::new(
            NostrConnectKeys {
                signer: signer_keys.clone(),
                user: user_keys.clone(),
            },
            [relay_url.clone()],
            Some("test-secret".to_string()),
            None,
        )
        .expect("remote signer should be created");
        (signer.bunker_uri(), signer)
    };
    let signer_task = tokio::spawn(async move { remote_signer.serve(ApproveAll).await });
    wait_until_signer_ready(&relay_url, signer_keys.public_key()).await;
    let resender = if let NostrConnectUri::Client { secret, .. } = &login_uri {
        Some(spawn_connect_response_resender(
            relay_url.clone(),
            signer_keys.clone(),
            app_keys.public_key(),
            secret.clone(),
        ))
    } else {
        None
    };

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        listen_for_remote_signer(
            &app_keys,
            &login_uri,
            Arc::new(Mutex::new(Printer::default())),
        ),
    )
    .await
    .expect("NIP-46 login should finish before its deadline")
    .expect("NIP-46 login should succeed");

    if let Some(resender) = resender {
        resender.abort();
    }
    signer_task.abort();

    let (_, returned_user_pubkey, saved_bunker_uri) = result;
    assert_eq!(returned_user_pubkey, user_keys.public_key());
    match saved_bunker_uri {
        NostrConnectUri::Bunker {
            remote_signer_public_key,
            ..
        } => {
            assert_eq!(remote_signer_public_key, signer_keys.public_key());
            assert_ne!(remote_signer_public_key, user_keys.public_key());
        }
        NostrConnectUri::Client { .. } => panic!("login should persist a bunker URI"),
    }
}

#[tokio::test]
async fn nostrconnect_login_persists_remote_signer_pubkey() {
    assert_login_persists_remote_signer_pubkey(true).await;
}

#[tokio::test]
async fn bunker_login_persists_remote_signer_pubkey() {
    assert_login_persists_remote_signer_pubkey(false).await;
}

#[tokio::test]
async fn same_npub_remote_signers_require_and_respect_distinct_aliases() -> Result<()> {
    let relay = LocalRelayBuilder::default().build();
    let relay_url = relay.url().await;
    relay.run().await?;

    let user_keys = Keys::generate();
    let first_remote_keys = Keys::generate();
    let second_remote_keys = Keys::generate();
    let first_client_keys = Keys::generate();
    let second_client_keys = Keys::generate();
    let first_remote = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: first_remote_keys.clone(),
            user: user_keys.clone(),
        },
        [relay_url.clone()],
        None,
        None,
    )?;
    let second_remote = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: second_remote_keys.clone(),
            user: user_keys.clone(),
        },
        [relay_url.clone()],
        None,
        None,
    )?;
    let first_uri = first_remote.bunker_uri().to_string();
    let second_uri = second_remote.bunker_uri().to_string();
    let first_task = tokio::spawn(async move { first_remote.serve(ApproveAll).await });
    let second_task = tokio::spawn(async move { second_remote.serve(ApproveAll).await });
    wait_until_signer_ready(&relay_url, first_remote_keys.public_key()).await;
    wait_until_signer_ready(&relay_url, second_remote_keys.public_key()).await;

    let first_encoded =
        nbunksec::encode(&first_uri, &first_client_keys.secret_key().to_secret_hex())?;
    let second_encoded = nbunksec::encode(
        &second_uri,
        &second_client_keys.secret_key().to_secret_hex(),
    )?;
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let first_repo = harness.fresh_repo()?;
    let rejected_repo = harness.fresh_repo()?;
    let aliased_repo = harness.fresh_repo()?;
    let credentials = NamedTempFile::new()?;

    let first = run_nbunksec_login(&first_repo, &first_encoded, credentials.path(), None).await?;
    assert!(
        first.status.success(),
        "initial remote-signer login failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let npub = user_keys.public_key().to_bech32()?;
    assert_eq!(
        first_repo.config("nostr.signer").await?.as_deref(),
        Some(npub.as_str()),
        "credential-backed bunker login should persist its resolved npub"
    );
    let before_rejected_login = std::fs::read(credentials.path())?;

    let rejected =
        run_nbunksec_login(&rejected_repo, &second_encoded, credentials.path(), None).await?;
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains("different signer credential") && stderr.contains("--alias <name>"),
        "same-npub replacement should explain the alias escape hatch: {stderr}"
    );
    assert_eq!(
        std::fs::read(credentials.path())?,
        before_rejected_login,
        "a rejected same-npub login must not alter the original credential"
    );

    let aliased = run_nbunksec_login(
        &aliased_repo,
        &second_encoded,
        credentials.path(),
        Some("dedicated"),
    )
    .await?;
    assert!(
        aliased.status.success(),
        "aliased same-npub login failed: {}",
        String::from_utf8_lossy(&aliased.stderr)
    );
    assert_eq!(
        aliased_repo.config("nostr.signer").await?.as_deref(),
        Some("dedicated")
    );

    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    assert!(stored.get(format!("nostr/signer:{npub}")).is_some());
    assert!(stored.get("nostr/signer-alias:dedicated").is_some());
    assert_eq!(stored["nostr/alias:dedicated"], npub);
    assert_eq!(
        stored["nostr/alias-credential:dedicated"],
        "signer-alias:dedicated"
    );

    first_task.abort();
    second_task.abort();

    let first_export = export_stored_signer(&first_repo, credentials.path()).await?;
    let aliased_export = export_stored_signer(&aliased_repo, credentials.path()).await?;
    assert_eq!(first_export.bunker_uri, first_uri);
    assert_eq!(
        first_export.client_key,
        first_client_keys.secret_key().to_secret_hex()
    );
    assert_eq!(aliased_export.bunker_uri, second_uri);
    assert_eq!(
        aliased_export.client_key,
        second_client_keys.secret_key().to_secret_hex()
    );

    let sole_credentials = NamedTempFile::new()?;
    let mut sole_stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    sole_stored
        .as_object_mut()
        .context("credential store should be a JSON object")?
        .remove(&format!("nostr/signer:{npub}"));
    std::fs::write(sole_credentials.path(), serde_json::to_vec(&sole_stored)?)?;
    let npub_repo = harness.fresh_repo()?;
    let npub_login = npub_repo
        .ngit([
            "--signer",
            &npub,
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", sole_credentials.path())
        .output()
        .await?;
    assert!(
        npub_login.status.success(),
        "npub did not resolve its sole alias-specific signer: {}",
        String::from_utf8_lossy(&npub_login.stderr)
    );
    let npub_export = export_stored_signer(&npub_repo, sole_credentials.path()).await?;
    assert_eq!(npub_export.bunker_uri, second_uri);
    assert_eq!(
        npub_export.client_key,
        second_client_keys.secret_key().to_secret_hex()
    );
    let alias_export = export_stored_signer(&aliased_repo, sole_credentials.path()).await?;
    assert_eq!(alias_export.bunker_uri, second_uri);
    assert_eq!(
        alias_export.client_key,
        second_client_keys.secret_key().to_secret_hex()
    );

    let ambiguous_credentials = NamedTempFile::new()?;
    let mut ambiguous_stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    let values = ambiguous_stored
        .as_object_mut()
        .context("credential store should be a JSON object")?;
    let default = values
        .remove(&format!("nostr/signer:{npub}"))
        .context("default signer record should exist")?;
    values.insert("nostr/signer-alias:other".to_string(), default);
    values.insert(
        "nostr/alias:other".to_string(),
        serde_json::Value::String(npub.clone()),
    );
    values.insert(
        "nostr/alias-credential:other".to_string(),
        serde_json::Value::String("signer-alias:other".to_string()),
    );
    std::fs::write(
        ambiguous_credentials.path(),
        serde_json::to_vec(&ambiguous_stored)?,
    )?;
    let ambiguous_repo = harness.fresh_repo()?;
    let ambiguous_login = ambiguous_repo
        .ngit([
            "--signer",
            &npub,
            "account",
            "login",
            "--local",
            "--offline",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", ambiguous_credentials.path())
        .output()
        .await?;
    assert!(!ambiguous_login.status.success());
    let stderr = String::from_utf8_lossy(&ambiguous_login.stderr);
    assert!(
        stderr.contains("multiple remote-signer connections")
            && stderr.contains("--signer <alias>"),
        "ambiguous bare-npub selection should require an alias: {stderr}"
    );

    let migration_credentials = NamedTempFile::new()?;
    let mut migration_stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    migration_stored["nostr/alias:dedicated"] =
        serde_json::Value::String(serde_json::to_string(&serde_json::json!({
            "version": 1,
            "type": "alias",
            "npub": npub,
            "credential": "signer-alias:dedicated",
        }))?);
    std::fs::write(
        migration_credentials.path(),
        serde_json::to_vec(&migration_stored)?,
    )?;
    let migration_repo = harness.fresh_repo()?;
    let migration = migration_repo
        .ngit([
            "account",
            "login",
            "--local",
            "--offline",
            "--alias",
            "dedicated",
        ])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", migration_credentials.path())
        .output()
        .await?;
    assert!(
        migration.status.success(),
        "intermediate alias migration failed: {}",
        String::from_utf8_lossy(&migration.stderr)
    );
    let migrated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(migration_credentials.path())?)?;
    assert_eq!(migrated["nostr/alias:dedicated"], npub);

    let logout = aliased_repo
        .ngit(["account", "logout", "--forget"])
        .env("NGIT_SECRET_STORAGE", "file")
        .env("NGIT_KEYRING_FILE", credentials.path())
        .output()
        .await?;
    assert!(
        logout.status.success(),
        "aliased signer logout failed: {}",
        String::from_utf8_lossy(&logout.stderr)
    );
    let stored_after_logout: serde_json::Value =
        serde_json::from_slice(&std::fs::read(credentials.path())?)?;
    assert!(
        stored_after_logout
            .get(format!("nostr/signer:{npub}"))
            .is_some()
    );
    assert!(
        stored_after_logout
            .get("nostr/signer-alias:dedicated")
            .is_none(),
        "forgetting an aliased session should remove only its bound credential"
    );
    assert!(
        stored_after_logout
            .get("nostr/alias-credential:dedicated")
            .is_none(),
        "forgetting an aliased session should remove its public binding"
    );
    Ok(())
}

#[tokio::test]
async fn paired_bunker_reuses_discovered_pubkey_for_real_signatures() {
    let relay = LocalRelayBuilder::default().build();
    let relay_url = relay.url().await;
    relay.run().await.expect("local relay should start");

    let client_keys = Keys::generate();
    let signer_keys = Keys::generate();
    let user_keys = Keys::generate();
    let remote_signer = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: signer_keys.clone(),
            user: user_keys.clone(),
        },
        [relay_url.clone()],
        Some("one-time-secret".to_string()),
        None,
    )
    .expect("remote signer should be created");
    let bunker_uri = remote_signer.bunker_uri();
    let request_counts = Arc::new(RequestCounts::default());
    let signer_actions = CountRequests(Arc::clone(&request_counts));
    let signer_task = tokio::spawn(async move { remote_signer.serve(signer_actions).await });
    wait_until_signer_ready(&relay_url, signer_keys.public_key()).await;

    let (signer, connected_user, _) = tokio::time::timeout(
        Duration::from_secs(30),
        listen_for_remote_signer(
            &client_keys,
            &bunker_uri,
            Arc::new(Mutex::new(Printer::default())),
        ),
    )
    .await
    .expect("initial pairing should finish before its deadline")
    .expect("initial pairing should succeed");
    assert_eq!(connected_user, user_keys.public_key());

    let event = tokio::time::timeout(
        Duration::from_secs(30),
        signer.sign_event_builder(EventBuilder::new(Kind::TextNote, "real ngit event")),
    )
    .await
    .expect("signing should finish before its deadline")
    .expect("signing should succeed");
    signer_task.abort();

    event.verify().expect("signature should verify");
    assert_eq!(event.pubkey, user_keys.public_key());
    assert_eq!(request_counts.get_public_key.load(Ordering::SeqCst), 1);
    assert_eq!(request_counts.sign_event.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn nbunksec_without_user_pubkey_fetches_identity_before_signing() {
    let relay = LocalRelayBuilder::default().build();
    let relay_url = relay.url().await;
    relay.run().await.expect("local relay should start");

    let client_keys = Keys::generate();
    let signer_keys = Keys::generate();
    let user_keys = Keys::generate();
    let remote_signer = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: signer_keys.clone(),
            user: user_keys.clone(),
        },
        [relay_url.clone()],
        None,
        None,
    )
    .expect("remote signer should be created");
    let bunker_uri = remote_signer.bunker_uri().to_string();
    let encoded =
        ngit::login::nbunksec::encode(&bunker_uri, &client_keys.secret_key().to_secret_hex())
            .expect("established connection should encode");
    let connection = ngit::login::nbunksec::decode(&encoded).expect("nbunksec should decode");
    let request_counts = Arc::new(RequestCounts::default());
    let signer_actions = CountRequests(Arc::clone(&request_counts));
    let signer_task = tokio::spawn(async move { remote_signer.serve(signer_actions).await });
    wait_until_signer_ready(&relay_url, signer_keys.public_key()).await;

    let connect = nostr_connect::client::NostrConnect::new(
        NostrConnectUri::parse(connection.bunker_uri).expect("decoded bunker URI should parse"),
        Keys::parse(&connection.client_key).expect("decoded client key should parse"),
        Duration::from_secs(10),
        None,
    )
    .expect("decoded connection should construct");
    let signer = ngit::signer::NgitSigner::Connect(Arc::new(connect));
    let public_key = tokio::time::timeout(Duration::from_secs(10), signer.get_public_key())
        .await
        .expect("identity request should finish before its deadline")
        .expect("identity request should succeed");
    let event = tokio::time::timeout(
        Duration::from_secs(10),
        signer.sign_event_builder(EventBuilder::new(Kind::TextNote, "real ngit event")),
    )
    .await
    .expect("signing should finish before its deadline")
    .expect("signing should succeed");
    signer_task.abort();

    assert_eq!(public_key, user_keys.public_key());
    event.verify().expect("signature should verify");
    assert_eq!(event.pubkey, user_keys.public_key());
    assert_eq!(request_counts.get_public_key.load(Ordering::SeqCst), 1);
    assert_eq!(request_counts.sign_event.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sanitized_bunker_uri_reconnects_with_stored_client_key() {
    let relay = LocalRelayBuilder::default().build();
    let relay_url = relay.url().await;
    relay.run().await.expect("local relay should start");

    let client_keys = Keys::generate();
    let signer_keys = Keys::generate();
    let user_keys = Keys::generate();
    let remote_signer = NostrConnectRemoteSigner::new(
        NostrConnectKeys {
            signer: signer_keys.clone(),
            user: user_keys.clone(),
        },
        [relay_url.clone()],
        None,
        None,
    )
    .expect("remote signer should be created");
    let sanitized_uri = remote_signer.bunker_uri();
    let request_counts = Arc::new(RequestCounts::default());
    let signer_actions = CountRequests(Arc::clone(&request_counts));
    let signer_task = tokio::spawn(async move { remote_signer.serve(signer_actions).await });
    wait_until_signer_ready(&relay_url, signer_keys.public_key()).await;

    let connect = nostr_connect::client::NostrConnect::new(
        sanitized_uri,
        client_keys,
        Duration::from_secs(10),
        None,
    )
    .expect("stored bunker connection should be valid");
    connect
        .non_secure_set_user_public_key(user_keys.public_key())
        .expect("stored user public key should seed the connection cache");
    let signer = ngit::signer::NgitSigner::Connect(Arc::new(connect));
    let event = tokio::time::timeout(
        Duration::from_secs(10),
        signer.sign_event_builder(EventBuilder::new(Kind::TextNote, "real ngit event")),
    )
    .await
    .expect("reconnection should finish before its deadline")
    .expect("reconnection should sign successfully");
    signer_task.abort();

    event.verify().expect("signature should verify");
    assert_eq!(event.pubkey, user_keys.public_key());
    assert_eq!(request_counts.get_public_key.load(Ordering::SeqCst), 0);
    assert_eq!(request_counts.sign_event.load(Ordering::SeqCst), 1);
}
