use std::{
    collections::HashSet,
    fs::{self, create_dir, create_dir_all, remove_dir},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use indicatif::MultiProgress;
use nostr::prelude::{
    Event, EventBuilder, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp, ToBech32, Url,
    event::Tag,
};
use serde::{self, Deserialize, Serialize};
use tempfile::NamedTempFile;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
#[cfg(not(test))]
use crate::{client::save_event_in_global_cache, get_dirs};
use crate::{
    client::{
        Connect, FetchReport, get_event_from_global_cache, is_verbose, sign_draft_event, sign_event,
    },
    git_events::{KIND_PRIVATE_GIT_RELAY_LIST, KIND_USER_GRASP_LIST},
};

const PRIVATE_RELAY_LIST_UPDATE_ATTEMPTS: usize = 3;
const PRIVATE_RELAY_LIST_LOCK_WAIT: Duration = Duration::from_secs(60);
const PRIVATE_RELAY_LIST_STALE_LOCK_AGE: Duration = Duration::from_secs(30 * 60);
const PRIVATE_RELAY_LIST_CACHE_DIR: &str = "private-git-relay-lists";

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
pub struct PrivateGitRelayList {
    pub relays: Vec<RelayUrl>,
    pub created_at: Timestamp,
    #[serde(skip)]
    source_event: Option<Event>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivateGitRelayDiscovery {
    Available(Vec<RelayUrl>),
    Absent,
    Unavailable(String),
}

impl PrivateGitRelayDiscovery {
    pub fn relays(&self) -> &[RelayUrl] {
        match self {
            Self::Available(relays) => relays,
            Self::Absent | Self::Unavailable(_) => &[],
        }
    }

    pub fn requires_repository_only_probe(&self) -> bool {
        matches!(self, Self::Available(relays) if !relays.is_empty())
    }
}

impl PrivateGitRelayList {
    pub fn new(relays: Vec<RelayUrl>) -> Result<Self> {
        Ok(Self {
            relays: validate_and_dedupe_private_git_relays(relays)?,
            created_at: Timestamp::from(0),
            source_event: None,
        })
    }

    pub async fn to_event(&mut self, signer: &Arc<crate::NgitSigner>) -> Result<Event> {
        self.relays = validate_and_dedupe_private_git_relays(self.relays.clone())?;
        let private_items = self
            .relays
            .iter()
            .map(|relay| vec!["g".to_string(), relay.to_string()])
            .collect::<Vec<_>>();
        let plaintext = serde_json::to_string(&private_items)
            .context("failed to encode private git relay list")?;
        let public_key = signer.get_public_key().await?;
        let content = signer
            .nip44_encrypt(&public_key, &plaintext)
            .await
            .context("failed to encrypt private git relay list")?;
        let event = sign_draft_event(
            crate::event_ordering::finalize_strictly_later_unsigned(
                EventBuilder::new(KIND_PRIVATE_GIT_RELAY_LIST, content),
                public_key,
                self.source_event.as_ref(),
            )?,
            signer,
            "private git relay list".to_string(),
        )
        .await?;
        self.created_at = event.created_at;
        self.source_event = Some(event.clone());
        Ok(event)
    }

    pub async fn from_event(event: &Event, signer: &Arc<crate::NgitSigner>) -> Result<Self> {
        #[cfg(test)]
        let cache_dir: Option<PathBuf> = None;
        #[cfg(not(test))]
        let cache_dir = get_dirs()
            .ok()
            .map(|dirs| dirs.cache_dir().join(PRIVATE_RELAY_LIST_CACHE_DIR));
        Self::from_event_with_cache_dir(event, signer, cache_dir.as_deref()).await
    }

    async fn from_event_with_cache_dir(
        event: &Event,
        signer: &Arc<crate::NgitSigner>,
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        if event.kind != KIND_PRIVATE_GIT_RELAY_LIST {
            bail!("event is not a private git relay list");
        }
        event
            .verify()
            .context("invalid private git relay list event")?;
        let public_key = signer.get_public_key().await?;
        if event.pubkey != public_key {
            bail!("private git relay list was not authored by the signer");
        }
        if !event.tags.is_empty() {
            bail!("private git relay list must not contain public tags");
        }

        if let Some(cache_dir) = cache_dir {
            if let Ok(Some(plaintext)) = read_cached_private_git_relay_list(cache_dir, event) {
                if let Ok(list) = Self::from_plaintext(event, &plaintext) {
                    return Ok(list);
                }
            }
        }
        let plaintext = signer
            .nip44_decrypt(&event.pubkey, &event.content)
            .await
            .context("failed to decrypt private git relay list")?;
        let list = Self::from_plaintext(event, &plaintext)?;
        if let Some(cache_dir) = cache_dir {
            if let Err(error) = write_cached_private_git_relay_list(cache_dir, event, &plaintext) {
                if crate::client::is_verbose() {
                    eprintln!("nostr: failed to cache decrypted private Git relay list: {error:#}");
                }
            }
        }
        Ok(list)
    }

    fn from_plaintext(event: &Event, plaintext: &str) -> Result<Self> {
        let items: Vec<Vec<String>> = serde_json::from_str(plaintext)
            .context("private git relay list content is not a JSON array")?;
        let mut relays = Vec::with_capacity(items.len());
        for item in items {
            let [tag, relay] = item.as_slice() else {
                bail!("private git relay list items must be two-element arrays");
            };
            if tag != "g" {
                bail!("private git relay list items must be g tags");
            }
            relays.push(
                RelayUrl::parse(relay)
                    .with_context(|| format!("invalid private git relay URL: {relay}"))?,
            );
        }

        Ok(Self {
            relays: validate_and_dedupe_private_git_relays(relays)?,
            created_at: event.created_at,
            source_event: Some(event.clone()),
        })
    }
}

fn private_git_relay_list_cache_path(cache_dir: &Path, event: &Event) -> PathBuf {
    cache_dir.join(format!("{}.json", event.id.to_hex()))
}

fn read_cached_private_git_relay_list(cache_dir: &Path, event: &Event) -> Result<Option<String>> {
    let path = private_git_relay_list_cache_path(cache_dir, event);
    match fs::read_to_string(&path) {
        Ok(plaintext) => Ok(Some(plaintext)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read private relay-list cache {}", path.display())),
    }
}

fn write_cached_private_git_relay_list(
    cache_dir: &Path,
    event: &Event,
    plaintext: &str,
) -> Result<()> {
    if !cache_dir.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(cache_dir).with_context(|| {
            format!(
                "failed to create private relay-list cache {}",
                cache_dir.display()
            )
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(cache_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to secure private relay-list cache {}",
                cache_dir.display()
            )
        })?;
    }

    let path = private_git_relay_list_cache_path(cache_dir, event);
    let mut temporary = NamedTempFile::new_in(cache_dir).with_context(|| {
        format!(
            "failed to create a temporary private relay-list cache file in {}",
            cache_dir.display()
        )
    })?;
    temporary
        .write_all(plaintext.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .with_context(|| {
            format!(
                "failed to write private relay-list cache {}",
                path.display()
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to secure private relay-list cache {}",
                    path.display()
                )
            })?;
    }
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "failed to replace private relay-list cache {}",
                path.display()
            )
        })?;
    #[cfg(unix)]
    fs::File::open(cache_dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "failed to sync private relay-list cache {}",
                cache_dir.display()
            )
        })?;
    Ok(())
}

