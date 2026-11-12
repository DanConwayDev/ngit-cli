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
    sync::{Arc, Mutex, RwLock},
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
use tokio::sync::Notify;

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

/// A local deadline for optional public-repository NIP-42 signing. This is a
/// retryable destination failure, distinct from an explicit signer refusal.
#[derive(Debug)]
pub(crate) struct AuthSigningTimeout;

impl std::fmt::Display for AuthSigningTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("relay AUTH signing exceeded its publication deadline")
    }
}

impl std::error::Error for AuthSigningTimeout {}

async fn with_auth_signing_deadline(
    timeout: Option<std::time::Duration>,
    operation: impl Future<Output = anyhow::Result<Event>>,
) -> anyhow::Result<Event> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| anyhow::Error::new(AuthSigningTimeout))?,
        None => operation.await,
    }
}

/// Auth-signing state for a one-shot isolated publication.
///
/// Ordinary shared clients do not install this capture. Early GRASP publication
/// uses it to distinguish a remote signer failure from a relay failure and to
/// stop the relay accepting new auth work before draining an in-flight human
/// signature. Public publication AUTH has its own short signing deadline;
/// private publication retains the signer's native interaction deadline.
/// Network waiting is independently bounded by the publication watchdog.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuthSigningCapture {
    inner: Arc<AuthSigningCaptureInner>,
}

#[derive(Debug, Default)]
struct AuthSigningCaptureInner {
    state: Mutex<AuthSigningState>,
    idle: Notify,
    failed: Notify,
    activity: Notify,
}

#[derive(Debug, Default)]
struct AuthSigningState {
    running: usize,
    activity_generation: u64,
    closing: bool,
    error: Option<anyhow::Error>,
}

impl AuthSigningCapture {
    /// Mark the isolated relay as closing and wait for any signing operation
    /// already started by its authenticator. A signer which has not started is
    /// prevented from starting by [`AuthSigningOperation::begin`].
    pub(crate) async fn close_and_drain(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closing = true;
        loop {
            let idle = self.inner.idle.notified();
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .running
                == 0
            {
                return;
            }
            idle.await;
        }
    }

    /// Bound network waiting while allowing an active human-backed signature
    /// to use its configured AUTH deadline (native for private repositories).
    /// A completed signature grants a fresh
    /// network budget for the AUTH acknowledgement and EVENT retry.
    pub(crate) async fn network_timeout(&self, budget: std::time::Duration) {
        loop {
            let changed = self.inner.activity.notified();
            let (signing, generation) = {
                let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                (state.running > 0, state.activity_generation)
            };
            if signing {
                changed.await;
            } else {
                tokio::select! {
                    () = tokio::time::sleep(budget) => {
                        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                        // Signing can start and finish between polls, including
                        // when both the timer and activity notification are ready.
                        if state.running == 0 && state.activity_generation == generation {
                            return;
                        }
                    },
                    () = changed => {},
                }
            }
        }
    }

    pub(crate) fn take_error(&self) -> Option<anyhow::Error> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .take()
    }

    pub(crate) async fn wait_for_signing_failure(&self) {
        loop {
            let failed = self.inner.failed.notified();
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .error
                .is_some()
            {
                return;
            }
            failed.await;
        }
    }

    pub(crate) async fn signing_failed(&self) -> anyhow::Error {
        self.wait_for_signing_failure().await;
        self.take_error()
            .expect("captured auth failure must remain available to its observer")
    }

    fn begin(&self) -> Option<AuthSigningOperation> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return None;
        }
        state.running += 1;
        state.activity_generation = state.activity_generation.wrapping_add(1);
        self.inner.activity.notify_one();
        drop(state);
        Some(AuthSigningOperation {
            capture: self.clone(),
            finished: false,
        })
    }

    fn fail(&self, error: anyhow::Error) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.error.is_none() {
                state.error = Some(error);
            }
        }
        // Store a permit when the send path has not started watching. This
        // covers signer refusal before the SDK's EVENT waiter subscription.
        self.inner.failed.notify_one();
    }

    fn finish(&self) {
        let became_idle = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.running = state.running.saturating_sub(1);
            state.activity_generation = state.activity_generation.wrapping_add(1);
            state.running == 0
        };
        if became_idle {
            self.inner.activity.notify_one();
            self.inner.idle.notify_one();
        }
    }
}

