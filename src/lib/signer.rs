use std::sync::Arc;

use anyhow::{Result, anyhow};
use nostr::{
    Event, EventBuilder, Keys, PublicKey,
    event::{AsyncSignEvent, FinalizeUnsignedEvent, SignEvent, UnsignedEvent},
    key::AsyncGetPublicKey,
    util::BoxedFuture,
};
use nostr_connect::{client::NostrConnect, error::Error as NostrConnectError};
use nostr_sdk::{authenticator::SignerAuthenticator, client::ClientBuilder, relay::RelayLimits};

/// Signer abstraction covering both local keys and remote NIP-46 bunker.
#[derive(Clone)]
pub enum NgitSigner {
    Keys(Keys),
    Connect(Arc<NostrConnect>),
}

/// Authenticator-facing handle that shares a single bootstrapped
/// [`NostrConnect`] instance via an [`Arc`].
///
/// The NIP-42 [`SignerAuthenticator`] takes its signer by value, so we can't
/// hand it a borrow. Cloning the underlying `NostrConnect` would give the
/// authenticator an independent, empty connect cache (`OnceCell`), forcing a
/// second connect handshake to the bunker the first time it signs an AUTH
/// event. Wrapping the shared `Arc` lets the authenticator reuse the
/// already-bootstrapped instance (and its live relay connection) instead.
#[derive(Debug, Clone)]
struct SharedConnect(Arc<NostrConnect>);

impl AsyncGetPublicKey for SharedConnect {
    type Error = NostrConnectError;

    #[inline]
    fn get_public_key_async(&self) -> BoxedFuture<'_, Result<PublicKey, Self::Error>> {
        self.0.get_public_key_async()
    }
}

impl AsyncSignEvent for SharedConnect {
    type Error = NostrConnectError;

    #[inline]
    fn sign_event_async(
        &self,
        unsigned: UnsignedEvent,
    ) -> BoxedFuture<'_, Result<Event, Self::Error>> {
        self.0.sign_event_async(unsigned)
    }
}

impl NgitSigner {
    pub async fn get_public_key(&self) -> Result<PublicKey> {
        match self {
            Self::Keys(k) => Ok(k.public_key()),
            Self::Connect(c) => c.get_public_key_async().await.map_err(|e| anyhow!(e)),
        }
    }

    pub async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event> {
        match self {
            Self::Keys(k) => k.sign_event(unsigned).map_err(|e| anyhow!(e)),
            Self::Connect(c) => c.sign_event_async(unsigned).await.map_err(|e| anyhow!(e)),
        }
    }

    pub async fn sign_event_builder(&self, builder: EventBuilder) -> Result<Event> {
        let public_key = self.get_public_key().await?;
        let unsigned = builder.finalize_unsigned(public_key);
        self.sign_event(unsigned).await
    }

    /// True when this is a remote (NIP-46) signer — used to show progress
    /// messages.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Connect(_))
    }

    /// Build a nostr_sdk client with the appropriate NIP-42 authenticator.
    pub fn build_client(&self) -> nostr_sdk::client::Client {
        match self {
            Self::Keys(k) => ClientBuilder::default()
                .relay_limits(RelayLimits::disable())
                .verify_subscriptions(true)
                .authenticator(SignerAuthenticator::new(k.clone()))
                .build(),
            Self::Connect(c) => ClientBuilder::default()
                .relay_limits(RelayLimits::disable())
                .verify_subscriptions(true)
                .authenticator(SignerAuthenticator::new(SharedConnect(Arc::clone(c))))
                .build(),
        }
    }
}

impl std::fmt::Debug for NgitSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keys(k) => write!(f, "NgitSigner::Keys({})", k.public_key()),
            Self::Connect(_) => write!(f, "NgitSigner::Connect"),
        }
    }
}

/// Wraps an `Arc<NgitSigner>` for use with `fetch_public_key` and similar
/// helpers.
pub async fn fetch_public_key_from_signer(signer: &Arc<NgitSigner>) -> Result<PublicKey> {
    if signer.is_remote() {
        let term = console::Term::stderr();
        term.write_line("fetching npub from remote signer...")?;
        let public_key = signer
            .get_public_key()
            .await
            .map_err(|e| anyhow!("failed to get npub from remote signer: {e}"))?;
        term.clear_last_lines(1)?;
        Ok(public_key)
    } else {
        signer
            .get_public_key()
            .await
            .map_err(|e| anyhow!("failed to get public key from local keys: {e}"))
    }
}