/// Fetch and decrypt the newest valid private Git relay list from the user's
/// ordinary discovery relays.
pub async fn fetch_private_git_relay_list<C: Connect + Sync>(
    client: &C,
    relays: Vec<String>,
    signer: &Arc<crate::NgitSigner>,
) -> Result<Option<PrivateGitRelayList>> {
    if relays.is_empty() {
        bail!("no normal relay is available for private Git relay discovery");
    }
    let public_key = signer.get_public_key().await?;
    let mut events = client
        .get_events(
            relays,
            vec![
                nostr::prelude::Filter::new()
                    .kind(KIND_PRIVATE_GIT_RELAY_LIST)
                    .author(public_key)
                    .limit(10),
            ],
        )
        .await?;
    events.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    for event in events {
        if let Ok(list) = PrivateGitRelayList::from_event(&event, signer).await {
            #[cfg(not(test))]
            let _ = save_event_in_global_cache(None, &event).await;
            return Ok(Some(list));
        }
    }
    Ok(None)
}

/// Discover the account's private Git relays without treating a discovery
/// outage as proof that the list is absent.
pub async fn discover_private_git_relay_list<C: Connect + Sync>(
    client: &C,
    relays: Vec<String>,
    signer: &Arc<crate::NgitSigner>,
) -> PrivateGitRelayDiscovery {
    let discovery = match fetch_private_git_relay_list(client, relays, signer).await {
        Ok(Some(list)) if list.relays.is_empty() => PrivateGitRelayDiscovery::Absent,
        Ok(Some(list)) => PrivateGitRelayDiscovery::Available(list.relays),
        Ok(None) => match cached_private_git_relay_list(signer).await {
            Ok(Some(list)) if list.relays.is_empty() => PrivateGitRelayDiscovery::Absent,
            Ok(Some(list)) => PrivateGitRelayDiscovery::Available(list.relays),
            Ok(None) | Err(_) => PrivateGitRelayDiscovery::Absent,
        },
        Err(error) => match cached_private_git_relay_list(signer).await {
            Ok(Some(list)) if list.relays.is_empty() => PrivateGitRelayDiscovery::Absent,
            Ok(Some(list)) => PrivateGitRelayDiscovery::Available(list.relays),
            Ok(None) | Err(_) => PrivateGitRelayDiscovery::Unavailable(error.to_string()),
        },
    };
    if let PrivateGitRelayDiscovery::Available(relays) = &discovery {
        // Decrypting kind 10318 is the trusted signal that these URLs are
        // account-private repository relays. The caller acquired `signer` to
        // decrypt the event before this classification is installed.
        client.nip42_register_private_repo_relays(relays.clone());
    }
    discovery
}

