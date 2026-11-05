//! NIP-42 relay authentication policy.
//!
//! Relay authentication proves the user's identity to the relay, so a relay
//! challenge alone is never permission to locate or activate a signer. ngit
//! defers unsolicited challenges until a request is rejected with auth-required
//! (unless proactive authentication is explicitly enabled), and classifies each
//! relay use before connecting:
//!
//! - repository relays may authenticate when a signer is already attached;
//! - relays used for a known-private repository require that the caller has
//!   explicitly acquired and attached a signer;
//! - the user's inbox/outbox relays may authenticate only while publishing;
//! - indexers, blasters, fallback relays, and other users' relays never
//!   authenticate.
//!
//! [`RelayAuthPolicy`] stores that session-scoped classification and the signer
//! explicitly supplied by [`crate::client::Connect::set_signer`]. The
//! authenticator deliberately has no repository or login access, which keeps a
//! malicious AUTH challenge from triggering credential discovery or a remote
//! signer request.

use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
};

use futures::StreamExt;
use nostr::prelude::{Event, IntoEventBuilder, RelayUrl, Url, nip42::ClientAuthentication};
use nostr_sdk::{
    authenticator::Authenticator,
    error::Error as NostrSdkError,
    transport::websocket::{
        DefaultWebsocketTransport, WebSocketSink, WebSocketStream, WebSocketTransport,
    },
};

use crate::signer::NgitSigner;

/// How ngit may respond when a relay requests NIP-42 authentication.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum RelayAuthMode {
    /// The relay is not an identity-bearing destination for this operation.
    #[default]
    Never,
    /// Authenticate only when the command has already attached a signer.
    IfSignerAttached,
    /// This is a known-private repository access. The caller must acquire and
    /// attach the selected signer before connecting.
    Required,
}

/// Session-scoped relay classifications shared with [`PolicyAuthenticator`].
#[derive(Debug, Default)]
pub struct RelayAuthPolicy {
    modes: RwLock<HashMap<RelayUrl, RelayAuthMode>>,
    /// Relays whose challenge could not be answered under the policy at the
    /// time. nostr-sdk consumes that challenge, so a newly allowed relay must
    /// reconnect to obtain another one.
    declined: RwLock<std::collections::HashSet<RelayUrl>>,
    /// Signer explicitly selected by the command. Challenges never populate
    /// this slot themselves.
    signer: RwLock<Option<Arc<NgitSigner>>>,
}

impl RelayAuthPolicy {
    fn register<I>(&self, relays: I, mode: RelayAuthMode)
    where
        I: IntoIterator<Item = RelayUrl>,
    {
        let mut modes = self.modes.write().unwrap_or_else(|e| e.into_inner());
        for relay in relays {
            modes
                .entry(relay)
                .and_modify(|current| *current = (*current).max(mode))
                .or_insert(mode);
        }
    }

    /// Mark ordinary repository relays as eligible only when the current
    /// command has already attached a signer.
    pub fn register_repo_relays<I>(&self, relays: I)
    where
        I: IntoIterator<Item = RelayUrl>,
    {
        self.register(relays, RelayAuthMode::IfSignerAttached);
    }

    /// Mark relays used for known-private repository access. Signer acquisition
    /// remains the caller's responsibility and must happen before connecting.
    pub fn register_private_repo_relays<I>(&self, relays: I)
    where
        I: IntoIterator<Item = RelayUrl>,
    {
        self.register(relays, RelayAuthMode::Required);
    }

    /// Allow authentication to the user's own relay while explicitly
    /// publishing events there, provided the command already has a signer.
    pub fn register_publish_relays<I>(&self, relays: I)
    where
        I: IntoIterator<Item = RelayUrl>,
    {
        self.register(relays, RelayAuthMode::IfSignerAttached);
    }

    /// Attach the signer explicitly selected or created by the command.
    pub fn set_signer(&self, signer: Arc<NgitSigner>) {
        *self.signer.write().unwrap_or_else(|e| e.into_inner()) = Some(signer);
    }

    pub fn mode_for(&self, relay: &RelayUrl) -> RelayAuthMode {
        self.modes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(relay)
            .copied()
            .unwrap_or_default()
    }

