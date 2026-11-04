//! Request publication is evidence about relays, not about signer receipt.

use std::time::Duration;

use anyhow::{Result, bail};
use ngit::remote_signer::{ConnectProgress, NostrConnect};
use nostr::prelude::*;
use nostr_sdk::relay::EventSendStatus;
use test_harness::{Harness, LocalRelayBuilderNip42};

#[tokio::test]
async fn reports_mixed_relay_outcomes_without_claiming_a_signer_response() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("accepting")
    .with_relay_nip42("rejecting", LocalRelayBuilderNip42::write())
    .build()
    .await?;
    let accepting = RelayUrl::parse(harness.relay("accepting").url())?;
    let rejecting = RelayUrl::parse(harness.relay("rejecting").url())?;
    let connect = NostrConnect::new(
        NostrConnectUri::Bunker {
            remote_signer_public_key: Keys::generate().public_key(),
            relays: vec![accepting.clone(), rejecting.clone()],
            secret: None,
        },
        Keys::generate(),
        Duration::from_secs(60),
        None,
    )?;
    let mut progress = connect.progress();
    let operation = connect.get_public_key_async();
    tokio::pin!(operation);

    tokio::time::timeout(Duration::from_secs(30), async {
        let mut request = None;
        loop {
            tokio::select! {
                result = &mut operation => bail!("absent signer unexpectedly completed: {result:?}"),
                event = progress.recv() => match event? {
                    ConnectProgress::Request { event_id, method } => {
                        assert_eq!(method, NostrConnectMethod::Connect);
                        request = Some(event_id);
                    }
                    ConnectProgress::Sent { event_id, output } => {
                        assert_eq!(Some(event_id), request);
                        assert!(matches!(output.success.get(&accepting), Some(EventSendStatus::Ack(_))));
                        assert!(output.failed.contains_key(&rejecting), "{output:?}");
                        assert!(!output.success.contains_key(&rejecting));
                        return Ok(());
                    }
                    ConnectProgress::Response { .. } | ConnectProgress::Paired => {
                        bail!("relay acceptance was mistaken for a signer response");
                    }
                    ConnectProgress::Listening { .. } | ConnectProgress::Ready { .. } => {}
                }
            }
        }
    })
    .await?
}
