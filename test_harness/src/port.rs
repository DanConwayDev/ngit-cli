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
//! ngit-grasp's in-process fixtures (`MockRelay`, `SmartGitServer`)
//! eliminate the race entirely by **keeping the listener bound** and
//! handing it straight to their tokio accept loop. We can't do that here:
//! `LocalRelay::run()` binds internally on a port number, and
//! `ngit-grasp` is a subprocess that binds itself from `NGIT_BIND_ADDRESS`.
//! Neither accepts a pre-bound listener without significant rework
//! (rewriting the hyper accept loop, or fd inheritance via `pre_exec`
//! respectively).
//!
//! ## The reservation pattern
//!
//! Instead, [`reserve_port`] returns a [`PortReservation`] that **holds the
//! bound `TcpListener`** until the caller is about to start the real
//! service. While any reservation is live, no other call to
//! `reserve_port` in this process can be handed the same port — the
//! kernel won't reissue a port that is currently bound.
//!
//! The caller drops the reservation immediately before the real bind,
//! shrinking the TOCTOU window from "however long the fixture takes to
//! spawn" to "a few microseconds inside the start function". The retry
//! loops in `relay.rs` and `grasp.rs` are belt-and-braces for that
//! residual window — they have never been observed to fire in our local
//! stress testing post-reservation, but the cost is zero when they
//! don't.

use std::net::TcpListener;

use anyhow::{Context, Result};

/// A port that the kernel has assigned to us via `:0` bind, held open by
/// a live `TcpListener` so that no other [`reserve_port`] call in this
/// process can be handed the same number.
///
/// The reservation is released by:
///
/// - calling [`PortReservation::release`] to consume the reservation and return
///   the port number (preferred — makes the release explicit at the call site),
///   or
/// - simply dropping the value (also fine, but the release point is then tied
///   to lexical scope).
///
/// The caller should release **immediately** before the consuming service
/// performs its own `bind` so that the TOCTOU window between
/// reservation-release and service-bind is as small as possible.
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
    /// returning the port number. The port is now free for the caller's
    /// real service to bind. Prefer this over relying on lexical drop —
    /// it makes the release point explicit at the call site.
    pub fn release(self) -> u16 {
        let port = self.port;
        // `self` is consumed; the listener inside is dropped here.
        drop(self);
        port
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

#[cfg(test)]
mod tests {
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

    /// After `release`, the returned port is actually bindable.
    #[test]
    fn released_port_is_bindable() {
        let reservation = reserve_port().unwrap();
        let port = reservation.release();
        // The kernel held the port for us until release; immediately after,
        // we must be able to bind it again ourselves.
        let listener = TcpListener::bind(("127.0.0.1", port))
            .expect("port should be bindable immediately after release");
        drop(listener);
    }
}
