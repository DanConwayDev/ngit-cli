//! Relay budgets, resolved once per client using the same precedence as Git
//! HTTP timeouts.
use crate::git::{Repo, utils::git_config_value};

pub const RELAY_TIMEOUT_ENV: &str = "NGIT_RELAY_TIMEOUT_SECS";
pub const CONNECT_TIMEOUT_ENV: &str = "NGIT_RELAY_CONNECT_TIMEOUT_SECS";
const RELAY_TIMEOUT_CONFIG: &str = "nostr.relay-timeout-secs";
const CONNECT_TIMEOUT_CONFIG: &str = "nostr.relay-connect-timeout-secs";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayTimeouts {
    /// Long timeout for standalone connections, connection retries, event reads
    /// and adaptive fetch rounds. Existing callers retain their own deadlines.
    pub overall: u64,
    /// Fetch connection-attempt timeout and adaptive-fetch quorum grace.
    pub connect: u64,
}

impl RelayTimeouts {
    pub(crate) fn load(repo: Option<&Repo>) -> Self {
        let test_mode = std::env::var("NGITTEST").is_ok();
        Self::from_sources(
            test_mode,
            std::env::var(crate::test_timeouts::RELAY_TIMEOUT_ENV)
                .ok()
                .as_deref(),
            std::env::var(RELAY_TIMEOUT_ENV).ok().as_deref(),
            std::env::var(CONNECT_TIMEOUT_ENV).ok().as_deref(),
            git_config_value(repo, RELAY_TIMEOUT_CONFIG).as_deref(),
            git_config_value(repo, CONNECT_TIMEOUT_CONFIG).as_deref(),
        )
    }

    fn from_sources(
        test_mode: bool,
        test_override: Option<&str>,
        overall_env: Option<&str>,
        connect_env: Option<&str>,
        overall_config: Option<&str>,
        connect_config: Option<&str>,
    ) -> Self {
        if test_mode {
            let seconds = parse_timeout_secs(test_override)
                .unwrap_or(crate::test_timeouts::RelayTimeout::Normal.seconds());
            return Self {
                overall: seconds,
                connect: seconds,
            };
        }
        let overall = parse_timeout_secs(overall_env)
            .or_else(|| parse_timeout_secs(overall_config))
            .unwrap_or(45);
        let connect = parse_timeout_secs(connect_env)
            .or_else(|| parse_timeout_secs(connect_config))
            .unwrap_or(7);
        Self { overall, connect }
    }
}

fn parse_timeout_secs(value: Option<&str>) -> Option<u64> {
    value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|seconds| *seconds > 0)
        .map(u64::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_precedence_preserves_independent_timeouts() {
        let resolve = |env, connect_env, config, connect_config| {
            RelayTimeouts::from_sources(false, Some("1"), env, connect_env, config, connect_config)
        };
        assert_eq!(
            resolve(None, None, None, None),
            RelayTimeouts {
                overall: 45,
                connect: 7
            }
        );
        assert_eq!(
            resolve(None, None, Some("60"), Some("12")),
            RelayTimeouts {
                overall: 60,
                connect: 12
            }
        );
        assert_eq!(
            resolve(Some("20"), Some("4"), Some("60"), Some("12")),
            RelayTimeouts {
                overall: 20,
                connect: 4
            }
        );
        for invalid in ["", "0", "-1", "invalid", "4294967296"] {
            assert_eq!(
                resolve(Some(invalid), Some(invalid), Some("60"), Some("12")),
                RelayTimeouts {
                    overall: 60,
                    connect: 12
                }
            );
            assert_eq!(
                resolve(Some("3"), Some(invalid), None, None),
                RelayTimeouts {
                    overall: 3,
                    connect: 7
                }
            );
        }
        assert_eq!(
            resolve(Some(" 3 "), Some("20"), None, None),
            RelayTimeouts {
                overall: 3,
                connect: 20
            }
        );
    }

    #[test]
    fn test_mode_is_isolated_from_production_settings() {
        for (value, expected) in [
            (None, 10),
            (Some("0"), 10),
            (Some("bad"), 10),
            (Some("3"), 3),
            (Some("10"), 10),
        ] {
            assert_eq!(
                RelayTimeouts::from_sources(
                    true,
                    value,
                    Some("1"),
                    Some("2"),
                    Some("30"),
                    Some("40")
                ),
                RelayTimeouts {
                    overall: expected,
                    connect: expected
                }
            );
        }
    }

    #[test]
    fn repository_config_uses_the_http_timeout_reader() {
        use crate::git::RepoActions;
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let repo = Repo::from_path(&dir.path().to_path_buf()).unwrap();
        repo.save_git_config_item(RELAY_TIMEOUT_CONFIG, "19", false)
            .unwrap();
        repo.save_git_config_item(CONNECT_TIMEOUT_CONFIG, "5", false)
            .unwrap();
        let timeouts = RelayTimeouts::from_sources(
            false,
            None,
            None,
            None,
            git_config_value(Some(&repo), RELAY_TIMEOUT_CONFIG).as_deref(),
            git_config_value(Some(&repo), CONNECT_TIMEOUT_CONFIG).as_deref(),
        );
        assert_eq!(
            timeouts,
            RelayTimeouts {
                overall: 19,
                connect: 5
            }
        );
    }
}
