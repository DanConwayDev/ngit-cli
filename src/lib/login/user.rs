use std::{collections::HashSet, path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use nostr::prelude::{Kind, PublicKey, SingleLetterTag, Timestamp, ToBech32, Url, event::Tag};
use serde::{self, Deserialize, Serialize};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::{Connect, FetchReport, get_event_from_global_cache, is_verbose, sign_event},
    git_events::KIND_USER_GRASP_LIST,
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub public_key: PublicKey,
    pub metadata: UserMetadata,
    pub relays: UserRelays,
    pub grasp_list: UserGraspList,
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
pub struct UserGraspList {
    pub urls: Vec<Url>,
    pub created_at: Timestamp,
}

impl UserGraspList {
    pub async fn to_event(
        &mut self,
        signer: &Arc<crate::NgitSigner>,
    ) -> Result<nostr::prelude::Event> {
        let event = sign_event(
            nostr::prelude::EventBuilder::new(KIND_USER_GRASP_LIST, "").tags(
                self.urls
                    .iter()
                    .map(|url| Tag::parse(["g", url.as_ref()]).unwrap())
                    .collect::<Vec<_>>(),
            ),
            signer,
            "user grasp list".to_string(),
        )
        .await?;
        self.created_at = event.created_at;
        Ok(event)
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
                if is_verbose() {
                    term.write_line("searching for profile updates...")?;
                }
                let (reports, progress_reporter) = client
                    .fetch_all(git_repo_path, None, &HashSet::from_iter(vec![*public_key]))
                    .await?;
                finish_profile_fetch(&reports, progress_reporter)?;
                if is_verbose() && !reports.iter().any(|report| report.is_err()) {
                    term.clear_last_lines(1)?;
                }
                return get_user_ref_from_cache(git_repo_path, public_key).await;
            }
        }
        Ok(user_ref)
    } else {
        // No cached profile found. Fall back to fetching from default relays
        // (bootstrapping).
        let empty = UserRef {
            public_key: public_key.to_owned(),
            metadata: extract_user_metadata(public_key, &[])?,
            relays: extract_user_relays(public_key, &[]),
            grasp_list: extract_user_grasp_list(public_key, &[]),
        };
        if cache_only {
            Ok(empty)
        } else if let Some(client) = client {
            let term = console::Term::stderr();
            if is_verbose() {
                term.write_line("searching for profile...")?;
            }
            let (reports, progress_reporter) = client
                .fetch_all(git_repo_path, None, &HashSet::from_iter(vec![*public_key]))
                .await?;
            finish_profile_fetch(&reports, progress_reporter)?;
            if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
                Ok(user_ref)
            } else {
                Ok(empty)
            }
        } else {
            Ok(empty)
        }
    }
}

/// Complete a profile fetch before its caller prints ordinary status text.
/// Successful relay details are transient; errors remain visible and receive
/// a separating newline so subsequent output cannot share the final bar line.
fn finish_profile_fetch(
    reports: &[Result<FetchReport>],
    progress_reporter: indicatif::MultiProgress,
) -> Result<()> {
    let had_errors = reports.iter().any(Result::is_err);
    if !had_errors {
        progress_reporter.clear()?;
    }
    drop(progress_reporter);
    if had_errors {
        console::Term::stderr().write_line("")?;
    }
    Ok(())
}

pub async fn get_user_ref_from_cache(
    git_repo_path: Option<&Path>,
    public_key: &PublicKey,
) -> Result<UserRef> {
    let filters = vec![
        nostr::prelude::Filter::default()
            .author(*public_key)
            .kind(Kind::Metadata),
        nostr::prelude::Filter::default()
            .author(*public_key)
            .kind(Kind::RelayList),
        nostr::prelude::Filter::default()
            .author(*public_key)
            .kind(KIND_USER_GRASP_LIST),
    ];

    let events = get_event_from_global_cache(git_repo_path, filters.clone()).await?;

    if events.is_empty() {
        bail!("no metadata and profile list in cache for selected public key");
    }
    Ok(UserRef {
        public_key: public_key.to_owned(),
        metadata: extract_user_metadata(public_key, &events)?,
        relays: extract_user_relays(public_key, &events),
        grasp_list: extract_user_grasp_list(public_key, &events),
    })
}

pub fn extract_user_metadata(
    public_key: &nostr::prelude::PublicKey,
    events: &[nostr::prelude::Event],
) -> Result<UserMetadata> {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&nostr::prelude::Kind::Metadata) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    let metadata: Option<nostr::prelude::Metadata> = if let Some(event) = event {
        Some(
            nostr::prelude::Metadata::from_json(event.content.clone())
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

pub fn extract_user_relays(
    public_key: &nostr::prelude::PublicKey,
    events: &[nostr::prelude::Event],
) -> UserRelays {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&nostr::prelude::Kind::RelayList) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    UserRelays {
        relays: if let Some(event) = event {
            event
                .tags
                .iter()
                .filter(|t| {
                    t.as_slice().len() > 1
                        && t.single_letter_tag() == Some(SingleLetterTag::LOWERCASE_R)
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

pub fn extract_user_grasp_list(
    public_key: &nostr::prelude::PublicKey,
    events: &[nostr::prelude::Event],
) -> UserGraspList {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&KIND_USER_GRASP_LIST) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    UserGraspList {
        urls: if let Some(event) = event {
            event
                .tags
                .iter()
                .filter_map(|t| {
                    if t.as_slice().len() > 1 && t.as_slice()[0] == "g" {
                        Url::parse(&t.as_slice()[1]).ok()
                    } else {
                        None
                    }
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

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};

    use super::finish_profile_fetch;
    use crate::client::FetchReport;

    #[derive(Debug)]
    struct ClearTrackingTerm {
        clears: Arc<AtomicUsize>,
    }

    impl TermLike for ClearTrackingTerm {
        fn width(&self) -> u16 {
            80
        }

        fn move_cursor_up(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, _s: &str) -> io::Result<()> {
            Ok(())
        }

        fn write_str(&self, _s: &str) -> io::Result<()> {
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            self.clears.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn successful_profile_fetch_clears_progress_without_a_profile() {
        let clears = Arc::new(AtomicUsize::new(0));
        let progress = MultiProgress::with_draw_target(ProgressDrawTarget::term_like(Box::new(
            ClearTrackingTerm {
                clears: clears.clone(),
            },
        )));
        let bar = progress.add(
            ProgressBar::new(1)
                .with_style(ProgressStyle::with_template("{msg}").expect("valid style")),
        );
        bar.finish_with_message("no new events");

        finish_profile_fetch(&[Ok(FetchReport::default())], progress)
            .expect("successful progress cleanup");

        assert!(
            clears.load(Ordering::Relaxed) > 0,
            "a successful fetch must clear transient relay details even when no profile was found"
        );
    }
}
