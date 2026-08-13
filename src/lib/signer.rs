use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use nostr::prelude::{
    Event, EventBuilder, EventId, Keys, PublicKey,
    event::{AsyncSignEvent, FinalizeUnsignedEvent, SignEvent, UnsignedEvent},
    key::AsyncGetPublicKey,
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
    fn get_public_key_async(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<PublicKey, Self::Error>> + Send + '_>> {
        self.0.get_public_key_async()
    }
}

impl AsyncSignEvent for SharedConnect {
    type Error = NostrConnectError;

    #[inline]
    fn sign_event_async(
        &self,
        unsigned: UnsignedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Event, Self::Error>> + Send + '_>> {
        Box::pin(async move {
            let expected_public_key = unsigned.pubkey;
            let expected_event_id = unsigned.compute_id();
            let event = self.0.sign_event_async(unsigned).await?;
            validate_remote_signed_event(expected_public_key, expected_event_id, event)
                .map_err(|error| NostrConnectError::other(std::io::Error::other(error.to_string())))
        })
    }
}

fn validate_remote_signed_event(
    expected_public_key: PublicKey,
    expected_event_id: EventId,
    event: Event,
) -> Result<Event> {
    if event.pubkey != expected_public_key {
        bail!("remote signer signed with a different npub than requested");
    }
    if event.id != expected_event_id {
        bail!("remote signer signed a different event than requested");
    }
    event
        .verify()
        .context("remote signer returned an invalid event signature")?;
    Ok(event)
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
            Self::Connect(c) => {
                let expected_public_key = unsigned.pubkey;
                let expected_event_id = unsigned.compute_id();
                let event = c.sign_event_async(unsigned).await.map_err(|e| anyhow!(e))?;
                validate_remote_signed_event(expected_public_key, expected_event_id, event)
            }
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
        let builder = match self {
            Self::Keys(k) => ClientBuilder::default()
                .relay_limits(RelayLimits::disable())
                .verify_subscriptions(true)
                .authenticator(SignerAuthenticator::new(k.clone())),
            Self::Connect(c) => ClientBuilder::default()
                .relay_limits(RelayLimits::disable())
                .verify_subscriptions(true)
                .authenticator(SignerAuthenticator::new(SharedConnect(Arc::clone(c)))),
        };
        // Route `.onion` relays through the configured SOCKS5/Tor proxy.
        match crate::client::tor_socks5_proxy_addr() {
            Some(addr) => builder.proxy(nostr_sdk::proxy::Proxy::onion(addr)).build(),
            None => builder.build(),
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

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, FinalizeUnsignedEvent, Kind, SignEvent};

    use super::*;

    fn unsigned_event(keys: &Keys, content: &str) -> UnsignedEvent {
        EventBuilder::new(Kind::TextNote, content).finalize_unsigned(keys.public_key())
    }

    #[test]
    fn accepts_the_requested_event_with_a_valid_signature() {
        let keys = Keys::generate();
        let unsigned = unsigned_event(&keys, "requested");
        let event = keys
            .sign_event(unsigned.clone())
            .expect("test key should sign");

        assert!(
            validate_remote_signed_event(unsigned.pubkey, unsigned.compute_id(), event).is_ok()
        );
    }

    #[test]
    fn rejects_an_event_signed_by_a_different_npub() {
        let requested_keys = Keys::generate();
        let returned_keys = Keys::generate();
        let unsigned = unsigned_event(&requested_keys, "requested");
        let returned = returned_keys
            .sign_event(unsigned_event(&returned_keys, "requested"))
            .expect("test key should sign");

        let error = validate_remote_signed_event(unsigned.pubkey, unsigned.compute_id(), returned)
            .expect_err("a different signer must be rejected");
        assert!(error.to_string().contains("different npub"));
    }

    #[test]
    fn rejects_a_different_event_from_the_requested_signer() {
        let keys = Keys::generate();
        let unsigned = unsigned_event(&keys, "requested");
        let returned = keys
            .sign_event(unsigned_event(&keys, "different"))
            .expect("test key should sign");

        let error = validate_remote_signed_event(unsigned.pubkey, unsigned.compute_id(), returned)
            .expect_err("different event contents must be rejected");
        assert!(error.to_string().contains("different event"));
    }

    #[test]
    fn rejects_an_invalid_signature_for_the_requested_event() {
        let keys = Keys::generate();
        let other_keys = Keys::generate();
        let unsigned = unsigned_event(&keys, "requested");
        let mut returned = keys
            .sign_event(unsigned.clone())
            .expect("test key should sign");
        returned.sig = other_keys
            .sign_event(unsigned_event(&other_keys, "requested"))
            .expect("other test key should sign")
            .sig;

        let error = validate_remote_signed_event(unsigned.pubkey, unsigned.compute_id(), returned)
            .expect_err("an invalid signature must be rejected");
        assert!(error.to_string().contains("invalid event signature"));
    }
}