    fn signer(&self) -> Option<Arc<NgitSigner>> {
        self.signer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn record_declined(&self, relay: &RelayUrl) {
        self.declined
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(relay.clone());
    }

    /// True when a previously declined relay can now be authenticated. Clears
    /// the marker so the caller can reconnect exactly once.
    pub fn take_stale_declined(&self, relay: &RelayUrl) -> bool {
        if self.mode_for(relay) == RelayAuthMode::Never || self.signer().is_none() {
            return false;
        }
        self.declined
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(relay)
    }
}

/// Defer unsolicited AUTH challenges until a request actually needs auth.
/// The SDK authenticates immediately on every challenge and reports failures
/// to active subscriptions, even when those subscriptions allow public reads.
/// Keep the latest challenge at the transport boundary instead, then let the
/// SDK perform its normal authentication and retry after an auth-required
/// reply.
#[derive(Debug)]
pub(crate) struct DemandAuthTransport {
    proactive: bool,
}

impl DemandAuthTransport {
    pub(crate) fn new(proactive: bool) -> Self {
        Self { proactive }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AuthMessage {
    Challenge,
    Required,
    Other,
}

fn classify_auth_message(raw: &str) -> AuthMessage {
    // Decode only the message name before deciding whether to inspect the
    // full frame. Ordinary events never need a second payload decode. Using
    // the JSON decoder also handles whitespace and escaped message strings.
    use serde::Deserialize;
    let Some(fields) = raw.trim_start().strip_prefix('[') else {
        return AuthMessage::Other;
    };
    let Ok(kind) = String::deserialize(&mut serde_json::Deserializer::from_str(fields)) else {
        return AuthMessage::Other;
    };
    if !matches!(kind.as_str(), "AUTH" | "CLOSED" | "OK") {
        return AuthMessage::Other;
    }
    let Ok(message) = serde_json::from_str::<serde_json::Value>(raw) else {
        return AuthMessage::Other;
    };
    let reason = match message[0].as_str() {
        Some("AUTH") if message[1].is_string() => return AuthMessage::Challenge,
        Some("CLOSED") => message[2].as_str(),
        Some("OK") if message[2].as_bool() == Some(false) => message[3].as_str(),
        _ => None,
    };
    if reason.is_some_and(|reason| reason.starts_with("auth-required:")) {
        AuthMessage::Required
    } else {
        AuthMessage::Other
    }
}

#[derive(Debug)]
struct DeferredAuth<T> {
    challenge: Option<T>,
    required: bool,
}

impl<T> DeferredAuth<T> {
    fn new() -> Self {
        Self {
            challenge: None,
            required: false,
        }
    }

    fn receive(&mut self, message: T, kind: AuthMessage) -> Vec<T> {
        match kind {
            AuthMessage::Challenge if !self.required => {
                // New challenges replace earlier ones on this connection.
                self.challenge = Some(message);
                vec![]
            }
            AuthMessage::Required => {
                self.required = true;
                let mut messages = vec![message];
                // Forward rejection first so the SDK registers the pending
                // retry before authentication can complete.
                messages.extend(self.challenge.take());
                messages
            }
            _ => vec![message],
        }
    }
}

impl WebSocketTransport for DemandAuthTransport {
    fn support_ping(&self) -> bool {
        DefaultWebsocketTransport.support_ping()
    }

    fn connect<'a>(
        &'a self,
        url: &'a Url,
        proxy: Option<SocketAddr>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(WebSocketSink, WebSocketStream), NostrSdkError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let (sink, stream) = DefaultWebsocketTransport.connect(url, proxy).await?;
            if self.proactive {
                return Ok((sink, stream));
            }
            // Connection-local state is discarded on reconnect. Never carry
            // an authentication requirement or challenge to another session.
            let mut deferred = DeferredAuth::new();
            let stream: WebSocketStream = Box::pin(stream.flat_map(move |message| {
                let kind = message
                    .as_ref()
                    .ok()
                    .and_then(|message| message.as_text())
                    .map_or(AuthMessage::Other, classify_auth_message);
                futures::stream::iter(deferred.receive(message, kind))
            }));
            Ok((sink, stream))
        })
    }
}

