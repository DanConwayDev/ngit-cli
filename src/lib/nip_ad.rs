//! NIP-AD web-address resolution for Nostr resources.
//!
//! The NIP allows one `/.well-known/nostr.json` document to serve both
//! NIP-05 identities and path-addressed Nostr filters. ngit consumes the
//! latter only when the filter identifies one repository announcement
//! coordinate; all later event fetching remains in the regular repository
//! discovery pipeline.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use nostr::prelude::{Filter, Kind, RelayUrl, SingleLetterTag, Url, nip01::Coordinate};
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::Value;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NipAdProfile {
    pub filter: Filter,
    #[serde(default)]
    pub relays: Vec<RelayUrl>,
}

#[derive(Debug)]
pub enum NipAdLookup {
    Found(Box<NipAdProfile>),
    Missing,
    Invalid(anyhow::Error),
}

impl NipAdProfile {
    /// Parse the entry for `path` from a shared NIP-05/NIP-AD document.
    pub fn from_json(path: &str, document: &Value) -> Result<Self> {
        let entry = document
            .get(path)
            .with_context(|| format!("NIP-AD document has no mapping for path {path:?}"))?;
        serde_json::from_value(entry.clone())
            .with_context(|| format!("invalid NIP-AD mapping for path {path:?}"))
    }

    /// Convert a Git-specific NIP-AD filter into the repository coordinate
    /// consumed by ngit's existing discovery machinery.
    pub fn git_repository_coordinate(&self) -> Result<Coordinate> {
        let authors = self
            .filter
            .authors
            .as_ref()
            .context("NIP-AD Git repository filter must specify one author")?;
        if authors.len() != 1 {
            bail!("NIP-AD Git repository filter must specify exactly one author");
        }
        let public_key = *authors.iter().next().expect("length checked above");

        let kinds = self
            .filter
            .kinds
            .as_ref()
            .context("NIP-AD Git repository filter must specify kind 30617")?;
        if kinds.len() != 1 || !kinds.contains(&Kind::GitRepoAnnouncement) {
            bail!("NIP-AD Git repository filter must specify only kind 30617");
        }

        let identifiers = self
            .filter
            .generic_tags
            .get(&SingleLetterTag::LOWERCASE_D)
            .context("NIP-AD Git repository filter must specify one #d identifier")?;
        if identifiers.len() != 1 {
            bail!("NIP-AD Git repository filter must specify exactly one #d identifier");
        }
        let identifier = identifiers
            .iter()
            .next()
            .expect("length checked above")
            .clone();
        if identifier.is_empty() {
            bail!("NIP-AD Git repository filter #d identifier cannot be empty");
        }

        Ok(Coordinate {
            identifier,
            public_key,
            kind: Kind::GitRepoAnnouncement,
        })
    }
}

/// Fetch the NIP-AD mapping for `path` from `domain`.
pub async fn query(domain: &str, path: &str) -> Result<NipAdLookup> {
    let origin = Url::parse(&format!("https://{domain}"))
        .with_context(|| format!("invalid NIP-AD domain {domain:?}"))?;
    query_from_origin(&origin, path).await
}

pub(crate) async fn query_from_origin(origin: &Url, path: &str) -> Result<NipAdLookup> {
    let endpoint = endpoint_url(origin, path)?;
    query_endpoint(endpoint, path).await
}

async fn query_endpoint(endpoint: Url, path: &str) -> Result<NipAdLookup> {
    query_endpoint_with_timeout(endpoint, path, HTTP_TIMEOUT).await
}

