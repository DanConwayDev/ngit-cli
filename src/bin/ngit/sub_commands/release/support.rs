use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result, anyhow};
use ngit::{
    client::send_events_with_results,
    event_ordering::latest_event,
    software_release::{
        SOFTWARE_APPLICATION_KIND, SOFTWARE_ASSET_KIND, SOFTWARE_RELEASE_KIND, SoftwareApplication,
        SoftwareAsset, SoftwareRelease, ValidationIssue, release_platforms,
    },
};
use nostr::prelude::{
    Coordinate, Event, EventId, Filter, FromBech32, Kind, PublicKey, RelayUrl, SingleLetterTag,
    ToBech32,
    nip19::{Nip19Coordinate, Nip19Event},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{cli::SignerParams, sub_commands::id_resolver::parse_event_id};

pub(super) const ZAPSTORE_RELAY_URL: &str = "wss://relay.zapstore.dev";

pub(super) use crate::sub_commands::publication::LoginMode;
pub(crate) use crate::sub_commands::publication::{
    PublicationContext as ReleaseContext, PublicationError as ReleaseError, RepositoryJson,
    WarningJson, coded_error, coded_error_with_details, coordinate_key, repository_json,
};

pub(super) async fn load_context_for_write(
    explicit_relays: &[String],
    zapstore_relay: bool,
    auth: SignerParams<'_>,
) -> Result<ReleaseContext> {
    let mut context = ReleaseContext::load_for_write(explicit_relays, auth).await?;
    if let Some(relay) = zapstore_publication_relay(zapstore_relay)? {
        context.add_publication_relay(relay);
    }
    Ok(context)
}

fn zapstore_publication_relay(enabled: bool) -> Result<Option<RelayUrl>> {
    enabled
        .then(|| RelayUrl::parse(ZAPSTORE_RELAY_URL))
        .transpose()
        .context("built-in Zapstore relay URL is invalid")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AssetReuseOption {
    ReleasePublish,
    AssetAdd,
}

impl AssetReuseOption {
    fn flag(self) -> &'static str {
        match self {
            Self::ReleasePublish => "--asset-event",
            Self::AssetAdd => "--event",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OrderedPublicationEvent {
    entity: &'static str,
    event_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublicationBatchResult {
    ordered_events: Vec<OrderedPublicationEvent>,
    relays: Vec<(String, bool)>,
}

impl PublicationBatchResult {
    fn from_events(events: &[Event], relays: Vec<(String, bool)>) -> Self {
        let ordered_events = events
            .iter()
            .map(|event| OrderedPublicationEvent {
                entity: if event.kind == SOFTWARE_APPLICATION_KIND {
                    "application"
                } else if event.kind == SOFTWARE_ASSET_KIND {
                    "asset"
                } else if event.kind == SOFTWARE_RELEASE_KIND {
                    "release"
                } else {
                    "event"
                },
                event_id: event.id.to_hex(),
            })
            .collect();
        Self {
            ordered_events,
            relays,
        }
    }

    pub(super) fn json(&self) -> Value {
        publication_json(self, &[], None)
    }

    fn release_event_id(&self) -> Option<&str> {
        self.ordered_events
            .iter()
            .rev()
            .find(|event| event.entity == "release")
            .map(|event| event.event_id.as_str())
    }
}

fn publication_json(
    publication: &PublicationBatchResult,
    possible_orphan_asset_ids: &[EventId],
    recovery: Option<&str>,
) -> Value {
    json!({
        "ordered_events": publication.ordered_events.iter().map(|event| json!({
            "entity": event.entity,
            "event_id": event.event_id,
        })).collect::<Vec<_>>(),
        "relays": publication.relays.iter().map(|(url, complete)| json!({
            "url": url,
            "status": if *complete { "complete" } else { "incomplete" },
            "message": if *complete {
                Value::Null
            } else {
                json!("the relay did not acknowledge the complete ordered batch; one or more leading events may still be present")
            },
        })).collect::<Vec<_>>(),
        "possible_orphan_asset_ids": possible_orphan_asset_ids
            .iter()
            .map(EventId::to_hex)
            .collect::<Vec<_>>(),
        "recovery": recovery,
    })
}

fn publication_recovery(
    publication: &PublicationBatchResult,
    reuse_option: AssetReuseOption,
) -> String {
    let release_id = publication.release_event_id().unwrap_or("<missing>");
    let asset_ids = publication
        .ordered_events
        .iter()
        .filter(|event| event.entity == "asset")
        .map(|event| event.event_id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "inspect exact release event {release_id} and asset events [{asset_ids}]. Then reuse any visible possible orphan asset with `{}`; only rerun after the observed state determines that the command is safe",
        reuse_option.flag()
    )
}

fn publication_failure_message(
    publication: &PublicationBatchResult,
    possible_orphan_asset_ids: &[EventId],
    recovery: &str,
) -> String {
    let ordered_events = publication
        .ordered_events
        .iter()
        .map(|event| format!("{} {}", event.entity, event.event_id))
        .collect::<Vec<_>>()
        .join(", ");
    let possible_orphans = if possible_orphan_asset_ids.is_empty() {
        "none".to_owned()
    } else {
        possible_orphan_asset_ids
            .iter()
            .map(EventId::to_hex)
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "no relay acknowledged the complete ordered publication batch\nordered event IDs: {ordered_events}\npossible orphan asset IDs: {possible_orphans}\nrecovery: {recovery}"
    )
}

impl ReleaseContext {
    pub(super) fn ordered_repo_coordinates(&self) -> Vec<ngit::software_release::AddressPointer> {
        let relay_hint = self.repo_ref.relays.first().map(ToString::to_string);
        self.repo_ref
            .members_for_announcement_tags()
            .into_iter()
            .map(|author| ngit::software_release::AddressPointer {
                coordinate: Coordinate::new(Kind::GitRepoAnnouncement, author)
                    .identifier(self.repo_ref.identifier.clone()),
                relay_hint: relay_hint.clone(),
            })
            .collect()
    }

    pub(super) fn application_is_linked(&self, application: &SoftwareApplication) -> bool {
        let repository_coordinates = self.repo_coordinate_keys();
        application
            .repository_coordinates
            .iter()
            .any(|pointer| repository_coordinates.contains(&coordinate_key(&pointer.coordinate)))
    }

    pub(super) fn application_is_fully_linked(&self, application: &SoftwareApplication) -> bool {
        let linked: BTreeSet<String> = application
            .repository_coordinates
            .iter()
            .map(|pointer| coordinate_key(&pointer.coordinate))
            .collect();
        self.repo_coordinate_keys().is_subset(&linked)
    }

    pub(super) fn application_is_trusted(&self, application: &SoftwareApplication) -> bool {
        self.repo_ref
            .is_authorized_maintainer(&application.raw_event.pubkey)
            && self.application_is_linked(application)
    }

    pub(super) fn authority(&self, application: &SoftwareApplication) -> AuthorityJson {
        let current_signer = self.current_signer();
        let is_current_maintainer =
            current_signer.is_some_and(|signer| self.repo_ref.is_authorized_maintainer(&signer));
        let linked = self.application_is_linked(application);
        let author_matches = current_signer == Some(application.raw_event.pubkey);
        let blocker = if current_signer.is_none() {
            Some("not_logged_in")
        } else if !is_current_maintainer {
            Some("not_repository_maintainer")
        } else if !linked {
            Some("application_not_linked")
        } else if !author_matches {
            Some("application_author_mismatch")
        } else {
            None
        };
        AuthorityJson {
            current_signer: current_signer.map(|key| key.to_hex()),
            application_author: Some(application.raw_event.pubkey.to_hex()),
            is_current_maintainer,
            application_linked: linked,
            can_publish: blocker.is_none(),
            blocker: blocker.map(str::to_owned),
        }
    }

    pub(super) fn require_application_author(
        &self,
        application: &SoftwareApplication,
    ) -> Result<()> {
        let authority = self.authority(application);
        if authority.can_publish {
            return Ok(());
        }
        let code = authority.blocker.as_deref().unwrap_or("publication_failed");
        let code = match code {
            "not_logged_in" => "not_logged_in",
            "not_repository_maintainer" => "not_repository_maintainer",
            "application_not_linked" => "application_not_linked",
            "application_author_mismatch" => "application_author_mismatch",
            _ => "publication_failed",
        };
        let signer = authority
            .current_signer
            .as_deref()
            .unwrap_or("not logged in");
        Err(coded_error_with_details(
            code,
            if code == "application_author_mismatch" {
                format!(
                    "only application author {} can publish; current signer is {signer}",
                    application.raw_event.pubkey.to_bech32()?
                )
            } else {
                format!(
                    "cannot publish application {}: {code}",
                    application.identifier
                )
            },
            json!({
                "current_signer": authority.current_signer,
                "required_author": application.raw_event.pubkey.to_hex(),
            }),
        ))
    }

    pub(super) fn require_owner_maintainer(&self, application: &SoftwareApplication) -> Result<()> {
        let signer = self
            .current_signer()
            .ok_or_else(|| coded_error("not_logged_in", "nostr account required"))?;
        if !self.repo_ref.is_authorized_maintainer(&signer) {
            return Err(coded_error(
                "not_repository_maintainer",
                "the active signer is not a current repository maintainer",
            ));
        }
        if signer != application.raw_event.pubkey {
            return Err(coded_error_with_details(
                "application_author_mismatch",
                format!(
                    "only application author {} can edit it; current signer is {}",
                    application.raw_event.pubkey.to_bech32()?,
                    signer.to_bech32()?
                ),
                json!({
                    "current_signer": signer.to_hex(),
                    "required_author": application.raw_event.pubkey.to_hex(),
                }),
            ));
        }
        Ok(())
    }

    pub(super) async fn publish_batch(
        &self,
        events: Vec<Event>,
        possible_orphan_asset_ids: &[EventId],
        reuse_option: AssetReuseOption,
        json_output: bool,
    ) -> Result<PublicationBatchResult> {
        let (user_write, repo_relays) = self.publication_relays();
        let results = send_events_with_results(
            &self.client,
            Some(self.git_repo_path()?),
            events.clone(),
            user_write,
            repo_relays,
            !json_output,
            json_output,
        )
        .await?;
        let publication = PublicationBatchResult::from_events(&events, results);
        if !publication.relays.iter().any(|(_, accepted)| *accepted) {
            let recovery = publication_recovery(&publication, reuse_option);
            let message =
                publication_failure_message(&publication, possible_orphan_asset_ids, &recovery);
            return Err(coded_error_with_details(
                "publication_failed",
                message,
                publication_json(&publication, possible_orphan_asset_ids, Some(&recovery)),
            ));
        }
        Ok(publication)
    }
}

fn tag_value(event: &Event, name: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [tag_name, value, ..] if tag_name == name && !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

fn latest_addressable(events: Vec<Event>) -> Vec<Event> {
    let mut by_address: BTreeMap<(PublicKey, String), Vec<Event>> = BTreeMap::new();
    for event in events {
        if let Some(identifier) = tag_value(&event, "d") {
            by_address
                .entry((event.pubkey, identifier))
                .or_default()
                .push(event);
        }
    }
    by_address
        .into_values()
        .filter_map(|events| latest_event(events.iter()).cloned())
        .collect()
}

pub(super) async fn load_applications(
    context: &mut ReleaseContext,
    authors: Vec<PublicKey>,
    strict: bool,
) -> Result<Vec<SoftwareApplication>> {
    let mut filter = Filter::new().kind(SOFTWARE_APPLICATION_KIND);
    if !authors.is_empty() {
        filter = filter.authors(authors);
    }
    let events = latest_addressable(context.query(vec![filter], strict).await?);
    let mut applications = Vec::new();
    for event in events {
        match SoftwareApplication::parse(&event) {
            Ok(application) => applications.push(application),
            Err(error) => context.warnings.push(
                WarningJson::new("invalid_application", error.to_string())
                    .with_details(json!({ "event_id": event.id.to_hex() })),
            ),
        }
    }
    applications.sort_by(|left, right| {
        left.identifier
            .cmp(&right.identifier)
            .then_with(|| left.raw_event.pubkey.cmp(&right.raw_event.pubkey))
    });
    Ok(applications)
}

pub(super) async fn load_trusted_linked_applications(
    context: &mut ReleaseContext,
) -> Result<Vec<SoftwareApplication>> {
    let maintainers = context.repo_ref.confirmed_maintainers();
    let applications = load_applications(context, maintainers, false).await?;
    Ok(applications
        .into_iter()
        .filter(|application| context.application_is_trusted(application))
        .collect())
}

pub(super) async fn load_releases(
    context: &mut ReleaseContext,
    applications: &[SoftwareApplication],
    strict: bool,
) -> Result<Vec<SoftwareRelease>> {
    if applications.is_empty() {
        return Ok(Vec::new());
    }
    let authors: BTreeSet<PublicKey> = applications
        .iter()
        .map(|application| application.raw_event.pubkey)
        .collect();
    let identifiers: BTreeSet<String> = applications
        .iter()
        .map(|application| application.identifier.clone())
        .collect();
    for author in &authors {
        context.add_author_relays(*author).await?;
    }
    let filter = Filter::new()
        .kind(SOFTWARE_RELEASE_KIND)
        .authors(authors)
        .custom_tags(SingleLetterTag::LOWERCASE_I, identifiers);
    let events = latest_addressable(context.query(vec![filter], strict).await?);
    let application_coordinates: HashSet<String> = applications
        .iter()
        .map(|application| coordinate_key(&application.coordinate()))
        .collect();
    let mut releases = Vec::new();
    for event in events {
        match SoftwareRelease::parse(&event) {
            Ok(release)
                if application_coordinates
                    .contains(&coordinate_key(&release.application.coordinate)) =>
            {
                releases.push(release);
            }
            Ok(_) => context.warnings.push(
                WarningJson::new(
                    "release_application_mismatch",
                    "release does not reference one of the selected applications",
                )
                .with_details(json!({ "event_id": event.id.to_hex() })),
            ),
            Err(error) => context.warnings.push(
                WarningJson::new("invalid_release", error.to_string())
                    .with_details(json!({ "event_id": event.id.to_hex() })),
            ),
        }
    }
    releases.sort_by(|left, right| {
        right
            .raw_event
            .created_at
            .cmp(&left.raw_event.created_at)
            .then_with(|| {
                left.application_identifier
                    .cmp(&right.application_identifier)
            })
            .then_with(|| left.version.cmp(&right.version))
            .then_with(|| left.raw_event.id.cmp(&right.raw_event.id))
    });
    Ok(releases)
}

pub(super) async fn load_assets(
    context: &mut ReleaseContext,
    releases: &[&SoftwareRelease],
    strict: bool,
) -> Result<Vec<SoftwareAsset>> {
    let ids: BTreeSet<EventId> = releases
        .iter()
        .flat_map(|release| release.assets.iter().map(|asset| asset.event_id))
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Fetch by immutable ID without a kind constraint so a referenced event of
    // the wrong kind is retained in cache and can be reported as invalid
    // rather than indistinguishable from a missing event.
    let filter = Filter::new().ids(ids);
    let events = context.query(vec![filter], strict).await?;
    let mut assets = Vec::new();
    for event in events {
        match SoftwareAsset::parse(&event) {
            Ok(asset) => assets.push(asset),
            Err(error) => context.warnings.push(
                WarningJson::new("invalid_asset_metadata", error.to_string())
                    .with_details(json!({ "event_id": event.id.to_hex() })),
            ),
        }
    }
    assets.sort_by_key(|asset| asset.raw_event.id);
    assets.dedup_by_key(|asset| asset.raw_event.id);
    Ok(assets)
}

pub(super) fn resolve_application<'a>(
    applications: &'a [SoftwareApplication],
    selector: &str,
) -> Result<&'a SoftwareApplication> {
    let selector = selector.trim();
    let explicit = Nip19Coordinate::from_bech32(selector)
        .ok()
        .map(|pointer| pointer.coordinate)
        .or_else(|| Coordinate::parse(selector).ok());
    let matches: Vec<&SoftwareApplication> = if let Some(coordinate) = explicit {
        if coordinate.kind != SOFTWARE_APPLICATION_KIND {
            return Err(coded_error(
                "application_not_found",
                "selector is not a software application coordinate",
            ));
        }
        applications
            .iter()
            .filter(|application| application.coordinate() == coordinate)
            .collect()
    } else {
        applications
            .iter()
            .filter(|application| application.identifier == selector)
            .collect()
    };
    match matches.as_slice() {
        [application] => Ok(application),
        [] => Err(coded_error(
            "application_not_found",
            format!("software application {selector:?} was not found"),
        )),
        _ => Err(coded_error_with_details(
            "ambiguous_selector",
            format!("application selector {selector:?} matches multiple authors"),
            json!({
                "coordinates": matches
                    .iter()
                    .map(|application| coordinate_key(&application.coordinate()))
                    .collect::<Vec<_>>()
            }),
        )),
    }
}

pub(super) fn resolve_release<'a>(
    releases: &'a [SoftwareRelease],
    applications: &'a [SoftwareApplication],
    selector: &str,
    app_selector: Option<&str>,
) -> Result<&'a SoftwareRelease> {
    let selector = selector.trim();
    if let Ok(pointer) = Nip19Coordinate::from_bech32(selector) {
        if pointer.coordinate.kind != SOFTWARE_RELEASE_KIND {
            return Err(coded_error(
                "release_not_found",
                "selector is not a software release coordinate",
            ));
        }
        return unique_release(
            releases
                .iter()
                .filter(|release| release.coordinate() == pointer.coordinate)
                .collect(),
            selector,
        );
    }
    if let Ok(event_id) = parse_event_id(selector) {
        return unique_release(
            releases
                .iter()
                .filter(|release| release.raw_event.id == event_id)
                .collect(),
            selector,
        );
    }

    let matches = if let Some(app_selector) = app_selector {
        let application = resolve_application(applications, app_selector)?;
        releases
            .iter()
            .filter(|release| {
                release.application.coordinate == application.coordinate()
                    && (release.version == selector || release.identifier == selector)
            })
            .collect()
    } else {
        releases
            .iter()
            .filter(|release| release.identifier == selector || release.version == selector)
            .collect()
    };
    unique_release(matches, selector)
}

#[allow(clippy::needless_pass_by_value)]
fn unique_release<'a>(
    matches: Vec<&'a SoftwareRelease>,
    selector: &str,
) -> Result<&'a SoftwareRelease> {
    match matches.as_slice() {
        [release] => Ok(release),
        [] => Err(coded_error(
            "release_not_found",
            format!("software release {selector:?} was not found"),
        )),
        _ => Err(coded_error_with_details(
            "ambiguous_selector",
            format!("release selector {selector:?} is ambiguous; provide --app"),
            json!({
                "coordinates": matches
                    .iter()
                    .map(|release| coordinate_key(&release.coordinate()))
                    .collect::<Vec<_>>()
            }),
        )),
    }
}

