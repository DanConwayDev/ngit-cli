use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result, bail};
use nostr::PublicKey;
use nostr_sdk::{Alphabet, JsonUtil, Kind, SingleLetterTag, Timestamp, ToBech32};
use serde::{self, Deserialize, Serialize};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::client::{Connect, get_event_from_global_cache};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub public_key: PublicKey,
    pub metadata: UserMetadata,
    pub relays: UserRelays,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserMetadata {
    pub name: String,
    pub created_at: Timestamp,
    pub nip05: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRelays {
    pub relays: Vec<UserRelayRef>,
    pub created_at: Timestamp,
}

impl UserRelays {
    pub fn write(&self) -> Vec<String> {
        self.relays
            .iter()
            .filter(|r| r.write)
            .map(|r| r.url.clone())
            .collect()
    }
    pub fn read(&self) -> Vec<String> {
        self.relays
            .iter()
            .filter(|r| r.read)
            .map(|r| r.url.clone())
            .collect()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRelayRef {
    pub url: String,
    pub read: bool,
    pub write: bool,
}

pub async fn get_user_details(
    public_key: &PublicKey,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    git_repo_path: Option<&Path>,
    cache_only: bool,
    fetch_profile_updates: bool,
) -> Result<UserRef> {
    if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
        if fetch_profile_updates {
            if let Some(client) = client {
                let term = console::Term::stderr();
                term.write_line("searching for profile updates...")?;
                let (reports, progress_reporter) = client
                    .fetch_all(git_repo_path, None, &HashSet::from_iter(vec![*public_key]))
                    .await?;
                if !reports.iter().any(|r| r.is_err()) {
                    progress_reporter.clear()?;
                    term.clear_last_lines(1)?;
                }
                return get_user_ref_from_cache(git_repo_path, public_key).await;
            }
        }
        Ok(user_ref)
    } else {
        let empty = UserRef {
            public_key: public_key.to_owned(),
            metadata: extract_user_metadata(public_key, &[])?,
            relays: extract_user_relays(public_key, &[]),
        };
        if cache_only {
            Ok(empty)
        } else if let Some(client) = client {
            let term = console::Term::stderr();
            term.write_line("searching for profile...")?;
            let (_, progress_reporter) = client
                .fetch_all(git_repo_path, None, &HashSet::from_iter(vec![*public_key]))
                .await?;
            if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
                progress_reporter.clear()?;
                // if std::env::var("NGITTEST").is_err() {term.clear_last_lines(1)?;}
                Ok(user_ref)
            } else {
                Ok(empty)
            }
        } else {
            Ok(empty)
        }
    }
}

pub async fn get_user_ref_from_cache(
    git_repo_path: Option<&Path>,
    public_key: &PublicKey,
) -> Result<UserRef> {
    let filters = vec![
        nostr::Filter::default()
            .author(*public_key)
            .kind(Kind::Metadata),
        nostr::Filter::default()
            .author(*public_key)
            .kind(Kind::RelayList),
    ];

    let events = get_event_from_global_cache(git_repo_path, filters.clone()).await?;

    if events.is_empty() {
        bail!("no metadata and profile list in cache for selected public key");
    }
    Ok(UserRef {
        public_key: public_key.to_owned(),
        metadata: extract_user_metadata(public_key, &events)?,
        relays: extract_user_relays(public_key, &events),
    })
}

pub fn extract_user_metadata(
    public_key: &nostr::PublicKey,
    events: &[nostr::Event],
) -> Result<UserMetadata> {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&nostr::Kind::Metadata) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    let metadata: Option<nostr::Metadata> = if let Some(event) = event {
        Some(
            nostr::Metadata::from_json(event.content.clone())
                .context("metadata cannot be found in kind 0 event content")?,
        )
    } else {
        None
    };

    Ok(UserMetadata {
        name: if let Some(metadata) = metadata.clone() {
            if let Some(n) = metadata.name {
                n
            } else if let Some(n) = metadata.custom.get("displayName") {
                // strip quote marks that custom.get() adds
                let binding = n.to_string();
                let mut chars = binding.chars();
                chars.next();
                chars.next_back();
                chars.as_str().to_string()
            } else if let Some(n) = metadata.display_name {
                n
            } else {
                public_key.to_bech32()?
            }
        } else {
            public_key.to_bech32()?
        },
        nip05: if let Some(metadata) = metadata {
            metadata.nip05
        } else {
            None
        },
        created_at: if let Some(event) = event {
            event.created_at
        } else {
            Timestamp::from(0)
        },
    })
}

pub fn extract_user_relays(public_key: &nostr::PublicKey, events: &[nostr::Event]) -> UserRelays {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&nostr::Kind::RelayList) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    UserRelays {
        relays: if let Some(event) = event {
            event
                .tags
                .iter()
                .filter(|t| {
                    t.as_slice().len() > 1
                        && t.kind()
                            .eq(&nostr::TagKind::SingleLetter(SingleLetterTag::lowercase(
                                Alphabet::R,
                            )))
                })
                .map(|t| UserRelayRef {
                    url: t.as_slice()[1].clone(),
                    read: t.as_slice().len() == 2 || t.as_slice()[2].eq("read"),
                    write: t.as_slice().len() == 2 || t.as_slice()[2].eq("write"),
                })
                .collect()
        } else {
            vec![]
        },
        created_at: if let Some(event) = event {
            event.created_at
        } else {
            Timestamp::from(0)
        },
    }
}
