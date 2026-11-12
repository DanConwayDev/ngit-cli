use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use futures::future::join_all;
use nostr::prelude::RelayUrl;
use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};

const NIP11_TIMEOUT: Duration = Duration::from_secs(5);
const NIP11_MEDIA_TYPE: &str = "application/nostr+json";
const BUZZ_SOFTWARE: &str = "https://github.com/block/buzz";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RelayInformationDocument {
    #[serde(default)]
    supported_grasps: Vec<String>,
    software: Option<String>,
    version: Option<String>,
    // Retain the complete document for future NIP-11 consumers.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

impl RelayInformationDocument {
    fn advertises_private_repository_transport(&self) -> bool {
        self.supported_grasps
            .iter()
            .any(|grasp| grasp.eq_ignore_ascii_case("GRASP-08"))
            || self.software.as_deref().is_some_and(|software| {
                software
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(BUZZ_SOFTWARE)
            })
    }
}

const REFRESH_AFTER: u64 = 24 * 60 * 60;
const RETRY_AFTER: u64 = 5 * 60;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
struct CachedRelayInformation {
    relay: String,
    fetched_at: u64,
    attempted_at: u64,
    document: Option<RelayInformationDocument>,
}

impl CachedRelayInformation {
    fn needs_refresh(&self, now: u64) -> bool {
        let elapsed = |then| now.checked_sub(then).unwrap_or(u64::MAX);
        (self.document.is_none() || elapsed(self.fetched_at) >= REFRESH_AFTER)
            && elapsed(self.attempted_at) >= RETRY_AFTER
    }
}

/// Each normalized relay URL (including its mount path) has its own atomic
/// document and nonblocking refresh lock. Unrelated processes/relays cannot
/// overwrite one another's entries. Timestamps control refresh, never validity.
struct RelayInformationCache {
    directory: PathBuf,
}

impl RelayInformationCache {
    fn global() -> Result<Self> {
        #[cfg(not(test))]
        let directory = crate::client::get_global_cache_dir()?.join("relay-information");
        #[cfg(test)]
        let directory = {
            // Production callers in unit tests must never touch the user's cache.
            static CACHE: OnceLock<tempfile::TempDir> = OnceLock::new();
            CACHE
                .get_or_init(|| tempfile::tempdir().expect("test cache directory"))
                .path()
                .to_path_buf()
        };
        Ok(Self { directory })
    }

    fn path(&self, relay: &RelayUrl) -> PathBuf {
        let key = bitcoin_hashes::sha256::Hash::hash(relay.as_str().as_bytes());
        self.directory.join(format!("{key}.json"))
    }

    fn read(&self, relay: &RelayUrl) -> Option<CachedRelayInformation> {
        let mut bytes = Vec::new();
        File::open(self.path(relay))
            .ok()?
            .take((MAX_DOCUMENT_BYTES + 4096 + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_DOCUMENT_BYTES + 4096 {
            return None;
        }
        let entry: CachedRelayInformation = serde_json::from_slice(&bytes).ok()?;
        (entry.relay == relay.as_str()).then_some(entry)
    }

    fn lock(&self, relay: &RelayUrl) -> Result<File> {
        std::fs::create_dir_all(&self.directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path(relay).with_extension("lock"))?;
        lock.try_lock()?;
        Ok(lock)
    }

    fn write(&self, relay: &RelayUrl, entry: &CachedRelayInformation) -> Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(&mut temporary, entry)?;
        temporary.flush()?;
        temporary.persist(self.path(relay))?;
        Ok(())
    }

    fn start_refresh(
        &self,
        relay: &RelayUrl,
        now: u64,
    ) -> Result<Option<(File, CachedRelayInformation)>> {
        let lock = self.lock(relay)?;
        let mut entry = self.read(relay).unwrap_or_else(|| CachedRelayInformation {
            relay: relay.to_string(),
            ..Default::default()
        });
        if !entry.needs_refresh(now) {
            return Ok(None);
        }
        // Persist the attempt before starting I/O. A failed/cancelled refresh
        // keeps the last good document and still backs off across CLI processes.
        entry.attempted_at = now;
        self.write(relay, &entry)?;
        Ok(Some((lock, entry)))
    }

    async fn refresh(&self, relay: &RelayUrl, mut entry: CachedRelayInformation) -> Result<()> {
        let client = crate::tls::http_client_builder()
            .timeout(NIP11_TIMEOUT)
            .build()?;
        let document = fetch_document(&client, relay).await?;
        entry.fetched_at = now_seconds();
        entry.document = Some(document);
        self.write(relay, &entry)
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Optional metadata jobs for one complete fetch report. Only announcement-
/// confirmed repository relays are added. Dropping the report cancels every
/// unfinished HTTP request; no relay result waits for metadata.
#[derive(Default)]
pub(crate) struct RelayInformationRefreshes {
    jobs: Mutex<HashMap<RelayUrl, Option<tokio::task::JoinHandle<()>>>>,
}

impl RelayInformationRefreshes {
    pub(crate) fn start(&self, relays: &[RelayUrl]) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|error| error.into_inner());
        for relay in relays {
            jobs.entry(relay.clone())
                .or_insert_with(|| start_refresh_task(relay));
        }
    }
}

impl Drop for RelayInformationRefreshes {
    fn drop(&mut self) {
        for task in self
            .jobs
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .flatten()
        {
            task.abort();
        }
    }
}

fn start_refresh_task(relay: &RelayUrl) -> Option<tokio::task::JoinHandle<()>> {
    // This HTTP client has no SOCKS transport. Never resolve onion names
    // directly or bypass the WebSocket/Git Tor policy to warm optional metadata.
    if crate::git::nostr_url::host_is_onion(relay.as_str()) {
        return None;
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let cache = RelayInformationCache::global().ok()?;
    let (lock, entry) = cache.start_refresh(relay, now_seconds()).ok()??;
    let relay = relay.clone();
    Some(runtime.spawn(async move {
        let _lock = lock;
        if let Err(error) = cache.refresh(&relay, entry).await {
            if crate::output_mode::is_verbose() {
                eprintln!("nostr: NIP-11 background refresh for {relay} failed: {error:#}");
            }
        }
    }))
}

fn cached_document(relay: &RelayUrl) -> Option<RelayInformationDocument> {
    RelayInformationCache::global().ok()?.read(relay)?.document
}

/// Temporary compatibility exception, not a protocol capability: GRASP
/// requires purgatory. Pyramid currently exposes PR events before Git arrives.
/// Remove this exception once supported Pyramid releases implement purgatory.
pub(crate) fn cached_hint_needs_git_first(relay: &RelayUrl) -> bool {
    cached_document(relay).is_some_and(|document| document.is_pyramid())
}

impl RelayInformationDocument {
    fn is_pyramid(&self) -> bool {
        let Some(software) = self.software.as_deref() else {
            return false;
        };
        let Ok(url) = reqwest::Url::parse(software) else {
            return false;
        };
        // Include Pyramid forks, without matching unrelated names/substrings.
        let segments: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
        url.host_str() == Some("github.com")
            && segments.len() == 3
            && !segments[1].is_empty()
            && (segments[2].eq_ignore_ascii_case("pyramid")
                || segments[2].eq_ignore_ascii_case("pyramid.git"))
    }
}

fn remember_document(relay: &RelayUrl, document: &RelayInformationDocument) {
    let Ok(cache) = RelayInformationCache::global() else {
        return;
    };
    let Ok(_lock) = cache.lock(relay) else { return };
    let now = now_seconds();
    let _ = cache.write(
        relay,
        &CachedRelayInformation {
            relay: relay.to_string(),
            fetched_at: now,
            attempted_at: now,
            document: Some(document.clone()),
        },
    );
}

/// Successful NIP-11 classifications are cached for the lifetime of the
/// process so repeated operations don't pay the probe timeout again. Failed
/// probes are not cached: they already degrade to "not private" for the
/// current call and a later probe may succeed.
fn nip11_classification_cache() -> &'static Mutex<HashMap<RelayUrl, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<RelayUrl, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Classify only from existing metadata; this never starts network work.
pub fn cached_private_transport_classification(relay: &RelayUrl) -> Option<bool> {
    if let Some(document) = cached_document(relay) {
        return Some(document.advertises_private_repository_transport());
    }
    nip11_classification_cache()
        .lock()
        .ok()?
        .get(relay)
        .copied()
}

fn store_private_transport_classification(relay: &RelayUrl, advertises: bool) {
    if let Ok(mut cache) = nip11_classification_cache().lock() {
        cache.insert(relay.clone(), advertises);
    }
}

/// Return relay hints whose public NIP-11 documents identify a private Git
/// transport. Unavailable or malformed documents are ignored so encrypted
/// kind-10318 discovery remains available as the authoritative account
/// signal. Classifications from earlier probes in this process are reused
/// without touching the network.
pub async fn discover_private_repository_relays(relays: &[RelayUrl]) -> Vec<RelayUrl> {
    let mut classifications: HashMap<RelayUrl, bool> = HashMap::new();
    let mut to_probe: Vec<RelayUrl> = vec![];
    for relay in relays {
        if let Some(document) = cached_document(relay) {
            classifications.insert(
                relay.clone(),
                document.advertises_private_repository_transport(),
            );
            continue;
        }
        match cached_private_transport_classification(relay) {
            Some(advertises) => {
                classifications.insert(relay.clone(), advertises);
            }
            None => {
                if !to_probe.contains(relay) {
                    to_probe.push(relay.clone());
                }
            }
        }
    }
    if !to_probe.is_empty() {
        if let Ok(client) = crate::tls::http_client_builder()
            .timeout(NIP11_TIMEOUT)
            .build()
        {
            let results = join_all(to_probe.into_iter().map(|relay| {
                let client = client.clone();
                async move {
                    let result = relay_advertises_private_repository(&client, &relay).await;
                    (relay, result)
                }
            }))
            .await;
            for (relay, result) in results {
                match result {
                    Ok(advertises) => {
                        store_private_transport_classification(&relay, advertises);
                        classifications.insert(relay, advertises);
                    }
                    Err(error) => {
                        if crate::output_mode::is_verbose() {
                            eprintln!("nostr: ignoring unavailable NIP-11 document: {error:#}");
                        }
                    }
                }
            }
        }
    }
    relays
        .iter()
        .filter(|relay| classifications.get(*relay) == Some(&true))
        .cloned()
        .collect()
}

async fn relay_advertises_private_repository(
    client: &reqwest::Client,
    relay: &RelayUrl,
) -> Result<bool> {
    let document = fetch_document(client, relay).await?;
    remember_document(relay, &document);
    Ok(document.advertises_private_repository_transport())
}

async fn fetch_document(
    client: &reqwest::Client,
    relay: &RelayUrl,
) -> Result<RelayInformationDocument> {
    let url = nip11_url(relay)?;
    let mut response = client
        .get(url)
        .header(ACCEPT, NIP11_MEDIA_TYPE)
        .send()
        .await
        .with_context(|| format!("failed to request NIP-11 from {relay}"))?
        .error_for_status()
        .with_context(|| format!("NIP-11 request to {relay} failed"))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            bail!("NIP-11 document from {relay} exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).with_context(|| format!("invalid NIP-11 document from {relay}"))
}

fn nip11_url(relay: &RelayUrl) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(relay.as_str()).context("invalid relay URL for NIP-11")?;
    let http_scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        scheme => bail!("unsupported relay URL scheme for NIP-11: {scheme}"),
    };
    url.set_scheme(http_scheme)
        .map_err(|_| anyhow::anyhow!("failed to set NIP-11 URL scheme"))?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pyramid_document() -> RelayInformationDocument {
        serde_json::from_value(serde_json::json!({
            "software": "https://github.com/fiatjaf/pyramid", "version": "v1.4.4",
            "supported_nips": [1, 11, 34], "vendor_extension": {"kept": true}
        }))
        .unwrap()
    }

    #[test]
    fn pyramid_compatibility_matches_implementations_not_host_names() {
        for software in [
            "https://github.com/fiatjaf/pyramid",
            "https://github.com/arbadacarbaYK/pyramid/",
            "https://github.com/some-fork/pyramid.git",
        ] {
            let document = RelayInformationDocument {
                software: Some(software.into()),
                ..Default::default()
            };
            assert!(document.is_pyramid(), "{software}");
        }
        for software in [
            "https://github.com/fiatjaf/not-pyramid",
            "https://github.com/fiatjaf/pyramid/unrelated",
            "https://example.com/fiatjaf/pyramid",
            "https://gitworkshop.dev/danconwaydev.com/ngit-grasp",
            "pyramid",
        ] {
            let document = RelayInformationDocument {
                software: Some(software.into()),
                ..Default::default()
            };
            assert!(!document.is_pyramid(), "{software}");
        }
        assert!(!RelayInformationDocument::default().is_pyramid());
    }

    #[test]
    fn persistent_entries_survive_age_and_preserve_unknown_fields() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cache = RelayInformationCache {
            directory: directory.path().into(),
        };
        let relay = RelayUrl::parse("wss://example.com/mounted")?;
        let _lock = cache.lock(&relay)?;
        cache.write(
            &relay,
            &CachedRelayInformation {
                relay: relay.to_string(),
                fetched_at: 1,
                attempted_at: 1,
                document: Some(pyramid_document()),
            },
        )?;
        let reopened = RelayInformationCache {
            directory: directory.path().into(),
        };
        let entry = reopened.read(&relay).context("persistent entry missing")?;
        assert!(entry.needs_refresh(10 * REFRESH_AFTER));
        let document = entry.document.context("stale document was discarded")?;
        assert!(document.is_pyramid());
        assert_eq!(document.extra["vendor_extension"]["kept"], true);
        assert_eq!(document.version.as_deref(), Some("v1.4.4"));
        assert!(
            reopened
                .read(&RelayUrl::parse("wss://example.com/other")?)
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn refresh_lock_and_retry_cooldown_preserve_last_good_document() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cache = RelayInformationCache {
            directory: directory.path().into(),
        };
        let relay = RelayUrl::parse("wss://example.com/")?;
        let now = 3 * REFRESH_AFTER;
        let lock = cache.lock(&relay)?;
        cache.write(
            &relay,
            &CachedRelayInformation {
                relay: relay.to_string(),
                fetched_at: 1,
                attempted_at: 1,
                document: Some(pyramid_document()),
            },
        )?;
        drop(lock);
        let (lock, _) = cache
            .start_refresh(&relay, now)?
            .context("refresh should start")?;
        assert!(
            cache.start_refresh(&relay, now).is_err(),
            "refreshes must not race"
        );
        drop(lock); // Simulate a failed/cancelled request, even in another CLI process.
        let reopened = RelayInformationCache {
            directory: directory.path().into(),
        };
        assert!(
            reopened
                .start_refresh(&relay, now + RETRY_AFTER - 1)?
                .is_none()
        );
        assert!(
            reopened
                .read(&relay)
                .unwrap()
                .document
                .unwrap()
                .is_pyramid()
        );
        assert!(reopened.start_refresh(&relay, now + RETRY_AFTER)?.is_some());
        Ok(())
    }

    #[test]
    fn fresh_entries_and_clock_changes_have_bounded_refresh_schedules() {
        let entry = CachedRelayInformation {
            relay: "wss://example.com/".into(),
            fetched_at: 1000,
            attempted_at: 1000,
            document: Some(pyramid_document()),
        };
        assert!(!entry.needs_refresh(1000 + REFRESH_AFTER - 1));
        assert!(entry.needs_refresh(1000 + REFRESH_AFTER));
        assert!(
            entry.needs_refresh(999),
            "clock rollback must not freeze refresh"
        );
    }

    #[test]
    fn corrupt_or_wrong_relay_entries_are_cache_misses() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cache = RelayInformationCache {
            directory: directory.path().into(),
        };
        let relay = RelayUrl::parse("wss://example.com/")?;
        std::fs::write(cache.path(&relay), b"{broken")?;
        assert!(cache.read(&relay).is_none());
        std::fs::write(
            cache.path(&relay),
            serde_json::to_vec(&CachedRelayInformation {
                relay: "wss://another.example/".into(),
                document: Some(pyramid_document()),
                ..Default::default()
            })?,
        )?;
        assert!(cache.read(&relay).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn background_refresh_keeps_stale_data_until_http_succeeds() -> Result<()> {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            sync::oneshot,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let relay = RelayUrl::parse(&format!("ws://{}/mount", listener.local_addr()?))?;
        let cache = RelayInformationCache::global()?;
        let lock = cache.lock(&relay)?;
        cache.write(
            &relay,
            &CachedRelayInformation {
                relay: relay.to_string(),
                fetched_at: 1,
                attempted_at: 1,
                document: Some(pyramid_document()),
            },
        )?;
        drop(lock);
        let (requested, request) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request_bytes = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let size = stream.read(&mut chunk).await?;
                anyhow::ensure!(size > 0, "request closed");
                request_bytes.extend_from_slice(&chunk[..size]);
                if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request_text = String::from_utf8(request_bytes)?;
            assert!(request_text.starts_with("GET /mount "));
            assert!(
                request_text
                    .to_ascii_lowercase()
                    .contains("accept: application/nostr+json")
            );
            assert!(!request_text.to_ascii_lowercase().contains("authorization:"));
            requested.send(()).unwrap();
            released.await?;
            let body = r#"{"software":"https://example.com/new-implementation","version":"2","custom":{"preserved":true}}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            anyhow::Ok(())
        });
        let refreshes = RelayInformationRefreshes::default();
        refreshes.start(std::slice::from_ref(&relay)); // Returns before the server replies.
        tokio::time::timeout(Duration::from_secs(2), request).await??;
        assert!(cached_hint_needs_git_first(&relay));
        refreshes.start(std::slice::from_ref(&relay)); // In-flight refresh is deduplicated.
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while cached_hint_needs_git_first(&relay) {
                tokio::task::yield_now().await
            }
        })
        .await?;
        let document = cached_document(&relay).context("replacement missing")?;
        assert_eq!(document.extra["custom"]["preserved"], true);
        assert_eq!(document.version.as_deref(), Some("2"));
        tokio::time::timeout(Duration::from_secs(2), server).await???;
        Ok(())
    }

    #[tokio::test]
    async fn finishing_fetch_cancels_metadata_and_preserves_stale_document() -> Result<()> {
        use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot};
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let relay = RelayUrl::parse(&format!("ws://{}", listener.local_addr()?))?;
        let cache = RelayInformationCache::global()?;
        let lock = cache.lock(&relay)?;
        cache.write(
            &relay,
            &CachedRelayInformation {
                relay: relay.to_string(),
                fetched_at: 1,
                attempted_at: 1,
                document: Some(pyramid_document()),
            },
        )?;
        drop(lock);
        let (requested, request) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request_bytes = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let size = stream.read(&mut buffer).await?;
                anyhow::ensure!(size > 0, "request closed early");
                request_bytes.extend_from_slice(&buffer[..size]);
                if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            requested.send(()).unwrap();
            let mut buffer = [0; 1024];
            assert_eq!(
                stream.read(&mut buffer).await?,
                0,
                "request was not cancelled"
            );
            anyhow::Ok(())
        });
        let refreshes = RelayInformationRefreshes::default();
        refreshes.start(std::slice::from_ref(&relay));
        tokio::time::timeout(Duration::from_secs(2), request).await??;
        assert!(cached_hint_needs_git_first(&relay));
        drop(refreshes);
        tokio::time::timeout(Duration::from_secs(2), server).await???;
        assert!(cached_hint_needs_git_first(&relay));
        assert!(!cache.read(&relay).unwrap().needs_refresh(now_seconds()));
        Ok(())
    }

    #[tokio::test]
    async fn invalid_http_document_does_not_replace_stale_entry() -> Result<()> {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let relay = RelayUrl::parse(&format!("ws://{}", listener.local_addr()?))?;
        let directory = tempfile::tempdir()?;
        let cache = RelayInformationCache {
            directory: directory.path().into(),
        };
        let lock = cache.lock(&relay)?;
        cache.write(
            &relay,
            &CachedRelayInformation {
                relay: relay.to_string(),
                fetched_at: 1,
                attempted_at: 1,
                document: Some(pyramid_document()),
            },
        )?;
        drop(lock);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let read = stream.read(&mut buffer).await?;
                anyhow::ensure!(read > 0, "request closed before its headers");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                anyhow::ensure!(request.len() <= 8192, "oversized test request");
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops")
                .await?;
            anyhow::Ok(())
        });
        let (lock, entry) = cache.start_refresh(&relay, now_seconds())?.unwrap();
        assert!(cache.refresh(&relay, entry).await.is_err());
        drop(lock);
        assert!(cache.read(&relay).unwrap().document.unwrap().is_pyramid());
        assert!(cache.start_refresh(&relay, now_seconds())?.is_none());
        tokio::time::timeout(Duration::from_secs(2), server).await???;
        Ok(())
    }

    #[test]
    fn grasp08_and_buzz_documents_identify_private_repository_relays() {
        for json in [
            r#"{"supported_grasps":["GRASP-08"]}"#,
            r#"{"software":"https://github.com/block/buzz"}"#,
            r#"{"software":"https://github.com/block/buzz/"}"#,
        ] {
            let document: RelayInformationDocument = serde_json::from_str(json).unwrap();
            assert!(document.advertises_private_repository_transport());
        }
    }

    #[test]
    fn generic_authenticated_relay_is_not_assumed_private() {
        let document: RelayInformationDocument = serde_json::from_str(
            r#"{"supported_nips":[42,98],"limitation":{"auth_required":true}}"#,
        )
        .unwrap();
        assert!(!document.advertises_private_repository_transport());
    }

    #[test]
    fn websocket_relay_url_becomes_http_nip11_url() {
        let public = RelayUrl::parse("wss://relay.example/nostr").unwrap();
        assert_eq!(
            nip11_url(&public).unwrap().as_str(),
            "https://relay.example/nostr"
        );
        let local = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        assert_eq!(
            nip11_url(&local).unwrap().as_str(),
            "http://127.0.0.1:7777/"
        );
    }

    #[test]
    fn classification_cache_round_trips() {
        let relay = RelayUrl::parse("wss://cache-round-trip.example").unwrap();
        assert_eq!(cached_private_transport_classification(&relay), None);
        store_private_transport_classification(&relay, true);
        assert_eq!(cached_private_transport_classification(&relay), Some(true));
    }

    /// Nothing listens on these addresses, so a probe would classify them as
    /// not-private; only the in-process cache can produce these results.
    #[tokio::test]
    async fn cached_classifications_short_circuit_network_probes() {
        let private = RelayUrl::parse("ws://127.0.0.1:1/cached-private").unwrap();
        let public = RelayUrl::parse("ws://127.0.0.1:1/cached-public").unwrap();
        store_private_transport_classification(&private, true);
        store_private_transport_classification(&public, false);
        assert_eq!(
            discover_private_repository_relays(&[public, private.clone()]).await,
            vec![private]
        );
    }
}