pub(super) fn resolve_asset<'a>(
    assets: &'a [SoftwareAsset],
    selector: &str,
) -> Result<&'a SoftwareAsset> {
    let matches: Vec<&SoftwareAsset> = if let Ok(event_id) = parse_event_id(selector) {
        assets
            .iter()
            .filter(|asset| asset.raw_event.id == event_id)
            .collect()
    } else {
        assets
            .iter()
            .filter(|asset| asset.filename.as_deref() == Some(selector))
            .collect()
    };
    match matches.as_slice() {
        [asset] => Ok(asset),
        [] => Err(coded_error(
            "asset_not_found",
            format!("software asset {selector:?} was not found"),
        )),
        _ => Err(coded_error(
            "ambiguous_selector",
            format!("asset filename {selector:?} is not unique"),
        )),
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct AuthorityJson {
    pub current_signer: Option<String>,
    pub application_author: Option<String>,
    pub is_current_maintainer: bool,
    pub application_linked: bool,
    pub can_publish: bool,
    pub blocker: Option<String>,
}

pub(super) struct CommandOutput {
    pub command: &'static str,
    pub repository: RepositoryJson,
    pub authority: AuthorityJson,
    pub warnings: Vec<WarningJson>,
    pub result: Value,
    pub human: String,
}

impl CommandOutput {
    pub(super) fn new(
        command: &'static str,
        context: &mut ReleaseContext,
        authority: AuthorityJson,
        result: Value,
        human: String,
    ) -> Self {
        Self {
            command,
            repository: repository_json(context),
            authority,
            warnings: std::mem::take(&mut context.warnings),
            result,
            human,
        }
    }
}

impl AuthorityJson {
    pub(super) fn unknown(current_signer: Option<PublicKey>) -> Self {
        Self {
            current_signer: current_signer.map(|key| key.to_hex()),
            application_author: None,
            is_current_maintainer: false,
            application_linked: false,
            can_publish: false,
            blocker: None,
        }
    }
}

pub(super) fn application_json(
    context: &ReleaseContext,
    application: &SoftwareApplication,
) -> Value {
    json!({
        "coordinate": coordinate_key(&application.coordinate()),
        "event_id": application.raw_event.id.to_hex(),
        "event_id_bech32": event_id_bech32(&application.raw_event),
        "author": application.raw_event.pubkey.to_hex(),
        "author_npub": application.raw_event.pubkey.to_bech32().ok(),
        "identifier": application.identifier,
        "name": application.name,
        "summary": application.summary,
        "description": application.description,
        "icon": application.icon,
        "images": application.images,
        "topics": application.topics,
        "communities": application.communities,
        "website": application.website,
        "repository": application.repository,
        "platforms": application.platforms,
        "license": application.license,
        "repository_coordinates": application.repository_coordinates.iter()
            .map(|pointer| coordinate_key(&pointer.coordinate)).collect::<Vec<_>>(),
        "linked_to_current_repository": context.application_is_linked(application),
        "fully_linked_to_current_repository": context.application_is_fully_linked(application),
        "trusted": context.application_is_trusted(application),
        "can_publish": context.authority(application).can_publish,
        "created_at": application.raw_event.created_at.as_secs(),
        "validation": Vec::<ValidationIssue>::new(),
        "raw_event": application.raw_event,
    })
}

pub(super) fn release_json(release: &SoftwareRelease, assets: &[SoftwareAsset]) -> Value {
    json!({
        "coordinate": coordinate_key(&release.coordinate()),
        "event_id": release.raw_event.id.to_hex(),
        "event_id_bech32": event_id_bech32(&release.raw_event),
        "author": release.raw_event.pubkey.to_hex(),
        "author_npub": release.raw_event.pubkey.to_bech32().ok(),
        "application_coordinate": coordinate_key(&release.application.coordinate),
        "application_identifier": release.application_identifier,
        "version": release.version,
        "channel": release.channel,
        "released_at": release.raw_event.created_at.as_secs(),
        "notes": release.notes,
        "commit": release.commit,
        "asset_ids": release.assets.iter().map(|asset| asset.event_id.to_hex()).collect::<Vec<_>>(),
        "published_platforms": release.platforms,
        "derived_platforms": release_platforms(assets.iter()),
        "validation": release.validate_assets(assets),
        "raw_event": release.raw_event,
    })
}

pub(super) fn asset_json(asset: &SoftwareAsset) -> Value {
    json!({
        "event_id": asset.raw_event.id.to_hex(),
        "event_id_bech32": event_id_bech32(&asset.raw_event),
        "author": asset.raw_event.pubkey.to_hex(),
        "author_npub": asset.raw_event.pubkey.to_bech32().ok(),
        "application_coordinate": asset.application.as_ref()
            .map(|application| coordinate_key(&application.coordinate)),
        "identifier": asset.identifier,
        "version": asset.version,
        "url": asset.url,
        "filename": asset.filename,
        "mime": asset.mime,
        "sha256": asset.sha256,
        "size": asset.size.map(|size| size.to_string()),
        "platforms": asset.platforms,
        "min_platform_version": asset.min_platform_version,
        "target_platform_version": asset.target_platform_version,
        "supported_nips": asset.supported_nips,
        "variant": asset.variant,
        "commit": asset.commit,
        "min_allowed_version": asset.min_allowed_version,
        "android": {
            "version_code": asset.version_code.map(|value| value.to_string()),
            "min_allowed_version_code": asset.min_allowed_version_code.map(|value| value.to_string()),
            "certificate_sha256": asset.apk_certificate_hashes,
        },
        "original_url": asset.original_url,
        "resolution": "resolved",
        "verification": null,
        "validation": Vec::<ValidationIssue>::new(),
        "raw_event": asset.raw_event,
    })
}

pub(super) fn application_for_release<'a>(
    applications: &'a [SoftwareApplication],
    release: &SoftwareRelease,
) -> Result<&'a SoftwareApplication> {
    applications
        .iter()
        .find(|application| application.coordinate() == release.application.coordinate)
        .ok_or_else(|| anyhow!("release application was not resolved"))
}

