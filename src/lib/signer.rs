use std::sync::Arc;

use anyhow::{Result, anyhow};
use nostr::{Event, EventBuilder, Keys, PublicKey, event::UnsignedEvent};
use nostr_connect::client::NostrConnect;
use nostr_sdk::{
    authenticator::SignerAuthenticator,
    client::ClientBuilder,
    relay::RelayLimits,
};

/// Signer abstraction covering both local keys and remote NIP-46 bunker.
#[derive(Clone)]
pub enum NgitSigner {
    Keys(Keys),
    Connect(NostrConnect),
}

impl NgitSigner {
    pub async fn get_public_key(&self) -> Result<PublicKey> {
        match self {
            Self::Keys(k) => Ok(k.public_key()),
            Self::Connect(c) => {
                use nostr::signer::AsyncGetPublicKey;
                c.get_public_key_async().await.map_err(|e| anyhow!(e))
            }
        }
    }

    pub async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event> {
        match self {
            Self::Keys(k) => {
                use nostr::signer::SignEvent;
                k.sign_event(unsigned).map_err(|e| anyhow!(e))
            }
            Self::Connect(c) => {
                use nostr::signer::AsyncSignEvent;
                c.sign_event_async(unsigned).await.map_err(|e| anyhow!(e))
            }
        }
    }

    pub async fn sign_event_builder(&self, builder: EventBuilder) -> Result<Event> {
        use nostr::event::unsigned::FinalizeUnsignedEvent;
        let public_key = self.get_public_key().await?;
        let unsigned = builder.finalize_unsigned(public_key);
        self.sign_event(unsigned).await
    }

    /// True when this is a remote (NIP-46) signer — used to show progress messages.
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
                .authenticator(SignerAuthenticator::new(c.clone()))
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

/// Wraps an `Arc<NgitSigner>` for use with `fetch_public_key` and similar helpers.
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