async fn cached_private_git_relay_list(
    signer: &Arc<crate::NgitSigner>,
) -> Result<Option<PrivateGitRelayList>> {
    #[cfg(test)]
    {
        let _ = signer;
        Ok(None)
    }
    #[cfg(not(test))]
    {
        let public_key = signer.get_public_key().await?;
        let events = get_event_from_global_cache(
            None,
            vec![
                nostr::prelude::Filter::new()
                    .kind(KIND_PRIVATE_GIT_RELAY_LIST)
                    .author(public_key),
            ],
        )
        .await?;
        newest_valid_private_git_relay_list(events, signer).await
    }
}

#[cfg(not(test))]
async fn newest_valid_private_git_relay_list(
    mut events: Vec<Event>,
    signer: &Arc<crate::NgitSigner>,
) -> Result<Option<PrivateGitRelayList>> {
    events.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    for event in events {
        if let Ok(list) = PrivateGitRelayList::from_event(&event, signer).await {
            return Ok(Some(list));
        }
    }
    Ok(None)
}

/// Add private relay-list entries to a repository coordinate and report
/// whether private discovery should be attempted before public discovery.
pub fn add_private_git_relay_hints(
    coordinate: &mut nostr::nips::nip19::Nip19Coordinate,
    relays: &[RelayUrl],
) -> bool {
    for relay in relays {
        if !coordinate.relays.contains(relay) {
            coordinate.relays.push(relay.clone());
        }
    }
    !relays.is_empty()
}