fn event_id_bech32(event: &Event) -> Option<String> {
    Nip19Event::new(event.id)
        .author(event.pubkey)
        .kind(event.kind)
        .to_bech32()
        .ok()
}

#[cfg(test)]
mod tests {
    use nostr::prelude::EventId;

    use super::{
        AssetReuseOption, OrderedPublicationEvent, PublicationBatchResult,
        publication_failure_message, publication_json, publication_recovery,
        zapstore_publication_relay,
    };

    #[test]
    fn zapstore_relay_is_an_optional_publication_target() {
        assert!(zapstore_publication_relay(false).unwrap().is_none());
        assert_eq!(
            zapstore_publication_relay(true).unwrap().unwrap().as_str(),
            super::ZAPSTORE_RELAY_URL
        );
    }

    #[test]
    fn publication_json_reports_only_ordered_batch_outcomes() {
        let orphan = EventId::from_hex(&"22".repeat(32)).unwrap();
        let publication = publication_fixture();

        let value = publication_json(
            &publication,
            &[orphan],
            Some("inspect observed state before retrying"),
        );

        assert_eq!(value["ordered_events"][0]["entity"], "asset");
        assert!(value["ordered_events"][0].get("relays").is_none());
        assert_eq!(value["relays"][0]["status"], "complete");
        assert_eq!(value["relays"][1]["status"], "incomplete");
        assert_eq!(value["possible_orphan_asset_ids"][0], orphan.to_hex());
        assert_eq!(value["recovery"], "inspect observed state before retrying");
    }

