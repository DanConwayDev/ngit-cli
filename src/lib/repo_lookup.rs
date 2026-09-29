//! Command-owned repository lookup context. Capture the source before fetching;
//! never re-resolve a target merely to explain a failure.

use std::{fmt, path::Path};

use anyhow::Result;
use nostr::prelude::{Nip19Coordinate, ToBech32};
use serde_json::{Value, json};

use crate::{
    client::{AnnouncementSearches, FetchReport, MissingRepoAnnouncement, get_repo_ref_from_cache},
    git::Repo,
    repo_ref::{RepoRef, ResolvedRepoCoordinate},
};

pub struct RepositoryLookup {
    pub coordinate: Nip19Coordinate,
    source: String,
    value: String,
    guidance: String,
    searches: Option<AnnouncementSearches>,
    search_history_known: bool,
}

impl RepositoryLookup {
    pub fn from_resolved(repo: &Repo, selected: &ResolvedRepoCoordinate) -> Result<Self> {
        let (source, value, guidance) = selected.lookup_diagnostic_source(repo)?;
        Ok(Self::new(
            selected.coordinate.clone(),
            source,
            value,
            guidance,
        ))
    }

    pub fn new(
        coordinate: Nip19Coordinate,
        source: String,
        value: String,
        guidance: String,
    ) -> Self {
        Self {
            coordinate,
            source,
            value,
            guidance,
            searches: None,
            search_history_known: true,
        }
    }

    /// Use for a related repository address discovered by the command itself.
    pub fn related(coordinate: Nip19Coordinate, source: &str) -> Result<Self> {
        let value = coordinate.to_bech32()?;
        let mut lookup = Self::new(
            coordinate,
            source.to_string(),
            value,
            "Check this repository address and refresh its data from Nostr relays.".to_string(),
        );
        // The parent lookup may have fetched related announcements. Its relay
        // evidence describes the selected address, not this derived address.
        lookup.search_history_known = false;
        Ok(lookup)
    }

    pub fn record(&mut self, report: &FetchReport) {
        self.searches
            .get_or_insert_with(AnnouncementSearches::default)
            .merge(report.announcement_searches.clone());
    }

    pub async fn load(&self, path: &Path) -> Result<RepoRef> {
        get_repo_ref_from_cache(Some(path), &self.coordinate)
            .await
            .map_err(|error| self.explain(error))
    }

    /// Preserve operational and authority errors; only an actual missing
    /// announcement receives the missing-repository diagnosis.
    pub fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        if error.is::<MissingRepoAnnouncement>() {
            self.missing().into()
        } else {
            error
        }
    }

    pub fn missing(&self) -> RepositoryLookupError {
        if let Some(searches) = &self.searches {
            let mut report = FetchReport::default();
            report.announcement_searches = searches.clone();
            let message = MissingRepoAnnouncement(self.coordinate.clone())
                .diagnostic(&self.source, &self.value, &self.guidance, &report)
                .to_string();
            RepositoryLookupError {
                category: if searches.has_response() {
                    "repository_not_found"
                } else {
                    "repository_lookup_incomplete"
                },
                message,
                details: json!({"source": self.source, "value": self.value,
                    "coordinate": self.coordinate.to_bech32().ok(), "network_searched": true,
                    "relays": searches.diagnostic_details()}),
            }
        } else {
            let network_advice = if self.search_history_known {
                "No Nostr relays were searched. Run again with network fetching enabled\n(without `--offline` or `--no-fetch`) to check the relays.\n\n"
            } else {
                ""
            };
            let mut message = format!(
                "couldn't find the repository specified by {} in the local cache:\n  {}\n\n{network_advice}{}",
                self.source, self.value, self.guidance
            );
            if crate::output_mode::is_verbose() {
                message.push_str(&format!(
                    "\nRepository coordinate: {}",
                    self.coordinate.coordinate
                ));
            }
            RepositoryLookupError {
                category: "repository_not_cached",
                message,
                details: json!({"source": self.source, "value": self.value,
                    "coordinate": self.coordinate.to_bech32().ok(), "network_searched": self.search_history_known.then_some(false), "relays": []}),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct RepositoryLookupError {
    pub category: &'static str,
    pub message: String,
    pub details: Value,
}

impl RepositoryLookupError {
    pub fn with_advice(mut self, advice: &str) -> Self {
        self.message.push_str("\n\n");
        self.message.push_str(advice);
        self
    }
}

impl fmt::Display for RepositoryLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RepositoryLookupError {}

#[cfg(test)]
mod tests {
    use nostr::prelude::{Coordinate, Keys, Kind};

    use super::*;

    #[test]
    fn related_repository_does_not_claim_that_no_network_search_happened() {
        let coordinate = Nip19Coordinate::new(
            Coordinate::new(Kind::GitRepoAnnouncement, Keys::generate().public_key())
                .identifier("lead"),
            [],
        );
        let diagnostic =
            RepositoryLookup::related(coordinate, "the repository's lead announcement")
                .unwrap()
                .missing();
        assert!(diagnostic.details["network_searched"].is_null());
        assert!(!diagnostic.message.contains("No Nostr relays were searched"));
        assert!(diagnostic.message.contains("local cache"));
    }

    #[test]
    fn only_missing_announcements_receive_lookup_diagnostics() {
        let lookup = RepositoryLookup::new(
            Nip19Coordinate::new(
                Coordinate::new(Kind::GitRepoAnnouncement, Keys::generate().public_key())
                    .identifier("example"),
                [],
            ),
            "test source".into(),
            "test value".into(),
            "Check the selected source.".into(),
        );
        let operational = lookup.explain(anyhow::anyhow!("cache permission denied"));
        assert!(!operational.is::<RepositoryLookupError>());
        assert!(operational.to_string().contains("permission denied"));
        let missing = lookup.explain(MissingRepoAnnouncement(lookup.coordinate.clone()).into());
        let diagnostic = missing.downcast_ref::<RepositoryLookupError>().unwrap();
        assert_eq!(diagnostic.category, "repository_not_cached");
        assert_eq!(diagnostic.details["network_searched"], false);
    }
}
