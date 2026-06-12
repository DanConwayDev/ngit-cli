//! Real-wire event-store queries.
//!
//! Both [`crate::VanillaRelay`] and [`crate::GraspServer`] expose
//! `events(filter)` that queries the relay's event store over a fresh
//! websocket REQ. This module is the one place that knows how to drive a
//! short-lived `nostr-sdk` client: connect, fetch, disconnect.
//!
//! Both helpers explicitly avoid in-process database shortcuts so the
//! query path is identical to the one a production client would take.

use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;

/// Default query budget — generous enough for in-process / loopback relays
/// under load on CI, short enough to keep tests from hanging indefinitely
/// when the relay has dropped events.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to `relay_url`, REQ with `filter`, await EOSE, then disconnect.
pub(crate) async fn fetch_events(relay_url: &str, filter: Filter) -> Result<Vec<Event>> {
    let client = Client::default();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url}"))?;
    client.connect().await;
    let events = client
        .fetch_events(filter)
        .timeout(QUERY_TIMEOUT)
        .await
        .with_context(|| format!("failed to fetch events from {relay_url}"))?;
    client.disconnect().await;
    Ok(events.into_iter().collect())
}
