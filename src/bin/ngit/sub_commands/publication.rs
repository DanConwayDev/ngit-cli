use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    sync::Arc,
};

use anyhow::{Context, Result};
use ngit::{
    NgitSigner,
    blossom::{BlossomServerOutcome, summarize_blossom_replication},
    client::{
        Client, Connect, Params, fetch_filters_to_local_cache, fetching_with_report,
        fetching_without_summary, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions},
    login::{self, existing::load_existing_login, user::UserRef},
    repo_ref::{RepoRef, get_resolved_repo_coordinate_when_remote_unknown},
};
use nostr::prelude::{
    Coordinate, Event, Filter, Kind, PublicKey, RelayUrl, nip19::Nip19Coordinate,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::SignerParams;

mod blossom_progress;

pub(crate) use blossom_progress::BlossomUploadProgress;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoginMode {
    Optional,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryPolicy {
    Discovery,
    AtLeastOneDiscoveryRoute,
    PublicationPreflight,
    RepositoryPublicationPreflight,
    AccountPublicationPreflight,
}

impl QueryPolicy {
    fn is_publication_preflight(self) -> bool {
        matches!(
            self,
            Self::PublicationPreflight
                | Self::RepositoryPublicationPreflight
                | Self::AccountPublicationPreflight
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RelayThresholdGroup {
    label: &'static str,
    relays: Vec<RelayUrl>,
    required: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RelayThresholdStatus {
    label: &'static str,
    completed: usize,
    total: usize,
    required: usize,
    unavailable: Vec<String>,
}

impl RelayThresholdStatus {
    fn met(&self) -> bool {
        self.completed >= self.required
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WarningJson {
    pub code: String,
    pub message: String,
    pub details: Value,
}

impl WarningJson {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: json!({}),
        }
    }

    pub(crate) fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

#[derive(Debug)]
pub(crate) struct PublicationError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PublicationError {}

pub(crate) fn coded_error(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    PublicationError {
        code,
        message: message.into(),
        details: json!({}),
    }
    .into()
}

pub(crate) fn coded_error_with_details(
    code: &'static str,
    message: impl Into<String>,
    details: Value,
) -> anyhow::Error {
    PublicationError {
        code,
        message: message.into(),
        details,
    }
    .into()
}

pub(crate) struct PublicationContext {
    pub git_repo: Repo,
    pub client: Client,
    pub selected_coordinate: Nip19Coordinate,
    pub repo_ref: RepoRef,
    pub signer: Option<Arc<NgitSigner>>,
    pub user_ref: Option<UserRef>,
    pub explicit_relays: Vec<RelayUrl>,
    pub discovery_relays: Vec<RelayUrl>,
    additional_publication_relays: Vec<RelayUrl>,
    pub offline: bool,
    pub warnings: Vec<WarningJson>,
}

impl PublicationContext {
    pub(crate) async fn load(
        offline: bool,
        explicit_relays: &[String],
        login_mode: LoginMode,
        auth: SignerParams<'_>,
    ) -> Result<Self> {
        let git_repo = Repo::discover().context("failed to find a git repository")?;
        let git_repo_path = git_repo.get_path()?;
        let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
        let selected =
            get_resolved_repo_coordinate_when_remote_unknown(&git_repo, &mut client).await?;

        let login = match login_mode {
            LoginMode::Optional => optional_login(
                load_existing_login(
                    &Some(&git_repo),
                    auth.info,
                    auth.password,
                    &None,
                    Some(&client),
                    true,
                    false,
                    false,
                )
                .await,
                auth.info.is_some(),
            )?,
            LoginMode::Required => Some(
                login::login_or_signup(
                    &Some(&git_repo),
                    auth.info,
                    auth.password,
                    Some(&client),
                    true,
                )
                .await?,
            ),
        };
        let (signer, user_ref) = if let Some((signer, user_ref, _)) = login {
            client.set_signer(Arc::clone(&signer)).await;
            (Some(signer), Some(user_ref))
        } else {
            (None, None)
        };

        // Attach an explicitly selected signer before repository relays are
        // contacted. A NIP-42 challenge is single-use; fetching anonymously
        // first can consume it and leave a later publication racing a relay's
        // replacement challenge.
        if !offline {
            fetching_with_report(git_repo_path, &client, &selected.coordinate).await?;
        }
        let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &selected.coordinate).await?;

        let explicit_relays = parse_relays(explicit_relays)?;
        let mut discovery_relays = repo_ref.relays.clone();
        discovery_relays.extend(explicit_relays.iter().cloned());
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
            signer,
            user_ref,
            explicit_relays,
            discovery_relays,
            additional_publication_relays: Vec::new(),
            offline,
            warnings: Vec::new(),
        })
    }

    pub(crate) async fn load_for_write(
        explicit_relays: &[String],
        auth: SignerParams<'_>,
    ) -> Result<Self> {
        Self::load(false, explicit_relays, LoginMode::Required, auth).await
    }

    pub(crate) fn add_publication_relay(&mut self, relay: RelayUrl) {
        self.additional_publication_relays.push(relay);
        dedup_relays(&mut self.additional_publication_relays);
    }

    pub(crate) fn git_repo_path(&self) -> Result<&std::path::Path> {
        self.git_repo.get_path()
    }

    pub(crate) async fn refresh_repository(&mut self) -> Result<()> {
        fetching_without_summary(
            self.git_repo_path()?,
            &self.client,
            &self.selected_coordinate,
        )
        .await?;
        self.repo_ref =
            get_repo_ref_from_cache(Some(self.git_repo_path()?), &self.selected_coordinate).await?;
        Ok(())
    }

    pub(crate) fn current_signer(&self) -> Option<PublicKey> {
        self.user_ref.as_ref().map(|user| user.public_key)
    }

    pub(crate) fn emit_human_warnings_before_signing(&mut self, json_output: bool) {
        if json_output {
            return;
        }
        for warning in &self.warnings {
            eprintln!("warning: {}", warning.message);
        }
        // Human output has already shown these at the last safe point before
        // the signer is invoked. Keep JSON warnings for the final envelope,
        // but avoid repeating human warnings after publication.
        self.warnings.clear();
    }

    pub(crate) fn repo_coordinate_keys(&self) -> BTreeSet<String> {
        self.repo_ref
            .members_for_announcement_tags()
            .into_iter()
            .map(|author| {
                coordinate_key(
                    &Coordinate::new(Kind::GitRepoAnnouncement, author)
                        .identifier(self.repo_ref.identifier.clone()),
                )
            })
            .collect()
    }

    pub(crate) fn publication_relays(&self) -> (Vec<String>, Vec<RelayUrl>) {
        let user_write = self
            .user_ref
            .as_ref()
            .map_or_else(Vec::new, |user| user.relays.write());
        let mut repository_relays = self.repo_ref.relays.clone();
        repository_relays.extend(self.explicit_relays.iter().cloned());
        repository_relays.extend(self.additional_publication_relays.iter().cloned());
        dedup_relays(&mut repository_relays);
        (user_write, repository_relays)
    }

    fn publication_query_groups(&self) -> Result<Vec<RelayThresholdGroup>> {
        let mut groups = self.configured_publication_query_groups(true)?;
        self.add_fallback_query_group(&mut groups)?;
        Ok(groups)
    }

    fn account_publication_query_groups(&self) -> Result<Vec<RelayThresholdGroup>> {
        self.configured_publication_query_groups(false)
    }

    fn configured_publication_query_groups(
        &self,
        include_repository_relays: bool,
    ) -> Result<Vec<RelayThresholdGroup>> {
        let account_write_relays = match &self.user_ref {
            Some(user) => parse_relays(&user.relays.write())?,
            None => Vec::new(),
        };
        let mut explicit_relays = self.explicit_relays.clone();
        explicit_relays.extend(self.additional_publication_relays.iter().cloned());
        Ok(publication_query_groups_for_scope(
            self.repo_ref.relays.clone(),
            account_write_relays,
            explicit_relays,
            include_repository_relays,
        ))
    }

    fn add_fallback_query_group(&self, groups: &mut Vec<RelayThresholdGroup>) -> Result<()> {
        if groups.is_empty() {
            groups.extend(relay_threshold_groups([(
                "fallback relays",
                parse_relays(self.client.get_relay_default_set())?,
            )]));
        }
        Ok(())
    }

    pub(crate) async fn add_author_relays(&mut self, author: PublicKey) -> Result<()> {
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

    pub(crate) async fn query(&mut self, filters: Vec<Filter>, strict: bool) -> Result<Vec<Event>> {
        let policy = if strict {
            QueryPolicy::PublicationPreflight
        } else {
            QueryPolicy::Discovery
        };
        self.query_with_policy(filters, policy).await
    }

    pub(crate) async fn query_account_publication_preflight(
        &mut self,
        filters: Vec<Filter>,
    ) -> Result<Vec<Event>> {
        self.query_with_policy(filters, QueryPolicy::AccountPublicationPreflight)
            .await
    }

    /// Query the repository and explicit relay set as one redundant class.
    ///
    /// OCI repository events are published only to that combined set, unlike
    /// releases and nsites which also target account write relays. Requiring
    /// one completed query from the actual publication set preserves the
    /// decentralized failure threshold without making unrelated account
    /// relays a prerequisite.
    pub(crate) async fn query_repository_publication_preflight(
        &mut self,
        filters: Vec<Filter>,
    ) -> Result<Vec<Event>> {
        self.query_with_policy(filters, QueryPolicy::RepositoryPublicationPreflight)
            .await
    }

    pub(crate) async fn query_with_required_discovery_route(
        &mut self,
        filters: Vec<Filter>,
    ) -> Result<Vec<Event>> {
        self.query_with_policy(filters, QueryPolicy::AtLeastOneDiscoveryRoute)
            .await
    }

    async fn query_with_policy(
        &mut self,
        filters: Vec<Filter>,
        policy: QueryPolicy,
    ) -> Result<Vec<Event>> {
        if !self.offline {
            let threshold_groups = match policy {
                QueryPolicy::PublicationPreflight => Some(self.publication_query_groups()?),
                QueryPolicy::RepositoryPublicationPreflight => {
                    Some(self.repository_publication_query_groups()?)
                }
                QueryPolicy::AccountPublicationPreflight => {
                    let mut groups = self.account_publication_query_groups()?;
                    self.add_fallback_query_group(&mut groups)?;
                    Some(groups)
                }
                QueryPolicy::Discovery | QueryPolicy::AtLeastOneDiscoveryRoute => None,
            };
            let relays = threshold_groups.as_ref().map_or_else(
                || self.discovery_relays.clone(),
                |groups| relay_threshold_union(groups),
            );
            let results = fetch_filters_to_local_cache(
                &self.client,
                self.git_repo_path()?,
                &relays,
                &filters,
            )
            .await?;
            let failed: Vec<String> = results
                .iter()
                .filter_map(|(relay, result)| result.as_ref().err().map(|_| relay.to_string()))
                .collect();
            if let Some(groups) = threshold_groups {
                let completed = results
                    .iter()
                    .filter_map(|(relay, result)| result.is_ok().then_some(relay.clone()))
                    .collect::<HashSet<_>>();
                let statuses = relay_threshold_statuses(&groups, &completed);
                let unmet = statuses
                    .iter()
                    .filter(|status| !status.met())
                    .collect::<Vec<_>>();
                if !unmet.is_empty() {
                    return Err(coded_error_with_details(
                        "relay_preflight_incomplete",
                        format!(
                            "publication preflight did not meet relay thresholds: {}",
                            unmet
                                .iter()
                                .map(|status| relay_threshold_summary(status))
                                .collect::<Vec<_>>()
                                .join("; ")
                        ),
                        relay_threshold_details(&statuses),
                    ));
                }
                if !failed.is_empty() {
                    self.warnings
                        .push(relay_preflight_incomplete_warning(&statuses));
                }
            }
            if policy == QueryPolicy::AtLeastOneDiscoveryRoute
                && (results.is_empty() || failed.len() == results.len())
            {
                return Err(coded_error_with_details(
                    "relay_discovery_unavailable",
                    "Blossom server discovery did not complete on any relay; provide --blossom-server or retry",
                    json!({ "relays": failed }),
                ));
            }
            if !policy.is_publication_preflight() && !failed.is_empty() {
                self.warnings
                    .push(relay_discovery_incomplete_warning(&failed, results.len()));
            }
        }
        get_events_from_local_cache(self.git_repo_path()?, filters).await
    }
}

impl PublicationContext {
    fn repository_publication_query_groups(&self) -> Result<Vec<RelayThresholdGroup>> {
        let mut relays = self.repo_ref.relays.clone();
        relays.extend(self.explicit_relays.iter().cloned());
        relays.extend(self.additional_publication_relays.iter().cloned());
        let mut groups = relay_threshold_groups([("repository relays", relays)]);
        self.add_fallback_query_group(&mut groups)?;
        Ok(groups)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RepositoryJson {
    pub selected_coordinate: String,
    pub coordinates: Vec<String>,
}

pub(crate) fn repository_json(context: &PublicationContext) -> RepositoryJson {
    RepositoryJson {
        selected_coordinate: coordinate_key(&context.selected_coordinate.coordinate),
        coordinates: context.repo_coordinate_keys().into_iter().collect(),
    }
}

pub(crate) fn coordinate_key(coordinate: &Coordinate) -> String {
    format!(
        "{}:{}:{}",
        coordinate.kind.as_u16(),
        coordinate.public_key.to_hex(),
        coordinate.identifier
    )
}

pub(crate) fn blossom_replication_warning<'a>(
    blobs: impl IntoIterator<Item = &'a [BlossomServerOutcome]>,
) -> Option<WarningJson> {
    let summary = summarize_blossom_replication(blobs);
    let message = summary.incomplete_message()?;
    let incomplete_servers = summary
        .servers
        .iter()
        .filter(|server| server.available != server.expected)
        .map(|server| server.server.to_string())
        .collect::<Vec<_>>();
    Some(
        WarningJson::new("blossom_replication_incomplete", message).with_details(json!({
            "confirmed": summary.available_copies,
            "placements": summary.expected_copies,
            "servers": incomplete_servers,
            "blobs": {
                "available": summary.available_blobs,
                "total": summary.blobs,
            },
            "copies_by_server": summary.servers,
        })),
    )
}

fn relay_threshold_groups<const N: usize>(
    groups: [(&'static str, Vec<RelayUrl>); N],
) -> Vec<RelayThresholdGroup> {
    groups
        .into_iter()
        .filter_map(|(label, mut relays)| {
            dedup_relays(&mut relays);
            (!relays.is_empty()).then_some(RelayThresholdGroup {
                label,
                relays,
                required: 1,
            })
        })
        .collect()
}

fn publication_query_groups_for_scope(
    repository_relays: Vec<RelayUrl>,
    account_write_relays: Vec<RelayUrl>,
    explicit_relays: Vec<RelayUrl>,
    include_repository_relays: bool,
) -> Vec<RelayThresholdGroup> {
    let mut groups = Vec::new();
    if include_repository_relays {
        groups.extend(relay_threshold_groups([(
            "repository relays",
            repository_relays,
        )]));
    }
    groups.extend(relay_threshold_groups([
        ("account write relays", account_write_relays),
        ("explicit publication relays", explicit_relays),
    ]));
    groups
}

fn relay_threshold_union(groups: &[RelayThresholdGroup]) -> Vec<RelayUrl> {
    let mut relays = groups
        .iter()
        .flat_map(|group| group.relays.iter().cloned())
        .collect::<Vec<_>>();
    dedup_relays(&mut relays);
    relays
}

fn relay_threshold_statuses(
    groups: &[RelayThresholdGroup],
    completed: &HashSet<RelayUrl>,
) -> Vec<RelayThresholdStatus> {
    groups
        .iter()
        .map(|group| {
            let unavailable = group
                .relays
                .iter()
                .filter(|relay| !completed.contains(*relay))
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            RelayThresholdStatus {
                label: group.label,
                completed: group.relays.len() - unavailable.len(),
                total: group.relays.len(),
                required: group.required,
                unavailable,
            }
        })
        .collect()
}

fn relay_threshold_summary(status: &RelayThresholdStatus) -> String {
    format!(
        "{} {}/{} completed (requires at least {}; unavailable: {})",
        status.label,
        status.completed,
        status.total,
        status.required,
        status.unavailable.join(", ")
    )
}

fn relay_threshold_details(statuses: &[RelayThresholdStatus]) -> Value {
    json!({
        "groups": statuses.iter().map(|status| json!({
            "type": status.label,
            "completed": status.completed,
            "total": status.total,
            "required": status.required,
            "unavailable": status.unavailable,
        })).collect::<Vec<_>>()
    })
}

fn relay_preflight_incomplete_warning(statuses: &[RelayThresholdStatus]) -> WarningJson {
    let incomplete = statuses
        .iter()
        .filter(|status| status.completed < status.total)
        .collect::<Vec<_>>();
    WarningJson::new(
        "relay_preflight_incomplete",
        format!(
            "publication preflight was incomplete: {}; publication will proceed because every relay class met its threshold",
            incomplete
                .iter()
                .map(|status| relay_threshold_summary(status))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    )
    .with_details(relay_threshold_details(statuses))
}

fn relay_discovery_incomplete_warning(failed: &[String], queried: usize) -> WarningJson {
    let failed_count = failed.len();
    let relay_label = if queried == 1 {
        "repository relay"
    } else {
        "repository relays"
    };
    let message = format!(
        "{failed_count}/{queried} {relay_label} ({}) could not be queried; results may be incomplete",
        failed.join(", ")
    );
    WarningJson::new("relay_discovery_incomplete", message).with_details(json!({
        "relays": failed,
        "failed": failed_count,
        "queried": queried,
    }))
}

fn optional_login<T>(login: Result<T>, explicit_signer: bool) -> Result<Option<T>> {
    match login {
        Ok(login) => Ok(Some(login)),
        Err(error) if explicit_signer => Err(error),
        Err(_) => Ok(None),
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use nostr::prelude::RelayUrl;

    use super::{
        optional_login, publication_query_groups_for_scope, relay_discovery_incomplete_warning,
        relay_preflight_incomplete_warning, relay_threshold_groups, relay_threshold_statuses,
        relay_threshold_union,
    };

    #[test]
    fn optional_login_fails_closed_for_explicit_signers() {
        let explicit =
            optional_login::<()>(Err(anyhow::anyhow!("selected signer failed")), true).unwrap_err();
        assert_eq!(explicit.to_string(), "selected signer failed");

        let configured =
            optional_login::<()>(Err(anyhow::anyhow!("configured signer failed")), false).unwrap();
        assert!(configured.is_none());
    }

    #[test]
    fn publication_preflight_requires_one_completed_relay_per_class() {
        let repository_one = RelayUrl::parse("wss://repo-one.example").unwrap();
        let repository_two = RelayUrl::parse("wss://repo-two.example").unwrap();
        let account = RelayUrl::parse("wss://account.example").unwrap();
        let explicit = RelayUrl::parse("wss://explicit.example").unwrap();
        let groups = relay_threshold_groups([
            (
                "repository relays",
                vec![repository_one.clone(), repository_two.clone()],
            ),
            ("account write relays", vec![account.clone()]),
            ("explicit publication relays", vec![explicit.clone()]),
        ]);
        let completed = HashSet::from([repository_two, account, explicit]);

        let statuses = relay_threshold_statuses(&groups, &completed);

        assert!(statuses.iter().all(|status| status.met()));
        assert_eq!(statuses[0].completed, 1);
        assert_eq!(statuses[0].total, 2);
        assert_eq!(statuses[0].unavailable, [repository_one.to_string()]);
        let warning = relay_preflight_incomplete_warning(&statuses);
        assert_eq!(warning.code, "relay_preflight_incomplete");
        assert!(warning.message.contains("repository relays 1/2 completed"));
        assert!(warning.message.contains(repository_one.as_str()));
    }

    #[test]
    fn publication_preflight_rejects_an_unavailable_relay_class() {
        let repository = RelayUrl::parse("wss://repo.example").unwrap();
        let account = RelayUrl::parse("wss://account.example").unwrap();
        let groups = relay_threshold_groups([
            ("repository relays", vec![repository.clone()]),
            ("account write relays", vec![account]),
        ]);

        let statuses = relay_threshold_statuses(&groups, &HashSet::from([repository]));

        assert!(statuses[0].met());
        assert!(!statuses[1].met());
    }

    #[test]
    fn publication_preflight_queries_shared_relays_once() {
        let shared = RelayUrl::parse("wss://shared.example").unwrap();
        let groups = relay_threshold_groups([
            ("repository relays", vec![shared.clone()]),
            ("explicit publication relays", vec![shared.clone()]),
        ]);

        assert_eq!(relay_threshold_union(&groups), [shared]);
    }

    #[test]
    fn account_preflight_excludes_repository_relays() {
        let repository = RelayUrl::parse("wss://repo.example").unwrap();
        let account = RelayUrl::parse("wss://account.example").unwrap();
        let explicit = RelayUrl::parse("wss://explicit.example").unwrap();

        let groups = publication_query_groups_for_scope(
            vec![repository.clone()],
            vec![account.clone()],
            vec![explicit.clone()],
            false,
        );

        assert_eq!(
            groups.iter().map(|group| group.label).collect::<Vec<_>>(),
            ["account write relays", "explicit publication relays"]
        );
        assert_eq!(relay_threshold_union(&groups), [account, explicit]);
        assert!(!relay_threshold_union(&groups).contains(&repository));
    }

    #[test]
    fn incomplete_relay_warning_names_failures_and_query_coverage() {
        let warning = relay_discovery_incomplete_warning(
            &[
                "wss://relay.ngit.dev".to_owned(),
                "wss://gitnostr.com".to_owned(),
            ],
            4,
        );

        assert_eq!(warning.code, "relay_discovery_incomplete");
        assert_eq!(
            warning.message,
            "2/4 repository relays (wss://relay.ngit.dev, wss://gitnostr.com) could not be queried; results may be incomplete"
        );
        assert_eq!(warning.details["relays"].as_array().map(Vec::len), Some(2));
        assert_eq!(warning.details["failed"], 2);
        assert_eq!(warning.details["queried"], 4);
    }

    #[test]
    fn incomplete_relay_warning_uses_singular_for_one_query_route() {
        let warning = relay_discovery_incomplete_warning(&["wss://relay.ngit.dev".to_owned()], 1);

        assert!(warning.message.starts_with("1/1 repository relay ("));
    }
}
