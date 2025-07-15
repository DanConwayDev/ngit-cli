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
    unsafe {
        // Set a 3 000 ms timeout for establishing the TCP connection (default: 60 000
        // ms).
        set_server_connect_timeout_in_milliseconds(3_000)
            .context("failed to set libgit2 connect timeout")?;

        // The server timeout applies per socket send()/recv() call rather than
        // to the entire fetch or push. libgit2 transfers data in ~16 KiB chunks,
        // so each chunk’s transfer is subject to this timeout instead of the
        // overall command.
        //
        // We set it to 15 000 ms (instead of the 300 000 ms default) to quickly
        // abort any stalled ~16 KiB chunk transfer—enabling fast failover across
        // redundant Git servers—while still accommodating transient hiccups.
        set_server_timeout_in_milliseconds(15_000).context("failed to set libgit2 I/O timeout")?;

        Ok(())
    }
}
