//! NIP-42 relay authentication policy.
//!
//! Relay authentication proves the user's identity to the relay, so a relay
//! challenge alone is never permission to locate or activate a signer. ngit
//! classifies each relay use before connecting:
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
    pin::Pin,
    sync::{Arc, RwLock},
};

use nostr::prelude::{Event, IntoEventBuilder, RelayUrl, nip42::ClientAuthentication};
use nostr_sdk::{authenticator::Authenticator, error::Error as NostrSdkError};

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
