//! Observe response subscriptions only while a command is running. Retaining
//! the notification stream lets the next command consume closures received
//! between operations, without a background task or another relay subscription.

use std::{collections::HashMap, fmt, time::Duration};

use futures::{FutureExt, StreamExt, stream::BoxStream};
use nostr_sdk::prelude::*;

use super::{ConnectProgress, NostrConnect};

pub(super) struct SubscriptionProgress {
    notifications: BoxStream<'static, ClientNotification>,
    ready: HashMap<RelayUrl, usize>,
}

impl fmt::Debug for SubscriptionProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionProgress")
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

impl SubscriptionProgress {
    pub(super) fn new(notifications: BoxStream<'static, ClientNotification>) -> Self {
        Self {
            notifications,
            ready: HashMap::new(),
        }
    }

    async fn message(
        &mut self,
        connect: &NostrConnect,
        notification: ClientNotification,
        live: bool,
    ) {
        let ClientNotification::Message { relay_url, message } = notification else {
            return;
        };
        match message.as_ref() {
            RelayMessage::EndOfStoredEvents(id)
                if live && id.as_ref() == &connect.subscription_id =>
            {
                let relays = connect.client.relays().await;
                if let Some(relay) = relays
                    .get(&relay_url)
                    .filter(|relay| relay.status() == RelayStatus::Connected)
                {
                    self.ready
                        .insert(relay_url.clone(), relay.stats().success());
                    let _ = connect.progress.send(ConnectProgress::Ready { relay_url });
                }
            }
            RelayMessage::Closed {
                subscription_id,
                message,
            } if subscription_id.as_ref() == &connect.subscription_id => {
                self.ready.remove(&relay_url);
                let _ = connect
                    .progress
                    .send(ConnectProgress::SubscriptionUnavailable {
                        relay_url,
                        reason: Some(message.to_string()),
                    });
            }
            _ => {}
        }
    }

    async fn refresh(&mut self, connect: &NostrConnect, replay: bool) {
        let relays = connect.client.relays().await;
        self.ready.retain(|url, generation| {
            let current = relays.get(url).is_some_and(|relay| {
                relay.status() == RelayStatus::Connected && relay.stats().success() == *generation
            });
            if !current {
                let _ = connect
                    .progress
                    .send(ConnectProgress::SubscriptionUnavailable {
                        relay_url: url.clone(),
                        reason: None,
                    });
            } else if replay {
                let _ = connect.progress.send(ConnectProgress::Ready {
                    relay_url: url.clone(),
                });
            }
            current
        });
    }
}

impl NostrConnect {
    /// Poll alongside a monitored operation; dropping the future stops all
    /// work.
    pub(crate) async fn observe_subscription_progress(&self) {
        let mut state = self.subscription.lock().await;
        // An EOSE queued while idle could belong to an older connection. Only
        // live EOSE confirms readiness; queued CLOSED still invalidates it.
        while let Some(Some(notification)) = state.notifications.next().now_or_never() {
            state.message(self, notification, false).await;
        }
        state.refresh(self, true).await;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                notification = state.notifications.next() => match notification {
                    Some(notification) => state.message(self, notification, true).await,
                    None => return,
                },
                _ = tick.tick() => state.refresh(self, false).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect() -> NostrConnect {
        NostrConnect::new(
            NostrConnectUri::Bunker {
                remote_signer_public_key: Keys::generate().public_key(),
                relays: vec![RelayUrl::parse("wss://signer.example").unwrap()],
                secret: None,
            },
            Keys::generate(),
            Duration::from_secs(10),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn only_matching_closed_invalidates_response_readiness() {
        let connect = connect();
        let relay_url = connect.relays()[0].clone();
        let mut progress = connect.progress();
        let mut state = connect.subscription.lock().await;
        state.ready.insert(relay_url.clone(), 1);
        for (id, matching) in [
            (SubscriptionId::generate(), false),
            (connect.subscription_id.clone(), true),
        ] {
            state
                .message(
                    &connect,
                    ClientNotification::Message {
                        relay_url: relay_url.clone(),
                        message: Box::new(RelayMessage::Closed {
                            subscription_id: std::borrow::Cow::Owned(id),
                            message: "auth-required".into(),
                        }),
                    },
                    false,
                )
                .await;
            assert_eq!(state.ready.contains_key(&relay_url), !matching);
        }
        assert!(
            matches!(progress.try_recv().unwrap(), ConnectProgress::SubscriptionUnavailable { reason: Some(reason), .. } if reason == "auth-required")
        );
        assert!(progress.try_recv().is_err());
    }

    #[tokio::test]
    async fn idle_eose_does_not_confirm_a_potentially_new_connection() {
        let connect = connect();
        let mut state = connect.subscription.lock().await;
        state
            .message(
                &connect,
                ClientNotification::Message {
                    relay_url: connect.relays()[0].clone(),
                    message: Box::new(RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
                        connect.subscription_id.clone(),
                    ))),
                },
                false,
            )
            .await;
        assert!(state.ready.is_empty());
    }

    #[tokio::test]
    async fn unavailable_connection_invalidates_cached_readiness_before_replay() {
        let connect = connect();
        let mut progress = connect.progress();
        let mut state = connect.subscription.lock().await;
        state.ready.insert(connect.relays()[0].clone(), 1);
        state.refresh(&connect, true).await;
        assert!(state.ready.is_empty());
        assert!(matches!(
            progress.try_recv().unwrap(),
            ConnectProgress::SubscriptionUnavailable { reason: None, .. }
        ));
    }
}
