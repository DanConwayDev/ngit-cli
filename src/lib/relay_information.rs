use std::time::Duration;

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

/// Return relay hints whose public NIP-11 documents identify a private Git
/// transport. Unavailable or malformed documents are ignored so encrypted
/// kind-10318 discovery remains available as the authoritative account signal.
pub async fn discover_private_repository_relays(relays: &[RelayUrl]) -> Vec<RelayUrl> {
    let client = match reqwest::Client::builder().timeout(NIP11_TIMEOUT).build() {
        Ok(client) => client,
        Err(_) => return vec![],
    };
    let results = join_all(
        relays
            .iter()
            .cloned()
            .map(|relay| relay_advertises_private_repository(&client, relay)),
    )
    .await;
    results
        .into_iter()
        .filter_map(|result| match result {
            Ok(relay) => relay,
            Err(error) => {
                if crate::client::is_verbose() {
                    eprintln!("nostr: ignoring unavailable NIP-11 document: {error:#}");
                }
                None
            }
        })
        .collect()
}

async fn relay_advertises_private_repository(
    client: &reqwest::Client,
    relay: RelayUrl,
) -> Result<Option<RelayUrl>> {
    let url = nip11_url(&relay)?;
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
    Ok(document
        .advertises_private_repository_transport()
        .then_some(relay))
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
}
