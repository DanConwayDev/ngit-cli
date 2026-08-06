use std::{sync::Arc, time::Duration};

use ngit::{cli_interactor::Printer, login::fresh::listen_for_remote_signer};
use nostr::{
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