async fn query_endpoint_with_timeout(
    endpoint: Url,
    path: &str,
    timeout: Duration,
) -> Result<NipAdLookup> {
    let mut response = crate::tls::http_client_builder()
        .timeout(timeout)
        .redirect(redirect_policy())
        .build()
        .context("failed to create the NIP-AD HTTP client")?
        .get(endpoint.clone())
        .send()
        .await
        .with_context(|| format!("NIP-AD server is not responding at {endpoint}"))?
        .error_for_status()
        .with_context(|| format!("NIP-AD server returned an error at {endpoint}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOCUMENT_BYTES as u64)
    {
        bail!("NIP-AD document from {endpoint} exceeds the {MAX_DOCUMENT_BYTES}-byte limit");
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read NIP-AD document from {endpoint}"))?
    {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_DOCUMENT_BYTES)
        {
            bail!("NIP-AD document from {endpoint} exceeds the {MAX_DOCUMENT_BYTES}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    let document: Value = serde_json::from_slice(&body)
        .with_context(|| format!("NIP-AD server did not return JSON at {endpoint}"))?;
    if document.get(path).is_none() {
        return Ok(NipAdLookup::Missing);
    }
    Ok(match NipAdProfile::from_json(path, &document) {
        Ok(profile) => NipAdLookup::Found(Box::new(profile)),
        Err(error) => NipAdLookup::Invalid(error),
    })
}

fn endpoint_url(origin: &Url, path: &str) -> Result<Url> {
    if !path.starts_with('/') {
        bail!("NIP-AD path must start with '/'");
    }

    let mut endpoint = origin.clone();
    if endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("invalid NIP-AD origin {origin:?}");
    }
    endpoint.set_path("/.well-known/nostr.json");
    endpoint.query_pairs_mut().append_pair("path", path);
    Ok(endpoint)
}

fn redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        let Some(initial) = attempt.previous().first() else {
            return attempt.error("NIP-AD redirect has no initial URL");
        };
        if attempt.previous().len() > 5 {
            attempt.error("too many NIP-AD redirects")
        } else if same_origin(initial, attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("NIP-AD redirects must stay on the original origin")
        }
    })
}

fn same_origin(initial: &Url, redirected: &Url) -> bool {
    initial.scheme() == redirected.scheme()
        && initial.host_str() == redirected.host_str()
        && initial.port_or_known_default() == redirected.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use nostr::prelude::PublicKey;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;

    const AUTHOR: &str = "a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d";

    fn document(filter: Value, relays: Option<Value>) -> Value {
        let mut entry = json!({ "filter": filter });
        if let Some(relays) = relays {
            entry["relays"] = relays;
        }
        json!({
            "names": { "dan": AUTHOR },
            "/ngit.git": entry,
        })
    }

    fn repository_filter() -> Value {
        json!({
            "kinds": [30617],
            "#d": ["ngit"],
            "authors": [AUTHOR],
            "limit": 1,
        })
    }

    async fn serve_response(response: Vec<u8>) -> (Url, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let bytes_read = stream.read(&mut chunk).await.unwrap();
                assert!(bytes_read > 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..bytes_read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16 * 1024, "request headers are too large");
            }
            let request = String::from_utf8_lossy(&request);
            assert!(
                request.starts_with("GET /.well-known/nostr.json?path=%2Fngit.git HTTP/1.1\r\n"),
                "unexpected request: {request}"
            );
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let endpoint = Url::parse(&format!(
            "http://{address}/.well-known/nostr.json?path=%2Fngit.git"
        ))
        .unwrap();
        (endpoint, server)
    }

    async fn finish_server(server: JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("NIP-AD test server timed out")
            .unwrap();
    }

    #[test]
    fn endpoint_uses_the_original_path_as_one_query_value() {
        let origin = Url::parse("https://ngit.dev").unwrap();
        let endpoint = endpoint_url(&origin, "/projects/ngit.git").unwrap();
        assert_eq!(endpoint.scheme(), "https");
        assert_eq!(endpoint.host_str(), Some("ngit.dev"));
        assert_eq!(endpoint.path(), "/.well-known/nostr.json");
        assert_eq!(
            endpoint.query_pairs().collect::<Vec<_>>(),
            vec![("path".into(), "/projects/ngit.git".into())]
        );
    }

    #[test]
    fn redirects_must_preserve_the_complete_origin() {
        let initial = Url::parse("https://ngit.dev/.well-known/nostr.json").unwrap();
        assert!(same_origin(
            &initial,
            &Url::parse("https://ngit.dev/identity.json").unwrap()
        ));
        assert!(!same_origin(
            &initial,
            &Url::parse("http://ngit.dev/identity.json").unwrap()
        ));
        assert!(!same_origin(
            &initial,
            &Url::parse("https://other.example/identity.json").unwrap()
        ));
    }

    #[test]
    fn parses_a_path_mapping_alongside_nip05_fields() {
        let profile = NipAdProfile::from_json(
            "/ngit.git",
            &document(repository_filter(), Some(json!(["wss://relay.ngit.dev"]))),
        )
        .unwrap();

        assert_eq!(
            profile.relays,
            vec![RelayUrl::parse("wss://relay.ngit.dev").unwrap()]
        );
        assert_eq!(
            profile.git_repository_coordinate().unwrap(),
            Coordinate {
                identifier: "ngit".to_owned(),
                public_key: PublicKey::parse(AUTHOR).unwrap(),
                kind: Kind::GitRepoAnnouncement,
            }
        );
    }

    #[test]
    fn relay_hints_are_optional() {
        let profile =
            NipAdProfile::from_json("/ngit.git", &document(repository_filter(), None)).unwrap();
        assert!(profile.relays.is_empty());
    }

    #[test]
    fn lookup_is_exactly_path_scoped() {
        let error = NipAdProfile::from_json("/other.git", &document(repository_filter(), None))
            .unwrap_err();
        assert!(error.to_string().contains("no mapping for path"));
    }

    #[test]
    fn rejects_filters_that_do_not_name_one_repository_coordinate() {
        for filter in [
            json!({ "kinds": [1], "#d": ["ngit"], "authors": [AUTHOR] }),
            json!({ "kinds": [30617], "#d": ["one", "two"], "authors": [AUTHOR] }),
            json!({ "kinds": [30617], "#d": ["ngit"] }),
        ] {
            let profile =
                NipAdProfile::from_json("/ngit.git", &document(filter, Some(json!([])))).unwrap();
            assert!(profile.git_repository_coordinate().is_err());
        }
    }

    #[test]
    fn rejects_invalid_relay_hints() {
        let error = NipAdProfile::from_json(
            "/ngit.git",
            &document(repository_filter(), Some(json!(["not a relay"]))),
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid NIP-AD mapping"));
    }

    #[tokio::test]
    async fn fetches_and_selects_the_requested_mapping() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = serde_json::to_vec(&document(
            repository_filter(),
            Some(json!(["wss://relay.ngit.dev"])),
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let bytes_read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..bytes_read]);
            assert!(
                request.starts_with("GET /.well-known/nostr.json?path=%2Fngit.git HTTP/1.1\r\n"),
                "unexpected request: {request}"
            );
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let endpoint = Url::parse(&format!(
            "http://{address}/.well-known/nostr.json?path=%2Fngit.git"
        ))
        .unwrap();
        let profile = match query_endpoint(endpoint, "/ngit.git").await.unwrap() {
            NipAdLookup::Found(profile) => profile,
            other => panic!("expected a mapping, got {other:?}"),
        };
        finish_server(server).await;

        assert_eq!(
            profile.git_repository_coordinate().unwrap().identifier,
            "ngit"
        );
        assert_eq!(
            profile.relays,
            vec![RelayUrl::parse("wss://relay.ngit.dev").unwrap()]
        );
    }

    #[tokio::test]
    async fn rejects_an_oversized_content_length_before_buffering() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_DOCUMENT_BYTES + 1
        )
        .into_bytes();
        let (endpoint, server) = serve_response(response).await;

        let error = query_endpoint(endpoint, "/ngit.git").await.unwrap_err();
        finish_server(server).await;

        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn bounds_a_response_without_a_content_length() {
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend(vec![b'x'; MAX_DOCUMENT_BYTES + 1]);
        let (endpoint, server) = serve_response(response).await;

        let error = query_endpoint(endpoint, "/ngit.git").await.unwrap_err();
        finish_server(server).await;

        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn refuses_cross_origin_redirects() {
        let response = b"HTTP/1.1 302 Found\r\nLocation: http://other.example/nostr.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (endpoint, server) = serve_response(response).await;

        let error = query_endpoint(endpoint, "/ngit.git").await.unwrap_err();
        finish_server(server).await;

        assert!(
            format!("{error:#}").contains("redirects must stay on the original origin"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn times_out_an_unresponsive_origin() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let endpoint = Url::parse(&format!(
            "http://{address}/.well-known/nostr.json?path=%2Fngit.git"
        ))
        .unwrap();

        let error = query_endpoint_with_timeout(endpoint, "/ngit.git", Duration::from_millis(100))
            .await
            .unwrap_err();
        server.abort();
        let _ = server.await;

        assert!(
            format!("{error:#}")
                .to_ascii_lowercase()
                .contains("timed out"),
            "unexpected error: {error:#}"
        );
    }
}
