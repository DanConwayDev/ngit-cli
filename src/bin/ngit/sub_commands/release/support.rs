use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    sync::Arc,
};

use anyhow::{Context, Result};
use ngit::{
    client::{
        Client, Connect, Params, fetch_filters_to_local_cache, fetching_with_report,
        get_events_from_local_cache, get_repo_ref_from_cache,
    },
    event_ordering::latest_event,
    git::{Repo, RepoActions},
    login::{self, existing::load_existing_login, user::UserRef},
    repo_ref::{RepoRef, get_resolved_repo_coordinate_when_remote_unknown},
    software_release::{SOFTWARE_APPLICATION_KIND, SoftwareApplication, ValidationIssue},
};
use nostr::prelude::{
    Coordinate, Event, Filter, Kind, PublicKey, RelayUrl, ToBech32, nip19::Nip19Coordinate,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::{Cli, extract_signer_cli_arguments};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LoginMode {
    Optional,
    Required,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WarningJson {
    pub code: String,
    pub message: String,
    pub details: Value,
}

impl WarningJson {
    pub(super) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: json!({}),
        }
    }

    pub(super) fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

#[derive(Debug)]
pub(super) struct ReleaseError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReleaseError {}

pub(super) fn coded_error(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    ReleaseError {
        code,
        message: message.into(),
        details: json!({}),
    }
    .into()
}

pub(super) fn coded_error_with_details(
    code: &'static str,
    message: impl Into<String>,
    details: Value,
) -> anyhow::Error {
    ReleaseError {
        code,
        message: message.into(),
        details,
    }
    .into()
}

pub(super) struct ReleaseContext {
    pub git_repo: Repo,
    pub client: Client,
    pub selected_coordinate: Nip19Coordinate,
    pub repo_ref: RepoRef,
    pub user_ref: Option<UserRef>,
    pub discovery_relays: Vec<RelayUrl>,
    pub offline: bool,
    pub warnings: Vec<WarningJson>,
}

impl ReleaseContext {
    pub(super) async fn load(
        cli: &Cli,
        offline: bool,
        explicit_relays: &[String],
        login_mode: LoginMode,
    ) -> Result<Self> {
        let git_repo = Repo::discover().context("failed to find a git repository")?;
        let git_repo_path = git_repo.get_path()?;
        let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
        let selected = get_resolved_repo_coordinate_when_remote_unknown(&git_repo, &client).await?;
        if !offline {
            fetching_with_report(git_repo_path, &client, &selected.coordinate).await?;
        }
        let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &selected.coordinate).await?;

        let signer_info = extract_signer_cli_arguments(cli)?;
        let login = match login_mode {
            LoginMode::Optional => load_existing_login(
                &Some(&git_repo),
                &signer_info,
                &cli.password,
                &None,
                Some(&client),
                true,
                false,
                false,
            )
            .await
            .ok(),
            LoginMode::Required => Some(
                login::login_or_signup(
                    &Some(&git_repo),
                    &signer_info,
                    &cli.password,
                    Some(&client),
                    true,
                )
                .await?,
            ),
        };
        let user_ref = if let Some((signer, user_ref, _)) = login {
            client.set_signer(Arc::clone(&signer)).await;
            Some(user_ref)
        } else {
            None
        };

        let mut discovery_relays = repo_ref.relays.clone();
        discovery_relays.extend(parse_relays(explicit_relays)?);
        if let Some(user_ref) = &user_ref {
            discovery_relays.extend(parse_relays(&user_ref.relays.read())?);
            discovery_relays.extend(parse_relays(&user_ref.relays.write())?);
        }
        discovery_relays.extend(parse_relays(client.get_relay_default_set())?);
        dedup_relays(&mut discovery_relays);

        Ok(Self {
            git_repo,
            client,
            selected_coordinate: selected.coordinate,
            repo_ref,
            user_ref,
            discovery_relays,
            offline,
            warnings: Vec::new(),
        })
    }

    pub(super) fn git_repo_path(&self) -> Result<&std::path::Path> {
        self.git_repo.get_path()
    }

    pub(super) fn current_signer(&self) -> Option<PublicKey> {
        self.user_ref.as_ref().map(|user| user.public_key)
    }

    pub(super) fn repo_coordinate_keys(&self) -> BTreeSet<String> {
        self.repo_ref
            .maintainers_for_announcement_tags()
            .into_iter()
            .map(|author| {
                coordinate_key(
                    &Coordinate::new(Kind::GitRepoAnnouncement, author)
                        .identifier(self.repo_ref.identifier.clone()),
                )
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
            .maintainers
            .contains(&application.raw_event.pubkey)
            && self.application_is_linked(application)
    }

    pub(super) fn authority(&self, application: &SoftwareApplication) -> AuthorityJson {
        let current_signer = self.current_signer();
        let is_current_maintainer =
            current_signer.is_some_and(|signer| self.repo_ref.maintainers.contains(&signer));
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

    pub(super) async fn add_author_relays(&mut self, author: PublicKey) -> Result<()> {
        if let Ok(user) =
            ngit::login::user::get_user_ref_from_cache(Some(self.git_repo_path()?), &author).await
        {
            self.discovery_relays
                .extend(parse_relays(&user.relays.read())?);
            self.discovery_relays
                .extend(parse_relays(&user.relays.write())?);
            dedup_relays(&mut self.discovery_relays);
        }
        Ok(())
    }

    pub(super) async fn query(&mut self, filters: Vec<Filter>) -> Result<Vec<Event>> {
        if !self.offline {
            let results = fetch_filters_to_local_cache(
                &self.client,
                self.git_repo_path()?,
                &self.discovery_relays,
                &filters,
            )
            .await;
            let failed: Vec<String> = results
                .iter()
                .filter_map(|(relay, result)| result.as_ref().err().map(|_| relay.to_string()))
                .collect();
            if !failed.is_empty() {
                self.warnings.push(
                    WarningJson::new(
                        "relay_discovery_incomplete",
                        "some relays could not be queried; results may be incomplete",
                    )
                    .with_details(json!({ "relays": failed })),
                );
            }
        }
        get_events_from_local_cache(self.git_repo_path()?, filters).await
    }
}

fn parse_relays(values: &[String]) -> Result<Vec<RelayUrl>> {
    values
        .iter()
        .map(|value| RelayUrl::parse(value).with_context(|| format!("invalid relay URL {value:?}")))
        .collect()
}

fn dedup_relays(relays: &mut Vec<RelayUrl>) {
    let mut seen = HashSet::new();
    relays.retain(|relay| seen.insert(relay.to_string().trim_end_matches('/').to_string()));
}

pub(super) fn coordinate_key(coordinate: &Coordinate) -> String {
    format!(
        "{}:{}:{}",
        coordinate.kind.as_u16(),
        coordinate.public_key.to_hex(),
        coordinate.identifier
    )
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
) -> Result<Vec<SoftwareApplication>> {
    let mut filter = Filter::new().kind(SOFTWARE_APPLICATION_KIND);
    if !authors.is_empty() {
        filter = filter.authors(authors);
    }
    let events = latest_addressable(context.query(vec![filter]).await?);
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

pub(super) fn resolve_application<'a>(
    applications: &'a [SoftwareApplication],
    selector: &str,
) -> Result<&'a SoftwareApplication> {
    use nostr::prelude::FromBech32;

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

#[derive(Clone, Debug, Serialize)]
pub(super) struct RepositoryJson {
    pub selected_coordinate: String,
    pub coordinates: Vec<String>,
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

pub(super) fn repository_json(context: &ReleaseContext) -> RepositoryJson {
    RepositoryJson {
        selected_coordinate: coordinate_key(&context.selected_coordinate.coordinate),
        coordinates: context.repo_coordinate_keys().into_iter().collect(),
    }
}

pub(super) fn application_json(
    context: &ReleaseContext,
    application: &SoftwareApplication,
) -> Value {
    json!({
        "coordinate": coordinate_key(&application.coordinate()),
        "event_id": application.raw_event.id.to_hex(),
        "event_id_bech32": application.raw_event.id.to_bech32().ok(),
        "author": application.raw_event.pubkey.to_hex(),
        "author_npub": application.raw_event.pubkey.to_bech32().ok(),
        "identifier": application.identifier,
        "name": application.name,
        "summary": application.summary,
        "description": application.description,
        "icon": application.icon,
        "images": application.images,
        "topics": application.topics,
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
