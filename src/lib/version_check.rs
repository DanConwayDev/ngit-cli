use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

use anyhow::Result;
use console::{Color, Style};
use nostr::prelude::{
    Event, EventId, Filter, RelayUrl, SingleLetterTag, nip01::Coordinate, nip19::Nip19Coordinate,
};
use semver::Version;

use crate::{
    client::{STATE_KIND, get_event_from_global_cache, get_filter_state_events},
    event_ordering,
    software_release::{
        SOFTWARE_APPLICATION_KIND, SOFTWARE_ASSET_KIND, SOFTWARE_RELEASE_KIND, SoftwareAsset,
        SoftwareRelease,
    },
};

pub const UPDATE_RELAY_HOSTS: [&str; 3] = ["relay.ngit.dev", "gitnostr.com", "relay.zapstore.dev"];
pub const NGIT_APPLICATION_IDENTIFIER: &str = "ngit";
const NGIT_REPO_COORDINATE: &str =
    "30617:a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d:ngit";
static UPDATE_NOTICE_CHECKED: AtomicBool = AtomicBool::new(false);
const UPDATE_NOTICE_COLOR: Color = Color::Color256(214);

#[derive(Clone, Debug)]
pub struct AvailableUpdate {
    pub current: String,
    pub version: String,
    pub release: SoftwareRelease,
    pub asset: SoftwareAsset,
}

#[must_use]
pub fn ngit_repo_coordinate() -> Nip19Coordinate {
    Nip19Coordinate {
        coordinate: Coordinate::parse(NGIT_REPO_COORDINATE)
            .expect("hard-coded ngit repository coordinate must parse"),
        relays: vec![],
    }
}

#[must_use]
pub fn ngit_application_coordinate() -> Coordinate {
    Coordinate::new(SOFTWARE_APPLICATION_KIND, ngit_repo_coordinate().public_key)
        .identifier(NGIT_APPLICATION_IDENTIFIER)
}

