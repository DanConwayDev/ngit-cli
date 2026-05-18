//! OS-assigned port allocation.
//!
//! Mirrors the ngit-grasp test pattern: bind `127.0.0.1:0`, capture the
//! port the kernel chose, drop the listener so the port is free for the
//! real service to bind to. There is a TOCTOU window between drop and
//! re-bind, but in practice the kernel reuses ports lazily enough that
//! conflicts during a test run are negligible — and using port 0 is the
//! only way to actually parallelise.

use std::net::TcpListener;

use anyhow::{Context, Result};

/// Bind `127.0.0.1:0`, read the assigned port, then drop the listener so
/// the port is available for a fresh bind by the test's actual service.
pub fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind 127.0.0.1:0")?;
    let port = listener
        .local_addr()
        .context("failed to read local_addr from bound listener")?
        .port();
    drop(listener);
    Ok(port)
}