struct AuthSigningOperation {
    capture: AuthSigningCapture,
    finished: bool,
}

impl AuthSigningOperation {
    fn begin(capture: &AuthSigningCapture) -> Option<Self> {
        capture.begin()
    }

    fn fail(mut self, error: anyhow::Error) {
        self.capture.fail(error);
        self.finished = true;
        self.capture.finish();
    }
}

impl Drop for AuthSigningOperation {
    fn drop(&mut self) {
        if !self.finished {
            self.capture.finish();
        }
    }
}

/// A NIP-42 authenticator that signs only for pre-classified relays and only
/// with a signer explicitly attached by the command.
#[derive(Debug)]
pub struct PolicyAuthenticator {
    policy: Arc<RelayAuthPolicy>,
    signing_capture: Option<AuthSigningCapture>,
    auth_signing_timeout: Option<std::time::Duration>,
}

impl PolicyAuthenticator {
    pub fn new(policy: Arc<RelayAuthPolicy>) -> Self {
        Self {
            policy,
            signing_capture: None,
            auth_signing_timeout: None,
        }
    }

    pub(crate) fn with_signing_capture(
        policy: Arc<RelayAuthPolicy>,
        signing_capture: AuthSigningCapture,
        auth_signing_timeout: Option<std::time::Duration>,
    ) -> Self {
        Self {
            policy,
            signing_capture: Some(signing_capture),
            auth_signing_timeout,
        }
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
            let operation = self
                .signing_capture
                .as_ref()
                .and_then(AuthSigningOperation::begin);
            if self.signing_capture.is_some() && operation.is_none() {
                return Err(NostrSdkError::policy(format!(
                    "isolated publication to {relay_url} is closing; no NIP-42 signing was started"
                )));
            }
            let result = with_auth_signing_deadline(
                self.auth_signing_timeout,
                signer.sign_event_builder_with_description(builder, &description),
            )
            .await;
            match (operation, result) {
                (Some(operation), Err(error)) => {
                    let rendered = format!("{error:#}");
                    operation.fail(error);
                    Err(NostrSdkError::policy(rendered))
                }
                (Some(operation), result) => {
                    drop(operation);
                    result.map_err(|error| NostrSdkError::policy(format!("{error:#}")))
                }
                (None, result) => {
                    result.map_err(|error| NostrSdkError::policy(format!("{error:#}")))
                }
            }
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

    #[tokio::test(start_paused = true)]
    async fn public_auth_signing_has_a_short_retryable_deadline() {
        let operation = with_auth_signing_deadline(
            Some(std::time::Duration::from_secs(2)),
            std::future::pending(),
        );
        tokio::pin!(operation);
        assert!(futures::poll!(&mut operation).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert!(operation.await.unwrap_err().is::<AuthSigningTimeout>());
    }

    #[tokio::test(start_paused = true)]
    async fn private_auth_signing_retains_the_native_approval_window() {
        use nostr::prelude::{EventBuilder, FinalizeEvent};
        let event = EventBuilder::new(Kind::Authentication, "approval")
            .finalize(&Keys::generate())
            .unwrap();
        let (approve, approved) = tokio::sync::oneshot::channel();
        let operation = with_auth_signing_deadline(None, async { anyhow::Ok(approved.await?) });
        tokio::pin!(operation);
        assert!(futures::poll!(&mut operation).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        assert!(futures::poll!(&mut operation).is_pending());
        approve.send(event.clone()).unwrap();
        assert_eq!(operation.await.unwrap(), event);
    }

    #[tokio::test(start_paused = true)]
    async fn network_deadline_pauses_only_while_signing_is_active() {
        use std::time::Duration;
        let capture = AuthSigningCapture::default();
        let deadline = capture.network_timeout(Duration::from_secs(2));
        tokio::pin!(deadline);
        assert!(futures::poll!(&mut deadline).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        let signature = capture.begin().unwrap();
        assert!(futures::poll!(&mut deadline).is_pending());
        tokio::time::advance(Duration::from_secs(600)).await;
        assert!(futures::poll!(&mut deadline).is_pending());
        drop(signature);
        assert!(futures::poll!(&mut deadline).is_pending());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(futures::poll!(&mut deadline).is_ready());
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
