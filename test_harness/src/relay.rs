//! Vanilla in-process nostr relay backed by `nostr-relay-builder::LocalRelay`.
//!
//! Accepts arbitrary events with the relay-builder default `Generic` mode —
//! suitable for user metadata (kind 0), relay lists (kind 10002), signer
//! connect events, etc. Not a GRASP server (no git smart-http, no repo-only
//! filtering); a future PR adds GRASP via subprocess.
//!
//! Shutdown is `Drop`-driven: `LocalRelay` is internally an
//! `AtomicDestructor`, so the listener thread terminates when every clone
//! goes out of scope.

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use anyhow::{Context, Result};
use nostr_relay_builder::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::*;

/// A vanilla nostr relay bound to a fixed loopback port.
///
/// One instance per `with_relay(role)` call on the harness builder. Multiple
/// instances under the same role aggregate into the corresponding env-var
/// roster as a `;`-separated list.
#[derive(Clone, Debug)]
pub struct VanillaRelay {
    role: String,
    url: String,
    /// Held purely for its `Drop` side-effect — `LocalRelay` is an
    /// `AtomicDestructor` that shuts the listener down when the last clone
    /// is dropped. We never call methods on it after `run()`.
    #[allow(dead_code)]
    relay: LocalRelay,
}

impl VanillaRelay {
    /// Start a relay bound to the given loopback port and run it.
    pub(crate) async fn start(role: impl Into<String>, port: u16) -> Result<Self> {
        let builder = RelayBuilder::default()
            .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .port(port);
        let relay = LocalRelay::new(builder);
        relay.run().await.context("failed to start LocalRelay")?;
        let url = format!("ws://127.0.0.1:{port}");
        Ok(Self {
            role: role.into(),
            url,
            relay,
        })
    }

    /// Role label this relay was registered under (e.g. `"default"`).
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Websocket URL — `ws://127.0.0.1:<port>` form, suitable for the
    /// `NGIT_RELAY_*` env vars and for `nostr-sdk` clients alike.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Query the relay's event store via a real `nostr-sdk` client over
    /// websocket. No in-process database shortcut — this exercises the
    /// same wire path a production client would.
    ///
    /// The connection is short-lived: a single REQ + EOSE + disconnect.
    pub async fn events(&self, filter: Filter) -> Result<Vec<Event>> {
        let client = Client::default();
        client
            .add_relay(&self.url)
            .await
            .with_context(|| format!("failed to add relay {}", self.url))?;
        client.connect().await;
        let events = client
            .fetch_events(filter, Duration::from_secs(5))
            .await
            .context("failed to fetch events from relay")?;
        client.disconnect().await;
        Ok(events.into_iter().collect())
    }
}
