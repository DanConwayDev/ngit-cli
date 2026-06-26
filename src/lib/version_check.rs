use std::{
    cmp::Ordering,
    path::Path,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    time::Duration,
};

use anyhow::Result;
use console::{Color, Style};
use nostr::{
    RelayUrl,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};
use nostr_sdk::{
    client::ClientBuilder,
    relay::{RelayLimits, ReqExitPolicy},
};

use crate::client::{
    STATE_KIND, get_event_from_global_cache, get_filter_state_events, save_event_in_global_cache,
};

pub const UPDATE_RELAY_HOSTS: [&str; 2] = ["relay.ngit.dev", "gitnostr.com"];
const NGIT_REPO_COORDINATE: &str =
    "30617:a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d:ngit";
static UPDATE_NOTICE_CHECKED: AtomicBool = AtomicBool::new(false);
const UPDATE_NOTICE_COLOR: Color = Color::Color256(214);

#[must_use]
pub fn ngit_repo_coordinate() -> Nip19Coordinate {
    Nip19Coordinate {
        coordinate: Coordinate::parse(NGIT_REPO_COORDINATE)
            .expect("hard-coded ngit repository coordinate must parse"),
        relays: vec![],
    }
}

#[must_use]
pub fn is_version_check_relay(relay_url: &RelayUrl) -> bool {
    let without_scheme = relay_url
        .as_str()
        .strip_prefix("wss://")
        .or_else(|| relay_url.as_str().strip_prefix("ws://"))
        .unwrap_or(relay_url.as_str());
    let host = without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .split('@')
        .next_back()
        .unwrap_or(without_scheme)
        .split(':')
        .next()
        .unwrap_or(without_scheme);
    UPDATE_RELAY_HOSTS.contains(&host)
}

#[must_use]
pub fn ngit_repo_state_filter() -> nostr::Filter {
    get_filter_state_events(&[ngit_repo_coordinate()].into_iter().collect(), true)
}

pub async fn print_update_notice_if_available(git_repo_path: Option<&Path>) -> Result<()> {
    if UPDATE_NOTICE_CHECKED.swap(true, AtomicOrdering::Relaxed) {
        return Ok(());
    }

    if let Some((current, latest)) = available_update_versions_from_cache(git_repo_path).await? {
        let message = format!(
            "ngit {latest} is available; you have v{current}. Upgrade: curl -Ls https://ngit.dev/install.sh | bash"
        );
        eprintln!(
            "{}",
            Style::new()
                .fg(UPDATE_NOTICE_COLOR)
                .apply_to(message)
                .for_stderr()
        );
    }
    Ok(())
}

pub async fn refresh_update_cache(git_repo_path: Option<&Path>) -> Result<()> {
    let client = ClientBuilder::default()
        .relay_limits(RelayLimits::disable())
        .verify_subscriptions(true)
        .build();
    let filter = ngit_repo_state_filter();

    for host in UPDATE_RELAY_HOSTS {
        let Ok(relay_url) = RelayUrl::parse(&format!("wss://{host}")) else {
            continue;
        };
        if client.add_relay(relay_url.clone()).await.is_err() {
            continue;
        }
        if client.connect_relay(relay_url.clone()).await.is_err() {
            continue;
        }
        let Ok(Some(relay)) = client.relay(&relay_url).await else {
            continue;
        };
        let Ok(events) = relay
            .fetch_events(filter.clone())
            .timeout(Duration::from_secs(5))
            .policy(ReqExitPolicy::ExitOnEOSE)
            .await
        else {
            continue;
        };
        for event in events {
            if is_ngit_repo_state_event(&event) {
                save_event_in_global_cache(git_repo_path, &event).await?;
            }
        }
    }

    client.disconnect().await;
    Ok(())
}

pub async fn available_update_versions_from_cache(
    git_repo_path: Option<&std::path::Path>,
) -> Result<Option<(String, String)>> {
    let mut events =
        get_event_from_global_cache(git_repo_path, vec![ngit_repo_state_filter()]).await?;
    let coordinate = ngit_repo_coordinate();
    events.retain(|event| {
        event.kind == STATE_KIND
            && event.pubkey == coordinate.public_key
            && event
                .tags
                .identifier()
                .is_some_and(|id| id == coordinate.identifier)
    });
    events.sort_by_key(|event| (event.created_at, event.id));

    let Some(event) = events.last() else {
        return Ok(None);
    };
    let Some(latest) = latest_version_tag(event) else {
        return Ok(None);
    };

    Ok(available_update_versions(
        env!("CARGO_PKG_VERSION"),
        &latest,
    ))
}