    #[test]
    fn publication_failure_names_ids_and_safe_reuse_option() {
        let orphan = EventId::from_hex(&"22".repeat(32)).unwrap();
        let publication = publication_fixture();
        let recovery = publication_recovery(&publication, AssetReuseOption::ReleasePublish);
        let message = publication_failure_message(&publication, &[orphan], &recovery);

        assert!(message.contains(&"11".repeat(32)));
        assert!(message.contains(&"22".repeat(32)));
        assert!(message.contains(&"33".repeat(32)));
        assert!(message.contains("--asset-event"));
        assert!(message.contains("only rerun after the observed state"));

        let asset_add_recovery = publication_recovery(&publication, AssetReuseOption::AssetAdd);
        assert!(asset_add_recovery.contains("--event"));
        assert!(!asset_add_recovery.contains("--asset-event"));
    }

    fn publication_fixture() -> PublicationBatchResult {
        PublicationBatchResult {
            ordered_events: vec![
                OrderedPublicationEvent {
                    entity: "asset",
                    event_id: "11".repeat(32),
                },
                OrderedPublicationEvent {
                    entity: "asset",
                    event_id: "22".repeat(32),
                },
                OrderedPublicationEvent {
                    entity: "release",
                    event_id: "33".repeat(32),
                },
            ],
            relays: vec![
                ("wss://complete.example".to_owned(), true),
                ("wss://incomplete.example".to_owned(), false),
            ],
        }
    }
}
