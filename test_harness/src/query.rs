//! Real-wire event-store queries.
//!
//! Both [`crate::VanillaRelay`] and [`crate::GraspServer`] expose
//! `events(filter)` that queries the relay's event store over a fresh
//! websocket REQ. This module is the one place that knows how to drive a
//! short-lived `nostr-sdk` client: connect, fetch, disconnect.
//!
//! Both helpers explicitly avoid in-process database shortcuts so the
//! query path is identical to the one a production client would take.

use std::{collections::BTreeSet, time::Duration};

use anyhow::{Context, Result, bail};
use nostr_sdk::{
    authenticator::SignerAuthenticator, client::ClientBuilder, prelude::*, relay::RelayLimits,
};

/// Default query budget — generous enough for in-process / loopback relays
/// under load on CI, short enough to keep tests from hanging indefinitely
/// when the relay has dropped events.
const QUERY_TIMEOUT: Duration = Duration::from_secs(crate::RelayTimeout::Normal.seconds());

/// Connect to `relay_url`, REQ with `filter`, await EOSE, then disconnect.
pub(crate) async fn fetch_events(relay_url: &str, filter: Filter) -> Result<Vec<Event>> {
    let client = Client::default();
    fetch_events_with_client(client, relay_url, filter, QUERY_TIMEOUT).await
}

/// Connect and query as `keys`, allowing a private relay to complete NIP-42
/// before it handles the REQ.
pub(crate) async fn fetch_events_as(
    relay_url: &str,
    keys: &Keys,
    filter: Filter,
) -> Result<Vec<Event>> {
    let client = ClientBuilder::default()
        .relay_limits(RelayLimits::disable())
        .verify_subscriptions(true)
        .authenticator(SignerAuthenticator::new(keys.clone()))
        .build();
    fetch_events_with_client(client, relay_url, filter, QUERY_TIMEOUT).await
}

async fn fetch_events_with_client(
    client: Client,
    relay_url: &str,
    filter: Filter,
    timeout: Duration,
) -> Result<Vec<Event>> {
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url}"))?;
    let result = tokio::time::timeout(timeout, async {
        let relay = client.relay(relay_url).await?.context("query relay missing")?;
        relay.try_connect().await?;
        let id = SubscriptionId::generate();
        let mut notifications = relay.notifications();
        let mut stream = relay.stream_events(filter).with_id(id.clone()).await?;
        let mut events = BTreeSet::new();
        let mut received_eose = false;
        let mut drained = false;
        // SDK fetches can return partial results when a stream ends, and the
        // multi-relay client also suppresses individual relay errors. Require
        // this subscription's EOSE plus all SDK-validated buffered events.
        while !received_eose || !drained {
            tokio::select! {
                event = stream.next(), if !drained => match event {
                    Some(event) => {
                        let event = event?;
                        anyhow::ensure!(events.len() < 10_000 || events.contains(&event), "query exceeded 10,000 events");
                        events.insert(event);
                    }
                    None => drained = true,
                },
                notification = notifications.next(), if !received_eose => match notification {
                    Some(RelayNotification::Message { message }) => {
                        if matches!(*message, RelayMessage::EndOfStoredEvents(ref subscription) if subscription.as_ref() == &id) {
                            received_eose = true;
                        }
                    }
                    Some(RelayNotification::RelayStatus {
                        status: RelayStatus::Disconnected | RelayStatus::Terminated | RelayStatus::Banned,
                    }) | None => bail!("relay disconnected before query completion"),
                    _ => {}
                },
            }
        }
        Ok(events.into_iter().collect())
    }).await;
    client.disconnect().await;
    result
        .context("timed out waiting for complete relay query")?
        .with_context(|| format!("failed to query relay {relay_url}"))
}

#[cfg(test)]
mod tests {
    use futures::SinkExt;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::*;

    #[tokio::test]
    async fn queries_require_matching_eose_and_propagate_rejection() -> Result<()> {
        // A short deadline is used only when timeout itself is under test.
        for outcome in [
            "complete",
            "empty",
            "partial-close",
            "empty-close",
            "wrong-eose",
            "closed",
            "silent",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("ws://{}", listener.local_addr()?);
            let event =
                EventBuilder::new(Kind::TextNote, "query evidence").finalize(&Keys::generate())?;
            let expected = event.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await?;
                let mut ws = accept_async(socket).await?;
                let mut received_request = false;
                while let Some(frame) = ws.next().await {
                    let frame = frame?;
                    if !frame.is_text() {
                        continue;
                    }
                    let request: serde_json::Value = serde_json::from_str(frame.to_text()?)?;
                    if request[0] != "REQ" {
                        continue;
                    }
                    received_request = true;
                    if matches!(outcome, "complete" | "partial-close") {
                        ws.send(Message::Text(
                            serde_json::json!(["EVENT", request[1], event])
                                .to_string()
                                .into(),
                        ))
                        .await?;
                    }
                    if matches!(outcome, "complete" | "empty" | "wrong-eose") {
                        let id = if outcome == "wrong-eose" {
                            serde_json::json!("another-subscription")
                        } else {
                            request[1].clone()
                        };
                        ws.send(Message::Text(
                            serde_json::json!(["EOSE", id]).to_string().into(),
                        ))
                        .await?;
                    }
                    if outcome == "closed" {
                        ws.send(Message::Text(
                            serde_json::json!(["CLOSED", request[1], "blocked: fixture rejection"])
                                .to_string()
                                .into(),
                        ))
                        .await?;
                    }
                    if matches!(outcome, "partial-close" | "empty-close" | "wrong-eose") {
                        ws.close(None).await?;
                    }
                    let _ = stopped.await;
                    break;
                }
                anyhow::ensure!(received_request, "fixture never received the query");
                Ok::<_, anyhow::Error>(())
            });
            let timeout = if outcome == "silent" {
                Duration::from_millis(250)
            } else {
                QUERY_TIMEOUT
            };
            let result = fetch_events_with_client(
                Client::default(),
                &url,
                Filter::new().kind(Kind::TextNote),
                timeout,
            )
            .await;
            let _ = stop.send(());
            tokio::time::timeout(QUERY_TIMEOUT, server).await???;
            match outcome {
                "complete" => assert_eq!(result?, vec![expected]),
                "empty" => assert!(result?.is_empty()),
                "closed" => {
                    assert!(format!("{:#}", result.unwrap_err()).contains("fixture rejection"))
                }
                "silent" => assert!(
                    format!("{:#}", result.unwrap_err())
                        .contains("timed out waiting for complete relay query")
                ),
                _ => assert!(
                    result.is_err(),
                    "{outcome} accepted incomplete results: {result:?}"
                ),
            }
        }
        Ok(())
    }
}