/// Publish an updated private Git relay list to the user's ordinary relays.
///
/// This list is account-scoped rather than repository-related, so GRASP-08
/// permits publishing it to the user's normal discovery relays.
pub async fn publish_private_git_relay_list<C: Connect + Sync>(
    client: &C,
    repository_relays: &[RelayUrl],
    user_ref: &UserRef,
    signer: &Arc<crate::NgitSigner>,
) -> Result<()> {
    let public_key = signer.get_public_key().await?;
    let _lock = PrivateRelayListUpdateLock::acquire(&public_key).await?;
    let mut discovery_relays = user_ref.relays.read();
    for relay in user_ref.relays.write() {
        if !discovery_relays.contains(&relay) {
            discovery_relays.push(relay);
        }
    }
    if discovery_relays.is_empty() {
        discovery_relays.extend(client.get_relay_default_set().iter().cloned());
    }
    let write_relays = {
        let configured = user_ref.relays.write();
        if configured.is_empty() {
            discovery_relays.clone()
        } else {
            configured
        }
    };
    ensure_private_relay_list_write_target_was_read(client, &write_relays, public_key).await?;

    for attempt in 0..PRIVATE_RELAY_LIST_UPDATE_ATTEMPTS {
        let mut private_relays =
            fetch_private_git_relay_list(client, discovery_relays.clone(), signer)
                .await
                .context("failed to load the existing private Git relay list")?
                .unwrap_or(PrivateGitRelayList::new(vec![])?);
        for relay in repository_relays {
            if !private_relays.relays.contains(relay) {
                private_relays.relays.push(relay.clone());
            }
        }

        let event = private_relays.to_event(signer).await?;
        let mut published = false;
        let mut last_error = None;
        for relay in &write_relays {
            match client
                .send_event_to(None, relay, event.clone())
                .await
                .with_context(|| format!("failed to publish private relay list to {relay}"))
            {
                Ok(_) => published = true,
                Err(error) => last_error = Some(error),
            }
        }
        if !published {
            return Err(last_error.unwrap_or_else(|| {
                anyhow::anyhow!("no normal discovery relay is available for the private relay list")
            }));
        }

        let canonical = fetch_private_git_relay_list(client, discovery_relays.clone(), signer)
            .await
            .context("failed to verify the updated private Git relay list")?;
        if canonical.as_ref().is_some_and(|list| {
            repository_relays
                .iter()
                .all(|relay| list.relays.contains(relay))
        }) {
            return Ok(());
        }
        if attempt + 1 == PRIVATE_RELAY_LIST_UPDATE_ATTEMPTS {
            break;
        }
    }
    bail!(
        "private Git relay list did not converge after {PRIVATE_RELAY_LIST_UPDATE_ATTEMPTS} attempts"
    )
}

async fn ensure_private_relay_list_write_target_was_read<C: Connect + Sync>(
    client: &C,
    write_relays: &[String],
    public_key: PublicKey,
) -> Result<()> {
    if write_relays.is_empty() {
        bail!("no normal relay is available for the private relay list");
    }
    let relay_urls = write_relays
        .iter()
        .map(|relay| {
            RelayUrl::parse(relay)
                .with_context(|| format!("invalid private relay-list write relay URL: {relay}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let (results, _) = client
        .get_events_per_relay(
            relay_urls,
            vec![
                nostr::prelude::Filter::new()
                    .kind(KIND_PRIVATE_GIT_RELAY_LIST)
                    .author(public_key)
                    .limit(10),
            ],
            MultiProgress::new(),
        )
        .await
        .context("failed to read the private Git relay list from its write relays")?;
    if results.iter().any(Result::is_ok) {
        return Ok(());
    }
    let errors = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    bail!(
        "refusing to replace the private Git relay list because none of its write relays were read successfully{}",
        if errors.is_empty() {
            String::new()
        } else {
            format!(": {errors}")
        }
    )
}

struct PrivateRelayListUpdateLock {
    path: PathBuf,
}

impl PrivateRelayListUpdateLock {
    async fn acquire(public_key: &PublicKey) -> Result<Self> {
        #[cfg(test)]
        let cache_dir = std::env::temp_dir().join("ngit-private-relay-list-test-locks");
        #[cfg(not(test))]
        let cache_dir = get_dirs()?.cache_dir().to_path_buf();
        create_dir_all(&cache_dir)
            .with_context(|| format!("failed to create cache directory {}", cache_dir.display()))?;
        let path = cache_dir.join(format!(
            "private-git-relay-list-{}.lock",
            public_key.to_hex()
        ));
        let started = tokio::time::Instant::now();
        loop {
            match create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = path
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > PRIVATE_RELAY_LIST_STALE_LOCK_AGE);
                    if stale {
                        let _ = remove_dir(&path);
                        continue;
                    }
                    if started.elapsed() >= PRIVATE_RELAY_LIST_LOCK_WAIT {
                        bail!(
                            "timed out waiting for another private Git relay list update to finish"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to acquire private relay list lock {}",
                            path.display()
                        )
                    });
                }
            }
        }
    }
}

