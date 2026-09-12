//! Vanilla in-process nostr relay backed by
//! `nostr_sdk::local_relay::LocalRelay`.
//!
//! Accepts arbitrary events with the builder's default `Generic` mode —
//! suitable for user metadata (kind 0), relay lists (kind 10002), signer
//! connect events, etc. Not a GRASP server (no git smart-http, no repo-only
//! filtering); Grasp fixtures use a separate subprocess.
//!
//! The listener and its connection tasks remain owned until the last fixture
//! clone drops. Startup transfers the original reservation without rebinding.

use std::sync::Arc;

use anyhow::{Context, Result};
use nostr_sdk::{
    local_relay::{LocalRelay, LocalRelayBuilderNip42},
    prelude::*,
};

use crate::{port::PortReservation, query};

mod listener;
use listener::ListenerTask;

/// A vanilla nostr relay bound to a fixed loopback port.
///
/// One instance per `with_relay(role)` call on the harness builder. Multiple
/// instances under the same role aggregate into the corresponding env-var
/// roster as a `;`-separated list.
#[derive(Clone, Debug)]
pub struct VanillaRelay {
    role: String,
    url: String,
    /// Keeps the SDK relay state alive for the fixture's connection tasks.
    #[allow(dead_code)]
    relay: LocalRelay,
    _listener: Arc<ListenerTask>,
}

impl VanillaRelay {
    /// Transfer the reserved listener into the relay without releasing its
    /// port.
    pub(crate) async fn start(
        role: impl Into<String>,
        reservation: PortReservation,
        nip42: Option<LocalRelayBuilderNip42>,
    ) -> Result<Self> {
        let role = role.into();
        let listener = reservation.into_std_listener();
        listener
            .set_nonblocking(true)
            .context("set relay listener nonblocking")?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let url = format!("ws://{}", listener.local_addr()?);
        let mut builder = LocalRelay::builder();
        if let Some(nip42) = nip42 {
            builder = builder.nip42(nip42);
        }
        let relay = builder.build();
        let task = ListenerTask::start(listener, relay.clone());
        Ok(Self {
            role,
            url,
            relay,
            _listener: Arc::new(task),
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
        query::fetch_events(&self.url, filter).await
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, time::Duration};

    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::port::reserve_port;

    #[tokio::test]
    async fn listener_stays_owned_until_last_relay_clone_drops() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let reservation = reserve_port().unwrap();
            let address = format!("127.0.0.1:{}", reservation.port());
            let relay = VanillaRelay::start("lifetime", reservation, None)
                .await
                .unwrap();
            assert_eq!(relay.url(), format!("ws://{address}"));
            assert!(TcpListener::bind(&address).is_err());
            assert!(relay.events(Filter::new()).await.unwrap().is_empty());

            let clone = relay.clone();
            drop(relay);
            assert!(TcpListener::bind(&address).is_err());
            assert!(clone.events(Filter::new()).await.unwrap().is_empty());

            // Keep an unfinished handshake open: cancelling the fixture must
            // close accepted connections as well as the listening socket.
            let mut connection = tokio::net::TcpStream::connect(&address).await.unwrap();
            drop(clone);
            let mut byte = [0];
            match connection.read(&mut byte).await {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                result => panic!("connection should close with the last fixture: {result:?}"),
            }
        })
        .await
        .expect("relay lifecycle must complete within the deadline");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_relays_keep_their_original_reserved_addresses() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut starts = tokio::task::JoinSet::new();
            for _ in 0..32 {
                let reservation = reserve_port().unwrap();
                let port = reservation.port();
                starts.spawn(async move {
                    let relay = VanillaRelay::start("parallel", reservation, None)
                        .await
                        .unwrap();
                    assert_eq!(relay.url(), format!("ws://127.0.0.1:{port}"));
                    assert!(relay.events(Filter::new()).await.unwrap().is_empty());
                    relay
                });
            }
            let mut relays = Vec::new();
            while let Some(result) = starts.join_next().await {
                relays.push(result.unwrap());
            }
            let urls: std::collections::HashSet<_> = relays.iter().map(VanillaRelay::url).collect();
            assert_eq!(urls.len(), 32);
        })
        .await
        .expect("parallel relay startup must complete within the deadline");
    }
}
