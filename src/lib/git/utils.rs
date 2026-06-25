use std::path::Path;

use anyhow::{Context, Result};
use directories::UserDirs;
use git2::opts::{set_server_connect_timeout_in_milliseconds, set_server_timeout_in_milliseconds};

use super::{Repo, RepoActions};

const DEFAULT_HTTP_CONNECT_TIMEOUT_MS: i32 = 3_000;
const DEFAULT_HTTP_IO_TIMEOUT_MS: i32 = 15_000;

const HTTP_CONNECT_TIMEOUT_ENV: &str = "NGIT_HTTP_CONNECT_TIMEOUT_MS";
const HTTP_IO_TIMEOUT_ENV: &str = "NGIT_HTTP_IO_TIMEOUT_MS";

const HTTP_CONNECT_TIMEOUT_CONFIG: &str = "nostr.http-connect-timeout-ms";
const HTTP_IO_TIMEOUT_CONFIG: &str = "nostr.http-io-timeout-ms";

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

pub fn set_git_timeout(git_repo: Option<&Repo>) -> Result<()> {
    let connect_ms = git_timeout_ms(
        git_repo,
        HTTP_CONNECT_TIMEOUT_ENV,
        HTTP_CONNECT_TIMEOUT_CONFIG,
        DEFAULT_HTTP_CONNECT_TIMEOUT_MS,
    );

    let io_ms = git_timeout_ms(
        git_repo,
        HTTP_IO_TIMEOUT_ENV,
        HTTP_IO_TIMEOUT_CONFIG,
        DEFAULT_HTTP_IO_TIMEOUT_MS,
    );

    unsafe {
        // Set a 3 000 ms timeout for establishing the TCP connection (default: 60 000
        // ms). Override with NGIT_HTTP_CONNECT_TIMEOUT_MS or
        // nostr.http-connect-timeout-ms.
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
        // For GRASP servers that buffer the entire receive-pack response (as
        // ngit-grasp <= v1.1 does by reading `git receive-pack` stdout to EOF
        // before sending any HTTP body bytes back), the client sees zero bytes
        // for as long as the server takes to index the pushed pack. Big packs
        // (>100k objects) can easily exceed 15 s of silence, manifesting as
        // `could not read from socket: timed out; class=Net (12); code=Timeout
        // (-37)` even though the push actually completed server-side.
        //
        // Workaround: override via the NGIT_HTTP_IO_TIMEOUT_MS env var or the
        // nostr.http-io-timeout-ms git config key.
        // Recommended values: 600_000 (10 min) for trees with >500k objects,
        // 60_000 (1 min) for trees with >50k objects, default 15_000 otherwise.
        set_server_timeout_in_milliseconds(io_ms).context("failed to set libgit2 I/O timeout")?;

        Ok(())
    }
}

fn git_timeout_ms(
    git_repo: Option<&Repo>,
    env_key: &str,
    config_key: &str,
    default_ms: i32,
) -> i32 {
    let env_value = std::env::var(env_key).ok();
    let config_value = git_config_value(git_repo, config_key);

    git_timeout_ms_from_sources(env_value.as_deref(), config_value.as_deref(), default_ms)
}

fn git_config_value(git_repo: Option<&Repo>, config_key: &str) -> Option<String> {
    if let Some(git_repo) = git_repo {
        git_repo
            .get_git_config_item(config_key, None)
            .ok()
            .flatten()
    } else {
        git2::Config::open_default().ok().and_then(|config| {
            config
                .get_entry(config_key)
                .ok()
                .and_then(|entry| entry.value().ok().map(str::to_string))
        })
    }
}

fn git_timeout_ms_from_sources(
    env_value: Option<&str>,
    config_value: Option<&str>,
    default_ms: i32,
) -> i32 {
    env_value
        .and_then(|v| v.parse::<i32>().ok())
        .or_else(|| config_value.and_then(|v| v.parse::<i32>().ok()))
        .unwrap_or(default_ms)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;
    use crate::git::test_helpers::GitTestRepo;

    #[test]
    fn timeout_env_value_overrides_git_config_value() {
        assert_eq!(
            git_timeout_ms_from_sources(Some("600000"), Some("45000"), 15_000),
            600_000
        );
    }

    #[test]
    fn timeout_git_config_value_overrides_default() {
        assert_eq!(
            git_timeout_ms_from_sources(None, Some("45000"), 15_000),
            45_000
        );
    }

    #[test]
    fn timeout_invalid_values_fall_back_to_next_source() {
        assert_eq!(
            git_timeout_ms_from_sources(Some("not-a-number"), Some("45000"), 15_000),
            45_000
        );
        assert_eq!(
            git_timeout_ms_from_sources(Some("not-a-number"), Some("also-bad"), 15_000),
            15_000
        );
    }

    #[test]
    fn timeout_reads_repo_local_git_config() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        fixture
            .git_repo
            .config()?
            .set_str(HTTP_IO_TIMEOUT_CONFIG, "600000")?;
        let repo = Repo::from_path(&fixture.dir)?;

        assert_eq!(
            git_timeout_ms(
                Some(&repo),
                "NGIT_TEST_HTTP_IO_TIMEOUT_MS_UNSET",
                HTTP_IO_TIMEOUT_CONFIG,
                DEFAULT_HTTP_IO_TIMEOUT_MS,
            ),
            600_000
        );

        Ok(())
    }
}
