use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use ngit::{cli_interactor::Printer, login::fresh::listen_for_remote_signer};
use nostr::{
    event::{EventBuilder, Kind},
    filter::Filter,
    key::{Keys, PublicKey},
    nips::nip46::{
        NostrConnectEventBuilder, NostrConnectMessage, NostrConnectRequest, NostrConnectResponse,
        NostrConnectUri, ResponseResult,
    },
    prelude::event::FinalizeEvent,
    types::RelayUrl,
};
use nostr_connect::signer::{
    NostrConnectKeys, NostrConnectRemoteSigner, NostrConnectSignerActions,
};
use nostr_sdk::{
    client::{Client, ClientNotification},
    local_relay::LocalRelayBuilder,
};
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