#[must_use]
pub fn update_relay_urls() -> Vec<String> {
    UPDATE_RELAY_HOSTS
        .iter()
        .map(|host| format!("wss://{host}"))
        .collect()
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
pub fn ngit_repo_state_filter() -> Filter {
    get_filter_state_events(&[ngit_repo_coordinate()].into_iter().collect(), true)
}

#[must_use]
pub fn ngit_release_filters(version: &str) -> Vec<Filter> {
    let Some(version) = parse_version(version) else {
        return Vec::new();
    };
    let normalized = version.to_string();
    [
        format!("{NGIT_APPLICATION_IDENTIFIER}@{normalized}"),
        format!("{NGIT_APPLICATION_IDENTIFIER}@v{normalized}"),
    ]
    .into_iter()
    .map(|identifier| {
        Filter::new()
            .kind(SOFTWARE_RELEASE_KIND)
            .author(ngit_repo_coordinate().public_key)
            .identifier(identifier)
    })
    .collect()
}

fn ngit_release_cache_filter() -> Filter {
    Filter::new()
        .kind(SOFTWARE_RELEASE_KIND)
        .author(ngit_repo_coordinate().public_key)
        .custom_tag(SingleLetterTag::LOWERCASE_I, NGIT_APPLICATION_IDENTIFIER)
}

#[must_use]
pub fn is_update_check_event(event: &Event) -> bool {
    is_ngit_repo_state_event(event)
        || ngit_release_from_event(event).is_some()
        || ngit_asset_from_event(event).is_some()
}

pub async fn background_update_filters_from_cache(
    git_repo_path: Option<&Path>,
) -> Result<Vec<Filter>> {
    let mut filters = vec![ngit_repo_state_filter()];
    let state_events =
        get_event_from_global_cache(git_repo_path, vec![ngit_repo_state_filter()]).await?;
    let Some(candidate) = latest_update_tag(&state_events, env!("CARGO_PKG_VERSION")) else {
        return Ok(filters);
    };

    let release_filters = ngit_release_filters(&candidate);
    let release_events =
        get_event_from_global_cache(git_repo_path, release_filters.clone()).await?;
    filters.extend(release_filters);

    let asset_ids = matching_releases(&release_events, &candidate)
        .into_iter()
        .flat_map(|release| release.assets.into_iter().map(|asset| asset.event_id))
        .collect::<BTreeSet<_>>();
    if !asset_ids.is_empty() {
        filters.push(Filter::new().ids(asset_ids));
    }
    Ok(filters)
}

pub async fn print_update_notice_if_available(git_repo_path: Option<&Path>) -> Result<()> {
    // The notice is advisory, so quiet mode suppresses it for every caller,
    // including `--version`, which bypasses normal CLI parsing.
    if crate::output_mode::is_quiet() {
        return Ok(());
    }
    if UPDATE_NOTICE_CHECKED.swap(true, AtomicOrdering::Relaxed) {
        return Ok(());
    }

    if let Some(update) = available_update_from_cache(git_repo_path).await? {
        let message = format!(
            "ngit v{} is available; you have v{}. Run `ngit update` for details",
            update.version, update.current
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

pub async fn available_update_versions_from_cache(
    git_repo_path: Option<&Path>,
) -> Result<Option<(String, String)>> {
    Ok(available_update_from_cache(git_repo_path)
        .await?
        .map(|update| (update.current, format!("v{}", update.version))))
}

pub async fn available_update_from_cache(
    git_repo_path: Option<&Path>,
) -> Result<Option<AvailableUpdate>> {
    let state_events =
        get_event_from_global_cache(git_repo_path, vec![ngit_repo_state_filter()]).await?;
    let release_events =
        get_event_from_global_cache(git_repo_path, vec![ngit_release_cache_filter()]).await?;
    let asset_ids = latest_ngit_releases(&release_events)
        .into_values()
        .flat_map(|release| release.assets.into_iter().map(|asset| asset.event_id))
        .collect::<BTreeSet<_>>();
    let asset_events = if asset_ids.is_empty() {
        Vec::new()
    } else {
        get_event_from_global_cache(git_repo_path, vec![Filter::new().ids(asset_ids)]).await?
    };
    Ok(resolve_available_update(
        env!("CARGO_PKG_VERSION"),
        &state_events,
        &release_events,
        &asset_events,
        current_platform(),
        current_variant(),
    ))
}

#[must_use]
pub fn resolve_available_update(
    current: &str,
    state_events: &[Event],
    release_events: &[Event],
    asset_events: &[Event],
    platform: Option<&str>,
    variant: Option<&str>,
) -> Option<AvailableUpdate> {
    let current_version = parse_version(current)?;
    let platform = platform?;
    let tag_versions = eligible_tag_versions(state_events, &current_version);
    if tag_versions.is_empty() {
        return None;
    }
    let parsed_assets = asset_events
        .iter()
        .filter_map(ngit_asset_from_event)
        .collect::<Vec<_>>();
    let releases = latest_ngit_releases(release_events);

    tag_versions
        .keys()
        .rev()
        .filter_map(|version| {
            resolve_parsed_release(
                &current_version,
                version,
                &releases,
                &parsed_assets,
                platform,
                variant,
            )
        })
        .next()
}

#[must_use]
pub fn resolve_update_for_version(
    current: &str,
    target: &str,
    release_events: &[Event],
    asset_events: &[Event],
    platform: Option<&str>,
    variant: Option<&str>,
) -> Option<AvailableUpdate> {
    let current = parse_version(current)?;
    let target = parse_version(target)?;
    let platform = platform?;
    let releases = latest_ngit_releases(release_events);
    let assets = asset_events
        .iter()
        .filter_map(ngit_asset_from_event)
        .collect::<Vec<_>>();
    resolve_parsed_release(&current, &target, &releases, &assets, platform, variant)
}

fn resolve_parsed_release(
    current: &Version,
    target: &Version,
    releases: &BTreeMap<Version, SoftwareRelease>,
    assets: &[SoftwareAsset],
    platform: &str,
    variant: Option<&str>,
) -> Option<AvailableUpdate> {
    let release = releases.get(target)?.clone();
    let release_assets = assets
        .iter()
        .filter(|asset| {
            release
                .assets
                .iter()
                .any(|pointer| pointer.event_id == asset.raw_event.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    if !release.validate_assets(&release_assets).is_empty() {
        return None;
    }
    let matching_assets = release_assets
        .into_iter()
        .filter(|asset| {
            has_content_addressed_https_url(asset)
                && asset.application.as_ref().is_some_and(|application| {
                    application.coordinate == ngit_application_coordinate()
                })
                && asset.platforms.iter().any(|value| value == platform)
        })
        .collect::<Vec<_>>();
    let asset = select_asset(matching_assets, variant)?;
    Some(AvailableUpdate {
        current: current.to_string(),
        version: target.to_string(),
        release,
        asset,
    })
}

fn has_content_addressed_https_url(asset: &SoftwareAsset) -> bool {
    let Some(source) = asset.url.as_deref() else {
        return false;
    };
    reqwest::Url::parse(source).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url
                .path()
                .to_ascii_lowercase()
                .contains(&asset.sha256.to_ascii_lowercase())
    })
}

fn select_asset(assets: Vec<SoftwareAsset>, variant: Option<&str>) -> Option<SoftwareAsset> {
    let ranked = assets
        .into_iter()
        .filter_map(|asset| {
            variant_score(asset.variant.as_deref(), variant).map(|score| (score, asset))
        })
        .collect::<Vec<_>>();
    let best_score = ranked.iter().map(|(score, _)| *score).max()?;
    let mut best = ranked
        .into_iter()
        .filter_map(|(score, asset)| (score == best_score).then_some(asset));
    let selected = best.next()?;
    best.next().is_none().then_some(selected)
}

fn variant_score(asset: Option<&str>, requested: Option<&str>) -> Option<u8> {
    match (asset, requested) {
        (None, _) => Some(1),
        (Some(_), None) => Some(1),
        (Some(asset), Some("gnu")) if asset.contains("gnu") || asset.contains("glibc") => Some(2),
        (Some(asset), Some("musl")) if asset.contains("musl") => Some(2),
        (Some(asset), Some(requested)) if asset == requested => Some(2),
        (Some(_), Some(_)) => None,
    }
}

fn matching_releases(events: &[Event], version: &str) -> Vec<SoftwareRelease> {
    let Some(expected) = parse_version(version) else {
        return Vec::new();
    };
    latest_ngit_releases(events)
        .remove(&expected)
        .into_iter()
        .collect()
}

fn latest_ngit_releases(events: &[Event]) -> BTreeMap<Version, SoftwareRelease> {
    let mut releases = BTreeMap::<Version, SoftwareRelease>::new();
    for release in events.iter().filter_map(ngit_release_from_event) {
        let Some(version) = parse_version(&release.version) else {
            continue;
        };
        let should_replace = releases.get(&version).is_none_or(|existing| {
            event_ordering::latest_event([&existing.raw_event, &release.raw_event])
                .is_some_and(|event| event.id == release.raw_event.id)
        });
        if should_replace {
            releases.insert(version, release);
        }
    }
    releases
}

fn ngit_release_from_event(event: &Event) -> Option<SoftwareRelease> {
    let release = SoftwareRelease::parse(event).ok()?;
    (release.raw_event.pubkey == ngit_repo_coordinate().public_key
        && release.application.coordinate == ngit_application_coordinate()
        && release.application_identifier == NGIT_APPLICATION_IDENTIFIER
        && release.channel == "main")
        .then_some(release)
}

fn ngit_asset_from_event(event: &Event) -> Option<SoftwareAsset> {
    let asset = SoftwareAsset::parse(event).ok()?;
    (event.kind == SOFTWARE_ASSET_KIND
        && event.pubkey == ngit_repo_coordinate().public_key
        && asset
            .application
            .as_ref()
            .is_some_and(|application| application.coordinate == ngit_application_coordinate()))
    .then_some(asset)
}

fn eligible_tag_versions(events: &[Event], current: &Version) -> BTreeMap<Version, String> {
    latest_state_event(events)
        .map(|event| {
            version_tags(event)
                .filter(|(version, _)| {
                    version > current && (!current.pre.is_empty() || version.pre.is_empty())
                })
                .collect()
        })
        .unwrap_or_default()
}

#[must_use]
pub fn latest_update_tag(events: &[Event], current: &str) -> Option<String> {
    let current = parse_version(current)?;
    eligible_tag_versions(events, &current)
        .into_iter()
        .next_back()
        .map(|(_, tag)| tag)
}

#[must_use]
pub fn state_contains_version(events: &[Event], version: &str) -> bool {
    let Some(expected) = parse_version(version) else {
        return false;
    };
    latest_state_event(events)
        .is_some_and(|event| version_tags(event).any(|(version, _)| version == expected))
}

#[must_use]
pub fn referenced_asset_ids_for_version(events: &[Event], version: &str) -> BTreeSet<EventId> {
    matching_releases(events, version)
        .into_iter()
        .flat_map(|release| release.assets.into_iter().map(|asset| asset.event_id))
        .collect()
}

fn latest_state_event(events: &[Event]) -> Option<&Event> {
    event_ordering::latest_event(
        events
            .iter()
            .filter(|event| is_ngit_repo_state_event(event)),
    )
}

fn version_tags(event: &Event) -> impl Iterator<Item = (Version, String)> + '_ {
    event
        .tags
        .iter()
        .filter_map(|tag| tag.as_slice().first())
        .filter_map(|name| name.strip_prefix("refs/tags/"))
        .filter(|name| !name.ends_with("^{}"))
        .filter_map(|name| parse_version(name).map(|version| (version, name.to_string())))
}

fn parse_version(input: &str) -> Option<Version> {
    Version::parse(input.strip_prefix('v').unwrap_or(input)).ok()
}

#[must_use]
pub fn available_update_versions(current: &str, latest: &str) -> Option<(String, String)> {
    let current_version = parse_version(current)?;
    let latest_version = parse_version(latest)?;
    if latest_version > current_version
        && (!current_version.pre.is_empty() || latest_version.pre.is_empty())
    {
        Some((current_version.to_string(), latest.to_string()))
    } else {
        None
    }
}

#[must_use]
pub fn latest_version_tag(event: &Event) -> Option<String> {
    version_tags(event)
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, name)| name)
}

#[must_use]
pub fn is_ngit_repo_state_event(event: &Event) -> bool {
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
    parse_version(tag).is_some_and(|tag| {
        parse_version(current).is_some_and(|current| tag.cmp(&current) == Ordering::Equal)
    })
}

#[must_use]
pub const fn current_platform() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("linux-aarch64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("darwin-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("darwin-aarch64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some("windows-aarch64")
    } else {
        None
    }
}

#[must_use]
pub const fn current_variant() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        Some("musl")
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        Some("gnu")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{
        EventBuilder, Keys, Timestamp,
        event::{FinalizeEvent, Tag},
    };

    use super::*;
    use crate::software_release::{
        AddressPointer, AssetInput, ReleaseAssetInput, ReleaseInput, asset_event_builder,
        release_event_builder,
    };

    fn keys() -> Keys {
        Keys::parse("e9e3516c271f554bf696066438aed245c68474a5a86bb04a78c1ba87cb89eb11").unwrap()
    }

    fn state_event(tags: &[&str]) -> Event {
        let mut event_tags = vec![Tag::parse(["d", "ngit"]).unwrap()];
        event_tags.extend(tags.iter().map(|name| {
            Tag::parse([
                format!("refs/tags/{name}"),
                "0000000000000000000000000000000000000000".to_string(),
            ])
            .unwrap()
        }));
        let mut event = EventBuilder::new(STATE_KIND, "")
            .tags(event_tags)
            .finalize(&keys())
            .unwrap();
        event.pubkey = ngit_repo_coordinate().public_key;
        event
    }

    #[test]
    fn picks_highest_semver_tag_from_state_event() {
        let event = state_event(&["v2.4.0", "v2.6.0", "v2.6.0^{}"]);
        assert_eq!(latest_version_tag(&event), Some("v2.6.0".to_string()));
    }

    #[test]
    fn state_replacements_use_the_nip01_lower_id_tiebreak() {
        let first = state_event(&["v3.0.0"]);
        let mut second = state_event(&["v3.0.1"]);
        second.created_at = first.created_at;
        let expected = std::cmp::min(first.id, second.id);

        assert_eq!(latest_state_event(&[first, second]).unwrap().id, expected);
    }

    #[test]
    fn stable_installations_ignore_release_candidates() {
        let events = [state_event(&["v2.6.3", "v3.0.0-rc.3"])];
        assert!(state_contains_version(&events, "3.0.0-rc.3"));
        assert!(!state_contains_version(&events, "3.0.0-rc.4"));
        assert_eq!(latest_update_tag(&events, "2.6.2"), Some("v2.6.3".into()));
        assert_eq!(latest_update_tag(&events, "2.6.3"), None);
        assert_eq!(
            latest_update_tag(&events, "3.0.0-rc.2"),
            Some("v3.0.0-rc.3".into())
        );
    }

    #[test]
    fn prerelease_ordering_uses_semver_rules() {
        let events = [state_event(&["v3.0.0-rc.9", "v3.0.0-rc.10"])];
        assert_eq!(
            latest_update_tag(&events, "3.0.0-rc.8"),
            Some("v3.0.0-rc.10".into())
        );
    }

    #[test]
    fn current_version_matches_with_or_without_v_prefix() {
        assert!(version_tag_matches_current("v2.5.0", "2.5.0"));
        assert!(version_tag_matches_current("2.5.0", "2.5.0"));
        assert!(!version_tag_matches_current("v2.5.1", "2.5.0"));
    }

    #[test]
    fn update_is_available_only_for_newer_eligible_versions() {
        assert_eq!(
            available_update_versions("2.4.3", "v2.5.0"),
            Some(("2.4.3".to_string(), "v2.5.0".to_string()))
        );
        assert_eq!(available_update_versions("2.6.3", "v3.0.0-rc.3"), None);
        assert_eq!(
            available_update_versions("3.0.0-rc.2", "v3.0.0-rc.3"),
            Some(("3.0.0-rc.2".to_string(), "v3.0.0-rc.3".to_string()))
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
            &RelayUrl::parse("wss://relay.zapstore.dev/").unwrap()
        ));
        assert!(!is_version_check_relay(
            &RelayUrl::parse("wss://example.com").unwrap()
        ));
    }

    #[test]
    fn exact_release_filters_accept_prefixed_and_unprefixed_versions() {
        let filters = ngit_release_filters("v3.0.0");
        assert_eq!(filters.len(), 2);
        let serialized = filters
            .into_iter()
            .map(|filter| serde_json::to_value(filter).unwrap())
            .collect::<Vec<_>>();
        assert!(
            serialized
                .iter()
                .any(|filter| filter["#d"] == serde_json::json!(["ngit@3.0.0"]))
        );
        assert!(
            serialized
                .iter()
                .any(|filter| filter["#d"] == serde_json::json!(["ngit@v3.0.0"]))
        );
    }

    #[test]
    fn warning_requires_a_valid_release_and_all_referenced_assets() {
        let state = state_event(&["v3.0.0"]);
        let app = AddressPointer {
            coordinate: ngit_application_coordinate(),
            relay_hint: None,
        };
        let mut asset = asset_event_builder(AssetInput {
            application: Some(app.clone()),
            identifier: "ngit-linux".into(),
            version: "3.0.0+linux.1".into(),
            url: Some(format!("https://cdn.example/{}.tar.gz", "11".repeat(32))),
            filename: Some("ngit.tar.gz".into()),
            mime: "application/gzip".into(),
            sha256: "11".repeat(32),
            size: Some(42),
            platforms: vec!["linux-x86_64".into()],
            variant: Some("glibc-2.17".into()),
            ..AssetInput::default()
        })
        .unwrap()
        .finalize(&keys())
        .unwrap();
        asset.pubkey = ngit_repo_coordinate().public_key;
        let mut parsed_asset = ngit_asset_from_event(&asset).unwrap();
        assert!(has_content_addressed_https_url(&parsed_asset));
        parsed_asset.url = Some("https://downloads.example/ngit.tar.gz".into());
        assert!(!has_content_addressed_https_url(&parsed_asset));
        parsed_asset.url = Some(format!("http://cdn.example/{}.tar.gz", parsed_asset.sha256));
        assert!(!has_content_addressed_https_url(&parsed_asset));
        let mut release = release_event_builder(ReleaseInput {
            application: app,
            version: "3.0.0".into(),
            channel: "main".into(),
            notes: String::new(),
            assets: vec![ReleaseAssetInput {
                event_id: asset.id,
                author: asset.pubkey,
                relay_hint: None,
                platforms: vec!["linux-x86_64".into()],
            }],
            commit: None,
            extra_tags: vec![],
            released_at: Timestamp::now(),
        })
        .unwrap()
        .finalize(&keys())
        .unwrap();
        release.pubkey = ngit_repo_coordinate().public_key;

        assert!(
            resolve_available_update(
                "2.6.3",
                std::slice::from_ref(&state),
                std::slice::from_ref(&release),
                &[],
                Some("linux-x86_64"),
                Some("gnu")
            )
            .is_none()
        );
        let update = resolve_available_update(
            "2.6.3",
            std::slice::from_ref(&state),
            std::slice::from_ref(&release),
            std::slice::from_ref(&asset),
            Some("linux-x86_64"),
            Some("gnu"),
        )
        .unwrap();
        assert_eq!(update.version, "3.0.0");

        let selected = resolve_update_for_version(
            "3.1.0",
            "3.0.0",
            &[release],
            &[asset],
            Some("linux-x86_64"),
            Some("gnu"),
        )
        .unwrap();
        assert_eq!(selected.current, "3.1.0");
        assert_eq!(selected.version, "3.0.0");
    }
}