/// A NIP-42 authenticator that signs only for pre-classified relays and only
/// with a signer explicitly attached by the command.
#[derive(Debug)]
pub struct PolicyAuthenticator {
    policy: Arc<RelayAuthPolicy>,
}

impl PolicyAuthenticator {
    pub fn new(policy: Arc<RelayAuthPolicy>) -> Self {
        Self { policy }
    }
}

impl Authenticator for PolicyAuthenticator {
    fn make_auth_event<'a>(
        &'a self,
        relay_url: &'a RelayUrl,
        challenge: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Event, NostrSdkError>> + Send + 'a>> {
        Box::pin(async move {
            let mode = self.policy.mode_for(relay_url);
            if mode == RelayAuthMode::Never {
                self.policy.record_declined(relay_url);
                return Err(NostrSdkError::policy(format!(
                    "ngit declines NIP-42 auth to {relay_url}: relay is not an authenticated target"
                )));
            }

            let Some(signer) = self.policy.signer() else {
                self.policy.record_declined(relay_url);
                let reason = match mode {
                    RelayAuthMode::Required => {
                        "private repository relay requires an explicitly selected signer"
                    }
                    RelayAuthMode::IfSignerAttached => {
                        "no signer was attached before the relay requested authentication"
                    }
                    RelayAuthMode::Never => unreachable!(),
                };
                return Err(NostrSdkError::policy(format!(
                    "ngit declines NIP-42 auth to {relay_url}: {reason}"
                )));
            };

            let builder =
                ClientAuthentication::new(challenge, relay_url.clone()).into_event_builder();
            let description = format!("relay authentication for {relay_url}");
            signer
                .sign_event_builder_with_description(builder, &description)
                .await
                .map_err(|error| NostrSdkError::policy(format!("{error:#}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{Keys, Kind, nip42};

    use super::*;

    fn url(value: &str) -> RelayUrl {
        RelayUrl::parse(value).unwrap()
    }

    fn attach_keys(policy: &RelayAuthPolicy, keys: &Keys) {
        policy.set_signer(Arc::new(NgitSigner::Keys(keys.clone())));
    }

    #[test]
    fn only_request_rejections_require_authentication() {
        for message in [
            r#"["CLOSED","sub","auth-required: sign in"]"#,
            r#"["OK","id",false,"auth-required: sign in"]"#,
            r#" [ "CLOSED", "sub", "\u0061uth-required: sign in" ] "#,
        ] {
            assert_eq!(classify_auth_message(message), AuthMessage::Required);
        }
        for message in [
            r#"["OK","id",true,"auth-required: accepted"]"#,
            r#"["CLOSED","sub","restricted: not allowed"]"#,
            r#"["NOTICE","auth-required: unsolicited notice"]"#,
            r#"["EVENT","sub",{"content":"auth-required:"}]"#,
            "invalid auth-required: message",
        ] {
            assert_eq!(classify_auth_message(message), AuthMessage::Other);
        }
    }

    #[test]
    fn latest_challenge_is_released_after_rejection() {
        let mut deferred = DeferredAuth::new();
        assert!(deferred.receive("old", AuthMessage::Challenge).is_empty());
        assert!(deferred.receive("new", AuthMessage::Challenge).is_empty());
        assert_eq!(deferred.receive("event", AuthMessage::Other), vec!["event"]);
        assert_eq!(
            deferred.receive("closed", AuthMessage::Required),
            vec!["closed", "new"]
        );
        assert_eq!(
            deferred.receive("another challenge", AuthMessage::Challenge),
            vec!["another challenge"]
        );
        assert_eq!(
            deferred.receive("another rejection", AuthMessage::Required),
            vec!["another rejection"]
        );
        // A new connection must not inherit the previous auth requirement.
        assert!(
            DeferredAuth::new()
                .receive("challenge", AuthMessage::Challenge)
                .is_empty()
        );
    }

    #[test]
    fn rejection_before_challenge_is_supported() {
        let mut deferred = DeferredAuth::new();
        assert_eq!(
            deferred.receive("closed", AuthMessage::Required),
            vec!["closed"]
        );
        assert_eq!(
            deferred.receive("challenge", AuthMessage::Challenge),
            vec!["challenge"]
        );
    }

    // A real WebSocket exchange checks SDK retry and authentication-failure
    // behavior, including the public-read regression, without a user signer.
    async fn challenging_relay(proactive: bool, read_gate: bool, write_gate: bool, signer: bool) {
        use std::time::Duration;

        use futures::SinkExt;
        use nostr::prelude::{EventBuilder, Filter, FinalizeEvent};
        use nostr_sdk::relay::Relay;
        use serde_json::{Value, json};
        use tokio::net::TcpListener;
        use tokio_tungstenite::{accept_async, tungstenite::Message};

        crate::tls::install_default_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_url = url(&format!("ws://{}", listener.local_addr().unwrap()));
        let server_url = relay_url.clone();
        let keys = Keys::generate();
        let expected = EventBuilder::new(Kind::RepoState, "test state")
            .finalize(&keys)
            .unwrap();
        let stored = expected.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            socket
                .send(Message::Text(
                    json!(["AUTH", "challenge"]).to_string().into(),
                ))
                .await
                .unwrap();
            let mut authenticated = false;
            let mut auth_count = 0;
            let mut rejected_read = false;
            let mut rejected_write = false;
            let mut pending = None;
            while let Some(Ok(message)) = socket.next().await {
                if message.is_close() {
                    break;
                }
                let Ok(raw) = message.to_text() else { continue };
                let Ok(message) = serde_json::from_str::<Value>(raw) else {
                    continue;
                };
                match message[0].as_str() {
                    Some("AUTH") => {
                        let event: Event = serde_json::from_value(message[1].clone()).unwrap();
                        assert!(nip42::is_valid_auth_event(&event, &server_url, "challenge"));
                        assert!(event.verify().is_ok());
                        authenticated = true;
                        auth_count += 1;
                        socket
                            .send(Message::Text(
                                json!(["OK", event.id, true, ""]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                        // Proactive mode must authenticate even though this
                        // relay never sent an auth-required rejection.
                        if let Some(id) = pending.take() {
                            socket
                                .send(Message::Text(
                                    json!(["EVENT", id, stored]).to_string().into(),
                                ))
                                .await
                                .unwrap();
                            socket
                                .send(Message::Text(json!(["EOSE", id]).to_string().into()))
                                .await
                                .unwrap();
                        }
                    }
                    Some("REQ") => {
                        let id = message[1].as_str().unwrap().to_owned();
                        if read_gate && !authenticated {
                            rejected_read = true;
                            socket
                                .send(Message::Text(
                                    json!(["CLOSED", id, "auth-required: read"])
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                        } else if proactive && !authenticated {
                            pending = Some(id);
                        } else {
                            socket
                                .send(Message::Text(
                                    json!(["EVENT", id, stored]).to_string().into(),
                                ))
                                .await
                                .unwrap();
                            socket
                                .send(Message::Text(json!(["EOSE", id]).to_string().into()))
                                .await
                                .unwrap();
                        }
                    }
                    Some("EVENT") => {
                        let allowed = !write_gate || authenticated;
                        rejected_write |= !allowed;
                        socket
                            .send(Message::Text(
                                json!([
                                    "OK",
                                    message[1]["id"],
                                    allowed,
                                    if allowed { "" } else { "auth-required: write" }
                                ])
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    _ => {}
                }
            }
            (auth_count, rejected_read, rejected_write)
        });
        let policy = Arc::new(RelayAuthPolicy::default());
        policy.register_repo_relays([relay_url.clone()]);
        if signer {
            attach_keys(&policy, &keys);
        }
        let relay = Relay::builder(relay_url)
            .websocket_transport(DemandAuthTransport::new(proactive))
            .authenticator(PolicyAuthenticator::new(policy))
            .build();
        tokio::time::timeout(Duration::from_secs(5), async {
            relay.try_connect().await.unwrap();
            let events = relay
                .fetch_events(Filter::new().kind(Kind::RepoState))
                .await
                .unwrap();
            assert!(events.iter().any(|event| event.id == expected.id));
            if write_gate {
                relay.send_event(&expected).await.unwrap();
            }
            relay.disconnect();
            let (auth_count, rejected_read, rejected_write) = server.await.unwrap();
            assert_eq!(
                auth_count,
                usize::from(proactive || read_gate || write_gate)
            );
            assert_eq!(rejected_read, read_gate && !proactive);
            assert_eq!(rejected_write, write_gate && !proactive);
        })
        .await
        .expect("relay authentication exchange timed out");
    }

    #[tokio::test]
    async fn public_reads_do_not_authenticate_even_with_a_signer() {
        challenging_relay(false, false, false, true).await;
    }

    #[tokio::test]
    async fn public_reads_succeed_without_a_signer_despite_auth_challenge() {
        challenging_relay(false, false, false, false).await;
    }

    #[tokio::test]
    async fn reads_authenticate_only_after_auth_required() {
        challenging_relay(false, true, false, true).await;
    }

    #[tokio::test]
    async fn writes_authenticate_only_after_auth_required() {
        challenging_relay(false, false, true, true).await;
    }

    #[tokio::test]
    async fn proactive_mode_authenticates_without_request_rejection() {
        challenging_relay(true, false, false, true).await;
    }

    #[test]
    fn relay_modes_only_upgrade() {
        let policy = RelayAuthPolicy::default();
        let relay = url("wss://repo.example.com");

        assert_eq!(policy.mode_for(&relay), RelayAuthMode::Never);
        policy.register_repo_relays([relay.clone()]);
        assert_eq!(policy.mode_for(&relay), RelayAuthMode::IfSignerAttached);
        policy.register_private_repo_relays([relay.clone()]);
        policy.register_publish_relays([relay.clone()]);
        assert_eq!(policy.mode_for(&relay), RelayAuthMode::Required);
    }

    #[tokio::test]
    async fn public_repo_relay_does_not_acquire_a_signer_from_a_challenge() {
        let policy = Arc::new(RelayAuthPolicy::default());
        let relay = url("wss://repo.example.com");
        policy.register_repo_relays([relay.clone()]);
        let authenticator = PolicyAuthenticator::new(Arc::clone(&policy));

        let error = authenticator
            .make_auth_event(&relay, "challenge")
            .await
            .expect_err("an unattached signer must not be discovered lazily");
        assert!(error.to_string().contains("no signer was attached"));

        let keys = Keys::generate();
        attach_keys(&policy, &keys);
        assert!(policy.take_stale_declined(&relay));
        let event = authenticator
            .make_auth_event(&relay, "fresh-challenge")
            .await
            .expect("an explicitly attached signer may authenticate a repo relay");
        assert_eq!(event.pubkey, keys.public_key());
        assert!(nip42::is_valid_auth_event(
            &event,
            &relay,
            "fresh-challenge"
        ));
    }

    #[tokio::test]
    async fn private_repo_relay_requires_an_explicit_signer() {
        let policy = Arc::new(RelayAuthPolicy::default());
        let relay = url("wss://private.example.com");
        policy.register_private_repo_relays([relay.clone()]);
        let authenticator = PolicyAuthenticator::new(Arc::clone(&policy));

        let error = authenticator
            .make_auth_event(&relay, "challenge")
            .await
            .expect_err("private auth without an acquired signer must fail closed");
        assert!(error.to_string().contains("explicitly selected signer"));

        let keys = Keys::generate();
        attach_keys(&policy, &keys);
        let event = authenticator
            .make_auth_event(&relay, "fresh-challenge")
            .await
            .expect("known-private access may use the selected signer");
        assert_eq!(event.kind, Kind::Authentication);
    }

    #[tokio::test]
    async fn unrelated_relay_is_never_authenticated_even_with_a_signer() {
        let keys = Keys::generate();
        let policy = Arc::new(RelayAuthPolicy::default());
        attach_keys(&policy, &keys);
        let relay = url("wss://indexer.example.com");

        let error = PolicyAuthenticator::new(policy)
            .make_auth_event(&relay, "challenge")
            .await
            .expect_err("an unclassified relay must never receive identity proof");
        assert!(error.to_string().contains("not an authenticated target"));
    }

    #[test]
    fn relay_urls_use_normalized_identity() {
        let policy = RelayAuthPolicy::default();
        policy.register_repo_relays([url("wss://repo.example.com/")]);
        assert_eq!(
            policy.mode_for(&url("wss://repo.example.com")),
            RelayAuthMode::IfSignerAttached
        );
    }
}
