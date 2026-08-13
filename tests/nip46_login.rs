use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ngit::{cli_interactor::Printer, login::fresh::listen_for_remote_signer};
use nostr::{
    event::{EventBuilder, Kind},
    key::{Keys, PublicKey},
    nips::nip46::{NostrConnectRequest, NostrConnectUri},
};
use nostr_connect::signer::{
    NostrConnectKeys, NostrConnectRemoteSigner, NostrConnectSignerActions,
};
use nostr_sdk::local_relay::LocalRelayBuilder;
use tokio::sync::Mutex;

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

async fn assert_login_persists_remote_signer_pubkey(connect_from_client_uri: bool) {
    let relay = LocalRelayBuilder::default().build();
    let relay_url = relay.url().await;
    relay.run().await.expect("local relay should start");

    let app_keys = Keys::generate();
    let signer_keys = Keys::generate();
    let user_keys = Keys::generate();
    assert_ne!(signer_keys.public_key(), user_keys.public_key());

    let (login_uri, remote_signer) = if connect_from_client_uri {
        let uri = NostrConnectUri::client(app_keys.public_key(), [relay_url], "ngit test");
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
            [relay_url],
            Some("test-secret".to_string()),
            None,
        )
        .expect("remote signer should be created");
        (signer.bunker_uri(), signer)
    };
    let signer_task = tokio::spawn(async move { remote_signer.serve(ApproveAll).await });

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
        [relay_url],
        Some("one-time-secret".to_string()),
        None,
    )
    .expect("remote signer should be created");
    let bunker_uri = remote_signer.bunker_uri();
    let request_counts = Arc::new(RequestCounts::default());
    let signer_actions = CountRequests(Arc::clone(&request_counts));
    let signer_task = tokio::spawn(async move { remote_signer.serve(signer_actions).await });

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
            signer: signer_keys,
            user: user_keys.clone(),
        },
        [relay_url],
        None,
        None,
    )
    .expect("remote signer should be created");
    let sanitized_uri = remote_signer.bunker_uri();
    let request_counts = Arc::new(RequestCounts::default());
    let signer_actions = CountRequests(Arc::clone(&request_counts));
    let signer_task = tokio::spawn(async move { remote_signer.serve(signer_actions).await });
    tokio::task::yield_now().await;

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