impl Drop for PrivateRelayListUpdateLock {
    fn drop(&mut self) {
        let _ = remove_dir(&self.path);
    }
}

fn validate_and_dedupe_private_git_relays(relays: Vec<RelayUrl>) -> Result<Vec<RelayUrl>> {
    let mut deduped = Vec::with_capacity(relays.len());
    for relay in relays {
        if !deduped.contains(&relay) {
            deduped.push(relay);
        }
    }
    Ok(deduped)
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
                    .fetch_all(
                        git_repo_path,
                        None,
                        &HashSet::from_iter(vec![*public_key]),
                        false,
                    )
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
                .fetch_all(
                    git_repo_path,
                    None,
                    &HashSet::from_iter(vec![*public_key]),
                    false,
                )
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

#[cfg(test)]
mod private_git_relay_list_tests {
    use std::sync::Mutex;

    use nostr::prelude::{
        Keys,
        event::{FinalizeUnsignedEvent, SignEvent},
    };

    use super::*;

    fn test_signer() -> (Keys, Arc<crate::NgitSigner>) {
        let keys = Keys::generate();
        let signer = Arc::new(crate::NgitSigner::Keys(keys.clone()));
        (keys, signer)
    }

    async fn private_list_event_with_plaintext(
        keys: &Keys,
        signer: &Arc<crate::NgitSigner>,
        plaintext: &str,
        tags: Vec<Tag>,
    ) -> Event {
        let content = signer
            .nip44_encrypt(&keys.public_key(), plaintext)
            .await
            .unwrap();
        keys.sign_event(
            EventBuilder::new(KIND_PRIVATE_GIT_RELAY_LIST, content)
                .tags(tags)
                .finalize_unsigned(keys.public_key()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn private_git_relay_list_round_trips_without_public_urls() {
        let (_keys, signer) = test_signer();
        let relay_a = RelayUrl::parse("wss://private-a.example").unwrap();
        let relay_b = RelayUrl::parse("ws://private-b.example").unwrap();
        let mut list =
            PrivateGitRelayList::new(vec![relay_a.clone(), relay_b.clone(), relay_a.clone()])
                .unwrap();

        assert_eq!(list.relays, vec![relay_a.clone(), relay_b.clone()]);
        let event = list.to_event(&signer).await.unwrap();
        assert_eq!(event.kind, KIND_PRIVATE_GIT_RELAY_LIST);
        assert!(event.tags.is_empty());
        assert!(!event.content.contains(relay_a.as_str()));
        assert!(!event.content.contains(relay_b.as_str()));

        let decoded = PrivateGitRelayList::from_event(&event, &signer)
            .await
            .unwrap();
        assert_eq!(decoded.relays, vec![relay_a, relay_b]);
        assert_eq!(decoded.created_at, event.created_at);
    }

    #[tokio::test]
    async fn private_git_relay_list_cache_uses_the_concrete_event_id() {
        let (keys, signer) = test_signer();
        let event = private_list_event_with_plaintext(
            &keys,
            &signer,
            r#"[["g","wss://encrypted.example"]]"#,
            vec![],
        )
        .await;
        let temporary = tempfile::tempdir().unwrap();
        let cache_dir = temporary.path().join(PRIVATE_RELAY_LIST_CACHE_DIR);

        let decrypted =
            PrivateGitRelayList::from_event_with_cache_dir(&event, &signer, Some(&cache_dir))
                .await
                .unwrap();
        assert_eq!(
            decrypted.relays,
            vec![RelayUrl::parse("wss://encrypted.example").unwrap()]
        );
        let cache_path = private_git_relay_list_cache_path(&cache_dir, &event);
        assert_eq!(
            cache_path.file_name().unwrap(),
            format!("{}.json", event.id.to_hex()).as_str()
        );

        write_cached_private_git_relay_list(
            &cache_dir,
            &event,
            r#"[["g","wss://cached.example"]]"#,
        )
        .unwrap();
        let cached =
            PrivateGitRelayList::from_event_with_cache_dir(&event, &signer, Some(&cache_dir))
                .await
                .unwrap();
        assert_eq!(
            cached.relays,
            vec![RelayUrl::parse("wss://cached.example").unwrap()],
            "a valid concrete-event cache entry must avoid decrypting again"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&cache_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(cache_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn invalid_private_git_relay_list_cache_is_replaced_after_decryption() {
        let (keys, signer) = test_signer();
        let plaintext = r#"[["g","wss://private.example"]]"#;
        let event = private_list_event_with_plaintext(&keys, &signer, plaintext, vec![]).await;
        let temporary = tempfile::tempdir().unwrap();
        let cache_dir = temporary.path().join(PRIVATE_RELAY_LIST_CACHE_DIR);
        write_cached_private_git_relay_list(&cache_dir, &event, "not JSON").unwrap();

        let decoded =
            PrivateGitRelayList::from_event_with_cache_dir(&event, &signer, Some(&cache_dir))
                .await
                .unwrap();
        assert_eq!(
            decoded.relays,
            vec![RelayUrl::parse("wss://private.example").unwrap()]
        );
        assert_eq!(
            fs::read_to_string(private_git_relay_list_cache_path(&cache_dir, &event)).unwrap(),
            plaintext
        );
    }

    #[tokio::test]
    async fn publishing_private_relay_list_adds_acceptance_relays() {
        let (keys, signer) = test_signer();
        let normal_relay = "wss://normal.example".to_string();
        let repository_relay = RelayUrl::parse("wss://group.example").unwrap();
        let user_ref = UserRef {
            public_key: keys.public_key(),
            metadata: UserMetadata {
                name: String::new(),
                created_at: Timestamp::from(0),
                nip05: None,
            },
            relays: UserRelays {
                relays: vec![UserRelayRef {
                    url: normal_relay.clone(),
                    read: true,
                    write: true,
                }],
                created_at: Timestamp::from(0),
            },
            grasp_list: UserGraspList {
                urls: vec![],
                created_at: Timestamp::from(0),
            },
        };

        let published = Arc::new(Mutex::new(None));
        let published_for_mock = published.clone();
        let published_for_fetch = published.clone();
        let mut client = <MockConnect as Default>::default();
        client
            .expect_get_events_per_relay()
            .once()
            .return_once(|_, _, progress| Ok((vec![Ok(vec![])], progress)));
        client.expect_get_events().times(2).returning(move |_, _| {
            Ok(published_for_fetch
                .lock()
                .unwrap()
                .clone()
                .into_iter()
                .collect())
        });
        client
            .expect_send_event_to()
            .once()
            .withf(move |path, relay, _| path.is_none() && relay == normal_relay)
            .return_once(move |_, _, event| {
                let id = event.id;
                *published_for_mock.lock().unwrap() = Some(event);
                Ok(id)
            });

        publish_private_git_relay_list(
            &client,
            std::slice::from_ref(&repository_relay),
            &user_ref,
            &signer,
        )
        .await
        .unwrap();

        let event = published.lock().unwrap().clone().unwrap();
        let decoded = PrivateGitRelayList::from_event(&event, &signer)
            .await
            .unwrap();
        assert_eq!(decoded.relays, vec![repository_relay]);
        assert!(event.tags.is_empty());
    }

    #[tokio::test]
    async fn unavailable_private_relay_discovery_is_not_treated_as_absent() {
        let (_keys, signer) = test_signer();
        let mut client = <MockConnect as Default>::default();
        client
            .expect_get_events()
            .once()
            .return_once(|_, _| Err(anyhow::anyhow!("all discovery relays failed")));

        assert!(matches!(
            discover_private_git_relay_list(
                &client,
                vec!["wss://offline.example".to_string()],
                &signer,
            )
            .await,
            PrivateGitRelayDiscovery::Unavailable(error)
                if error.contains("all discovery relays failed")
        ));
    }

    #[tokio::test]
    async fn publishing_private_relay_list_merges_a_concurrent_winner() {
        let (keys, signer) = test_signer();
        let normal_relay = "wss://normal.example".to_string();
        let requested_relay = RelayUrl::parse("wss://requested.example").unwrap();
        let concurrent_relay = RelayUrl::parse("wss://concurrent.example").unwrap();
        let user_ref = UserRef {
            public_key: keys.public_key(),
            metadata: UserMetadata {
                name: String::new(),
                created_at: Timestamp::from(0),
                nip05: None,
            },
            relays: UserRelays {
                relays: vec![UserRelayRef {
                    url: normal_relay,
                    read: true,
                    write: true,
                }],
                created_at: Timestamp::from(0),
            },
            grasp_list: UserGraspList {
                urls: vec![],
                created_at: Timestamp::from(0),
            },
        };
        let concurrent_event = PrivateGitRelayList::new(vec![concurrent_relay.clone()])
            .unwrap()
            .to_event(&signer)
            .await
            .unwrap();
        let call = Arc::new(Mutex::new(0usize));
        let published = Arc::new(Mutex::new(None));
        let call_for_fetch = call.clone();
        let published_for_fetch = published.clone();
        let published_for_send = published.clone();
        let mut client = <MockConnect as Default>::default();
        client
            .expect_get_events_per_relay()
            .once()
            .return_once(|_, _, progress| Ok((vec![Ok(vec![])], progress)));
        client.expect_get_events().times(4).returning(move |_, _| {
            let mut call = call_for_fetch.lock().unwrap();
            *call += 1;
            Ok(match *call {
                1 => vec![],
                2 | 3 => vec![concurrent_event.clone()],
                _ => published_for_fetch
                    .lock()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .collect(),
            })
        });
        client
            .expect_send_event_to()
            .times(2)
            .returning(move |_, _, event| {
                let id = event.id;
                *published_for_send.lock().unwrap() = Some(event);
                Ok(id)
            });

        publish_private_git_relay_list(
            &client,
            std::slice::from_ref(&requested_relay),
            &user_ref,
            &signer,
        )
        .await
        .unwrap();

        let event = published.lock().unwrap().clone().unwrap();
        let decoded = PrivateGitRelayList::from_event(&event, &signer)
            .await
            .unwrap();
        assert!(decoded.relays.contains(&requested_relay));
        assert!(decoded.relays.contains(&concurrent_relay));
    }

    #[tokio::test]
    async fn publishing_private_relay_list_requires_a_successful_outbox_read() {
        let (keys, signer) = test_signer();
        let inbox_relay = "wss://inbox.example".to_string();
        let outbox_relay = "wss://outbox.example".to_string();
        let repository_relay = RelayUrl::parse("wss://group.example").unwrap();
        let user_ref = UserRef {
            public_key: keys.public_key(),
            metadata: UserMetadata {
                name: String::new(),
                created_at: Timestamp::from(0),
                nip05: None,
            },
            relays: UserRelays {
                relays: vec![
                    UserRelayRef {
                        url: inbox_relay,
                        read: true,
                        write: false,
                    },
                    UserRelayRef {
                        url: outbox_relay.clone(),
                        read: false,
                        write: true,
                    },
                ],
                created_at: Timestamp::from(0),
            },
            grasp_list: UserGraspList {
                urls: vec![],
                created_at: Timestamp::from(0),
            },
        };

        let expected_outbox = RelayUrl::parse(&outbox_relay).unwrap();
        let mut client = <MockConnect as Default>::default();
        client
            .expect_get_events_per_relay()
            .once()
            .withf(move |relays, _, _| relays == std::slice::from_ref(&expected_outbox))
            .return_once(|_, _, progress| {
                Ok((vec![Err(anyhow::anyhow!("outbox unavailable"))], progress))
            });
        client.expect_get_events().never();
        client.expect_send_event_to().never();

        let error = publish_private_git_relay_list(
            &client,
            std::slice::from_ref(&repository_relay),
            &user_ref,
            &signer,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("none of its write relays"));
    }

    #[tokio::test]
    async fn private_git_relay_list_rejects_non_websocket_and_malformed_items() {
        let (keys, signer) = test_signer();
        for plaintext in [
            r#"[["g","https://private.example"]]"#,
            r#"[["r","wss://private.example"]]"#,
            r#"[["g","wss://private.example","extra"]]"#,
            r#"{"g":"wss://private.example"}"#,
        ] {
            let event = private_list_event_with_plaintext(&keys, &signer, plaintext, vec![]).await;
            assert!(
                PrivateGitRelayList::from_event(&event, &signer)
                    .await
                    .is_err(),
                "unexpectedly accepted {plaintext}"
            );
        }
    }

    #[tokio::test]
    async fn private_git_relay_list_rejects_public_tags_and_other_authors() {
        let (keys, signer) = test_signer();
        let event = private_list_event_with_plaintext(
            &keys,
            &signer,
            r#"[["g","wss://private.example"]]"#,
            vec![Tag::parse(["g", "wss://leaked.example"]).unwrap()],
        )
        .await;
        assert!(
            PrivateGitRelayList::from_event(&event, &signer)
                .await
                .is_err()
        );

        let (_, other_signer) = test_signer();
        let private_event = private_list_event_with_plaintext(
            &keys,
            &signer,
            r#"[["g","wss://private.example"]]"#,
            vec![],
        )
        .await;
        assert!(
            PrivateGitRelayList::from_event(&private_event, &other_signer)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn private_git_relay_list_uses_nip01_id_tiebreak_and_orders_replacement() {
        let (keys, signer) = test_signer();
        let created_at = Timestamp::now();
        let relay_a = RelayUrl::parse("wss://private-a.example").unwrap();
        let relay_b = RelayUrl::parse("wss://private-b.example").unwrap();
        let event_a = private_list_event_with_plaintext(
            &keys,
            &signer,
            r#"[["g","wss://private-a.example"]]"#,
            vec![],
        )
        .await;
        let event_a = keys
            .sign_event(
                EventBuilder::new(KIND_PRIVATE_GIT_RELAY_LIST, event_a.content)
                    .custom_created_at(created_at)
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();
        let event_b = private_list_event_with_plaintext(
            &keys,
            &signer,
            r#"[["g","wss://private-b.example"]]"#,
            vec![],
        )
        .await;
        let event_b = keys
            .sign_event(
                EventBuilder::new(KIND_PRIVATE_GIT_RELAY_LIST, event_b.content)
                    .custom_created_at(created_at)
                    .finalize_unsigned(keys.public_key()),
            )
            .unwrap();
        let (winner, loser, expected_relay) = if event_a.id < event_b.id {
            (event_a, event_b, relay_a)
        } else {
            (event_b, event_a, relay_b)
        };

        let winner_for_fetch = winner.clone();
        let mut client = <MockConnect as Default>::default();
        client
            .expect_get_events()
            .once()
            .return_once(move |_, _| Ok(vec![loser, winner_for_fetch]));
        let mut fetched = fetch_private_git_relay_list(
            &client,
            vec!["wss://discovery.example".to_string()],
            &signer,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fetched.relays, vec![expected_relay]);

        let replacement = fetched.to_event(&signer).await.unwrap();
        assert!(
            replacement.tags.is_empty(),
            "ordered replacements must not expose a nonce or relay tag"
        );
        assert_eq!(
            crate::event_ordering::latest_event([&winner, &replacement])
                .unwrap()
                .id,
            replacement.id,
            "a newly signed kind-10318 replacement must win NIP-01 ordering"
        );
    }

    #[test]
    fn private_relay_hints_are_added_before_repository_discovery() {
        let keys = Keys::generate();
        let public_hint = RelayUrl::parse("wss://public-hint.example").unwrap();
        let private_hint = RelayUrl::parse("wss://private-hint.example").unwrap();
        let mut coordinate = nostr::nips::nip19::Nip19Coordinate {
            coordinate: nostr::nips::nip01::Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: keys.public_key(),
                identifier: "repo".to_string(),
            },
            relays: vec![public_hint.clone()],
        };

        assert!(add_private_git_relay_hints(
            &mut coordinate,
            std::slice::from_ref(&private_hint)
        ));
        assert_eq!(coordinate.relays, vec![public_hint, private_hint.clone()]);
        assert!(!add_private_git_relay_hints(&mut coordinate, &[]));
        assert!(
            !PrivateGitRelayDiscovery::Absent.requires_repository_only_probe(),
            "an ordinary naddr relay hint is not evidence of private discovery"
        );
        assert!(
            PrivateGitRelayDiscovery::Available(vec![private_hint])
                .requires_repository_only_probe()
        );
    }
}
