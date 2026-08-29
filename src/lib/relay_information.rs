use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures::future::join_all;
use nostr::prelude::RelayUrl;
use reqwest::header::ACCEPT;
use serde::Deserialize;

const NIP11_TIMEOUT: Duration = Duration::from_secs(5);
const NIP11_MEDIA_TYPE: &str = "application/nostr+json";
const BUZZ_SOFTWARE: &str = "https://github.com/block/buzz";

#[derive(Debug, Default, Deserialize)]
struct RelayInformationDocument {
    #[serde(default)]
    supported_grasps: Vec<String>,
    software: Option<String>,
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

/// Successful NIP-11 classifications are cached for the lifetime of the
/// process so repeated operations don't pay the probe timeout again. Failed
/// probes are not cached: they already degrade to "not private" for the
/// current call and a later probe may succeed.
fn nip11_classification_cache() -> &'static Mutex<HashMap<RelayUrl, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<RelayUrl, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_private_transport_classification(relay: &RelayUrl) -> Option<bool> {
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
                        if crate::client::is_verbose() {
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
    let url = nip11_url(relay)?;
    let response = client
        .get(url)
        .header(ACCEPT, NIP11_MEDIA_TYPE)
        .send()
        .await
        .with_context(|| format!("failed to request NIP-11 from {relay}"))?
        .error_for_status()
        .with_context(|| format!("NIP-11 request to {relay} failed"))?;
    let document = response
        .json::<RelayInformationDocument>()
        .await
        .with_context(|| format!("invalid NIP-11 document from {relay}"))?;
    Ok(document.advertises_private_repository_transport())
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