#[must_use]
pub fn available_update_versions(current: &str, latest: &str) -> Option<(String, String)> {
    let current = VersionTag::parse(current)?;
    let latest_version = VersionTag::parse(latest)?;

    if latest_version.gt(&current) {
        Some((current.to_string(), latest.to_string()))
    } else {
        None
    }
}

#[must_use]
pub fn latest_version_tag(event: &nostr::Event) -> Option<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| tag.as_slice().first())
        .filter_map(|name| name.strip_prefix("refs/tags/"))
        .filter(|name| !name.ends_with("^{}"))
        .filter_map(|name| VersionTag::parse(name).map(|version| (version, name.to_string())))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, name)| name)
}

#[must_use]
pub fn is_ngit_repo_state_event(event: &nostr::Event) -> bool {
    let coordinate = ngit_repo_coordinate();
    event.kind == STATE_KIND
        && event.pubkey == coordinate.public_key
        && event
            .tags
            .identifier()
            .is_some_and(|id| id == coordinate.identifier)
}

#[must_use]
pub fn version_tag_matches_current(tag: &str, current: &str) -> bool {
    VersionTag::parse(tag).is_some_and(|tag| {
        VersionTag::parse(current).is_some_and(|current| tag.cmp(&current) == Ordering::Equal)
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct VersionTag {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
}

impl VersionTag {
    fn parse(input: &str) -> Option<Self> {
        let input = input.strip_prefix('v').unwrap_or(input);
        let (core, pre) = input
            .split_once('-')
            .map_or((input, None), |(core, pre)| (core, Some(pre.to_string())));
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl std::fmt::Display for VersionTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl Ord for VersionTag {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.major,
            self.minor,
            self.patch,
            self.pre.is_none(),
            self.pre.as_deref(),
        )
            .cmp(&(
                other.major,
                other.minor,
                other.patch,
                other.pre.is_none(),
                other.pre.as_deref(),
            ))
    }
}

impl PartialOrd for VersionTag {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use nostr::{
        EventBuilder,
        event::{FinalizeEvent, Tag},
    };

    use super::*;

    #[test]
    fn picks_highest_semver_tag_from_state_event() {
        let keys = nostr::Keys::generate();
        let event = EventBuilder::new(STATE_KIND, "")
            .tags([
                Tag::parse(["d", "ngit"]).unwrap(),
                Tag::parse([
                    "refs/tags/v2.4.0",
                    "0000000000000000000000000000000000000000",
                ])
                .unwrap(),
                Tag::parse([
                    "refs/tags/v2.6.0",
                    "0000000000000000000000000000000000000000",
                ])
                .unwrap(),
                Tag::parse([
                    "refs/tags/v2.6.0^{}",
                    "0000000000000000000000000000000000000000",
                ])
                .unwrap(),
            ])
            .finalize(&keys)
            .unwrap();

        assert_eq!(latest_version_tag(&event), Some("v2.6.0".to_string()));
    }

    #[test]
    fn current_version_matches_with_or_without_v_prefix() {
        assert!(version_tag_matches_current("v2.5.0", "2.5.0"));
        assert!(version_tag_matches_current("2.5.0", "2.5.0"));
        assert!(!version_tag_matches_current("v2.5.1", "2.5.0"));
    }

    #[test]
    fn update_is_available_only_for_newer_versions() {
        assert_eq!(
            available_update_versions("2.4.3", "v2.5.0"),
            Some(("2.4.3".to_string(), "v2.5.0".to_string()))
        );
        assert_eq!(available_update_versions("2.5.0", "v2.5.0"), None);
        assert_eq!(available_update_versions("2.5.0", "v2.4.3"), None);
    }

    #[test]
    fn detects_update_relays_by_host() {
        assert!(is_version_check_relay(
            &RelayUrl::parse("wss://relay.ngit.dev").unwrap()
        ));
        assert!(is_version_check_relay(
            &RelayUrl::parse("wss://relay.ngit.dev/").unwrap()
        ));
        assert!(is_version_check_relay(
            &RelayUrl::parse("wss://gitnostr.com/some/path").unwrap()
        ));
        assert!(!is_version_check_relay(
            &RelayUrl::parse("wss://example.com").unwrap()
        ));
    }
}
