use std::{future::Future, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use nostr::{
    nips::nip44::{AsyncNip44, Nip44},
    prelude::{
        Event, EventBuilder, EventId, Keys, PublicKey,
        event::{AsyncSignEvent, FinalizeUnsignedEvent, SignEvent, UnsignedEvent},
        key::AsyncGetPublicKey,
    },
};
use nostr_connect::client::NostrConnect;

/// Signer abstraction covering both local keys and remote NIP-46 bunker.
#[derive(Clone)]
pub enum NgitSigner {
    Keys(Keys),
    Connect(Arc<NostrConnect>),
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
        self.sign_event_with_description(unsigned, "event").await
    }

    /// Sign an event while identifying remote-signer work on stderr.
    ///
    /// Keeping the progress message at this boundary ensures every event
    /// signing path remains visible, including protocol authorizations which
    /// do not go through the higher-level client helpers.
    pub async fn sign_event_with_description(
        &self,
        unsigned: UnsignedEvent,
        description: &str,
    ) -> Result<Event> {
        self.with_remote_signing_progress(description, self.sign_event_inner(unsigned))
            .await
    }

    async fn sign_event_inner(&self, unsigned: UnsignedEvent) -> Result<Event> {
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
        self.sign_event_builder_with_description(builder, "event")
            .await
    }

    /// Finalize and sign an event while identifying remote-signer work on
    /// stderr.
    pub async fn sign_event_builder_with_description(
        &self,
        builder: EventBuilder,
        description: &str,
    ) -> Result<Event> {
        self.with_remote_signing_progress(description, async move {
            let public_key = self.get_public_key().await?;
            let unsigned = builder.finalize_unsigned(public_key);
            self.sign_event_inner(unsigned).await
        })
        .await
    }

    async fn with_remote_signing_progress<T>(
        &self,
        description: &str,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if !self.is_remote() {
            return operation.await;
        }

        let term = console::Term::stderr();
        term.write_line(&remote_signing_message(description))?;
        let result = operation.await;
        if result.is_ok() {
            term.clear_last_lines(1)?;
        }
        result
    }

    pub async fn nip44_encrypt(&self, public_key: &PublicKey, content: &str) -> Result<String> {
        match self {
            Self::Keys(keys) => keys
                .nip44_encrypt(public_key, content)
                .map_err(|error| anyhow!(error)),
            Self::Connect(connect) => connect
                .nip44_encrypt_async(public_key, content)
                .await
                .map_err(|error| anyhow!(error)),
        }
    }

    pub async fn nip44_decrypt(&self, public_key: &PublicKey, payload: &str) -> Result<String> {
        match self {
            Self::Keys(keys) => keys
                .nip44_decrypt(public_key, payload)
                .map_err(|error| anyhow!(error)),
            Self::Connect(connect) => connect
                .nip44_decrypt_async(public_key, payload)
                .await
                .map_err(|error| anyhow!(error)),
        }
    }

    /// True when this is a remote (NIP-46) signer — used to show progress
    /// messages.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Connect(_))
    }
}

fn remote_signing_message(description: &str) -> String {
    format!("signing event ({description}) with remote signer...")
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
    fn remote_signing_progress_identifies_the_operation() {
        assert_eq!(
            remote_signing_message("Blossom upload authorization"),
            "signing event (Blossom upload authorization) with remote signer..."
        );
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
