use std::path::Path;

use anyhow::{Context, Result};
use directories::UserDirs;
use git2::opts::{set_server_connect_timeout_in_milliseconds, set_server_timeout_in_milliseconds};

pub fn check_ssh_keys() -> bool {
    // Get the user's home directory using the directories crate
    if let Some(user_dirs) = UserDirs::new() {
        let ssh_dir = user_dirs.home_dir().join(".ssh");
        let key_files = vec![
            "id_rsa",
            "id_ecdsa",
            "id_ed25519",
            "id_rsa.pub",
            "id_ecdsa.pub",
            "id_ed25519.pub",
        ];

        for key in key_files {
            if Path::new(&ssh_dir.join(key)).exists() {
                return true; // At least one key exists
            }
        }
    }
    false // No keys found
}

pub fn set_git_timeout() -> Result<()> {
    let connect_ms = std::env::var("NGIT_HTTP_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(3_000);

    let io_ms = std::env::var("NGIT_HTTP_IO_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(15_000);

    unsafe {
        // Set a 3 000 ms timeout for establishing the TCP connection (default: 60 000
        // ms). Override with NGIT_HTTP_CONNECT_TIMEOUT_MS env var.
        set_server_connect_timeout_in_milliseconds(connect_ms)
            .context("failed to set libgit2 connect timeout")?;

        // The server timeout applies per socket send()/recv() call rather than
        // to the entire fetch or push. libgit2 transfers data in ~16 KiB chunks,
        // so each chunk’s transfer is subject to this timeout instead of the
        // overall command.
        //
        // Default is 15 000 ms (instead of libgit2's 300 000 ms default) to quickly
        // abort any stalled ~16 KiB chunk transfer—enabling fast failover across
        // redundant Git servers—while still accommodating transient hiccups.
        //
        // For GRASP servers that buffer the entire receive-pack response (the
        // current ngit-grasp implementation reads `git receive-pack` stdout to
        // EOF before sending any HTTP body bytes back), the client sees zero
        // bytes for as long as the server takes to index the pushed pack. Big
        // packs (>100k objects) can easily exceed 15 s of silence, manifesting
        // as `could not read from socket: timed out; class=Net (12); code=Timeout (-37)`
        // even though the push actually completed server-side.
        //
        // Workaround: override via the NGIT_HTTP_IO_TIMEOUT_MS env var.
        // Recommended values: 600_000 (10 min) for trees with >500k objects,
        // 60_000 (1 min) for trees with >50k objects, default 15_000 otherwise.
        set_server_timeout_in_milliseconds(io_ms).context("failed to set libgit2 I/O timeout")?;

        Ok(())
    }
}
