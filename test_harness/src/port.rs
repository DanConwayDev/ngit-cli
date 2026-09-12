//! Race-free port reservation for test fixtures.
//!
//! ## The race
//!
//! The naive pattern — bind `127.0.0.1:0`, read the kernel-assigned port,
//! drop the listener, hand the bare `u16` to whoever wants it — has a
//! TOCTOU window between drop and the consumer's actual `bind`. During
//! that window, anything else in the process (or, more rarely, another
//! process) can be handed the same port by the kernel.
//!
//! The race is rare on lightly-loaded hardware — running the full
//! non-legacy suite ten times sequentially on a developer workstation
//! triggered zero collisions with the naive pattern — but it has been
//! observed at least once each in CI and during local development, and
//! the failure mode (`Address already in use (os error 98)`) is a hard
//! test fail with no useful information for the next debugger.
//!
//! In-process services consume the bound listener directly. Supported Grasp
//! subprocesses inherit it, retaining ownership across startup. Compatibility
//! paths for older binaries and third-party services release immediately before
//! spawning and retry bind failures; only those paths retain a handoff race.
//!
//! [`reserve_port`] returns a [`PortReservation`] that holds its listener until
//! ownership is transferred with [`PortReservation::into_std_listener`]. The
//! kernel cannot assign that address to another listener while it remains
//! bound.

use std::net::{SocketAddr, TcpListener};

use anyhow::{Context, Result};

/// A port that the kernel has assigned to us via `:0` bind, held open by
/// a live `TcpListener` so that no other [`reserve_port`] call in this
/// process can be handed the same number.
///
/// Transfer the listener with [`PortReservation::into_std_listener`] whenever
/// possible. [`PortReservation::release`] exists for services that cannot
/// accept a listener; those callers must handle a competing bind after release.
#[derive(Debug)]
pub struct PortReservation {
    port: u16,
    /// The listener whose binding holds the port. Dropped on
    /// [`Self::release`] or when the reservation goes out of scope.
    _listener: TcpListener,
}

impl PortReservation {
    /// The kernel-assigned loopback port number held by this reservation.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Consume the reservation, dropping the underlying listener and
    /// returning the port number. Another service may acquire it immediately,
    /// so callers must handle bind failures. Prefer transferring the listener.
    pub fn release(self) -> u16 {
        let port = self.port;
        // `self` is consumed; the listener inside is dropped here.
        drop(self);
        port
    }

    /// Consume the reservation and return the bound [`TcpListener`]
    /// itself. The port number is **never released back to the OS** —
    /// the fd is handed straight to the caller, which is the zero-TOCTOU
    /// path for fixtures that can promote a `std` listener into their
    /// own accept loop (e.g. a hyper server via
    /// `tokio::net::TcpListener::from_std`).
    ///
    /// Prefer this over [`Self::release`] + bind whenever the consuming
    /// service can take a pre-bound listener; it eliminates the
    /// microsecond-scale race that `MAX_BIND_ATTEMPTS` loops elsewhere
    /// in this crate exist to cover.
    pub fn into_std_listener(self) -> TcpListener {
        self._listener
    }
}

/// Bind `127.0.0.1:0`, capture the assigned port, and **keep the listener
/// bound** inside the returned [`PortReservation`] until the caller
/// releases it.
///
/// While the reservation is live, no other `reserve_port` call in this
/// process will be handed the same port. See module docs for why this
/// matters.
pub fn reserve_port() -> Result<PortReservation> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind 127.0.0.1:0")?;
    let port = listener
        .local_addr()
        .context("failed to read local_addr from bound listener")?
        .port();
    Ok(PortReservation {
        port,
        _listener: listener,
    })
}

/// A test-owned loopback endpoint that makes every TCP attempt fail at the
/// protocol layer by closing each accepted connection without a response.
///
/// Unlike binding an ephemeral port and then releasing it to manufacture a
/// supposedly dead address, this endpoint owns its port for its full lifetime.
/// A failure-path test therefore cannot race another process that happens to
/// claim the released port.
#[derive(Debug)]
pub struct UnavailableTcpEndpoint {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl UnavailableTcpEndpoint {
    /// Bind an ephemeral loopback port and start closing incoming connections.
    pub async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind unavailable TCP test endpoint")?;
        let addr = listener
            .local_addr()
            .context("failed to read unavailable TCP test endpoint address")?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        Ok(Self { addr, task })
    }

    /// The loopback socket address owned by this endpoint.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for UnavailableTcpEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncReadExt;

    use super::*;

    /// Two reservations held simultaneously must return distinct ports.
    /// This is the core same-process guarantee the reservation pattern
    /// provides — and exactly what the naive "bind, drop, return" pattern
    /// fails to give under parallel load.
    #[test]
    fn parallel_reservations_get_distinct_ports() {
        let a = reserve_port().unwrap();
        let b = reserve_port().unwrap();
        let c = reserve_port().unwrap();
        assert_ne!(a.port(), b.port());
        assert_ne!(b.port(), c.port());
        assert_ne!(a.port(), c.port());
    }

    /// Transferring ownership preserves the reservation without a rebind gap.
    #[test]
    fn transferred_listener_keeps_port_reserved() {
        let reservation = reserve_port().unwrap();
        let port = reservation.port();
        let listener = reservation.into_std_listener();
        assert_eq!(listener.local_addr().unwrap().port(), port);
        assert!(TcpListener::bind(listener.local_addr().unwrap()).is_err());
    }

    #[tokio::test]
    async fn unavailable_endpoint_owns_port_and_closes_connections() {
        let endpoint = UnavailableTcpEndpoint::start().await.unwrap();

        let rebound = TcpListener::bind(endpoint.addr());
        assert!(
            rebound.is_err(),
            "endpoint must keep its assigned port bound for its lifetime"
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            let mut stream = tokio::net::TcpStream::connect(endpoint.addr())
                .await
                .expect("endpoint should accept TCP connections");
            let mut byte = [0_u8; 1];
            let read = stream.read(&mut byte).await;
            assert!(
                matches!(read, Ok(0) | Err(_)),
                "endpoint must close without returning application data"
            );
        })
        .await
        .expect("endpoint did not close a connection promptly");
    }
}
