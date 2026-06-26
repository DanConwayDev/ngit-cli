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
    error::Error,
    io,
    net::{IpAddr, Ipv4Addr},
};

use anyhow::{Context, Result};
use nostr_relay_builder::{Error as RelayBuilderError, LocalRelay, error::ErrorKind};
use nostr_sdk::prelude::*;

use crate::{
    port::{self, PortReservation},
    query,
};

/// How many fresh port reservations to attempt before giving up. The
/// reservation pattern in `port.rs` already prevents same-process
/// collisions while reservations are held; this retry exists purely to
/// cover the microsecond-scale TOCTOU window between
/// [`PortReservation::release`] and `LocalRelay::run`'s internal `bind`.
///
/// In practice this loop has never been observed to fire in local
/// stress testing — kept as defense-in-depth for CI / loaded hardware
/// where the residual race may surface. Five tries is plenty: each
/// retry draws a brand-new kernel-assigned port, so two consecutive
/// `AddrInUse` failures would require two independent races back to
/// back.
const MAX_BIND_ATTEMPTS: usize = 5;

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
    /// Start a relay on the port held by `reservation` and run it.
    ///
    /// The reservation is released (its `TcpListener` dropped) immediately
    /// before `LocalRelay::run()` performs its own internal `bind`. This
    /// shrinks the TOCTOU window and ensures that no other `reserve_port`
    /// call in this process can be handed the same port number while this
    /// fixture is starting up.
    ///
    /// On the off-chance another parallel test grabs the port in the
    /// microseconds between release and bind, we retry with a fresh
    /// reservation up to [`MAX_BIND_ATTEMPTS`] times. The retry has not
    /// been observed to fire in local stress runs — it's defense-in-depth
    /// for CI / loaded hardware.
    pub(crate) async fn start(
        role: impl Into<String>,
        mut reservation: PortReservation,
    ) -> Result<Self> {
        let role = role.into();
        for attempt in 1..=MAX_BIND_ATTEMPTS {
            // Release the reservation right before LocalRelay performs its
            // own bind. The two operations are not atomic — `LocalRelay::run`
            // takes the port number, not a pre-bound listener — but
            // releasing here narrows the gap to microseconds, and parallel
            // `reserve_port` calls in this process cannot have been handed
            // this port number while we held the reservation.
            let port = reservation.release();
            let relay = LocalRelay::builder()
                .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
                .port(port)
                .build();
            match relay.run().await {
                Ok(()) => {
                    let url = relay.url().await.to_string();
                    return Ok(Self { role, url, relay });
                }
                Err(e) if is_addr_in_use(&e) && attempt < MAX_BIND_ATTEMPTS => {
                    // Lost the race against another parallel test in the
                    // post-release window. Get a brand new port from the
                    // kernel and try again.
                    reservation = port::reserve_port().context(
                        "failed to reserve replacement port after AddrInUse on LocalRelay bind",
                    )?;
                    continue;
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e).context(format!(
                        "failed to start LocalRelay (attempt {attempt}/{MAX_BIND_ATTEMPTS})"
                    )));
                }
            }
        }
        // Unreachable: the loop either returns `Ok` or returns an `Err`
        // on the final iteration via the second `Err` arm.
        unreachable!("MAX_BIND_ATTEMPTS loop terminated without returning")
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

/// `true` iff `e` is an I/O `AddrInUse` (EADDRINUSE) — the signature of
/// having lost the port-allocation race.
fn is_addr_in_use(e: &RelayBuilderError) -> bool {
    e.kind() == ErrorKind::IO
        && e.source()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .is_some_and(|io_err| io_err.kind() == io::ErrorKind::AddrInUse)
}
