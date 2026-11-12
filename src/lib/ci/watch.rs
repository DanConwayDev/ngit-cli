//! Subscription receipts, separate from the latest-state reducer.
use anyhow::{Result, bail};
use nostr::prelude::{Event, RelayUrl, SubscriptionId};

#[derive(Debug)]
pub enum Update {
    Event { relay: RelayUrl, event: Box<Event> },
    Notice(String),
}

pub struct Subscription {
    pub(crate) id: SubscriptionId,
    pub(crate) observer: Observer,
    pub(crate) client: nostr_sdk::client::Client,
    pub(crate) receiver: tokio::sync::mpsc::Receiver<Result<Update>>,
    pub(crate) pump: tokio::task::JoinHandle<()>,
}
impl Subscription {
    pub async fn next(&mut self) -> Result<Update> {
        match self.receiver.recv().await {
            Some(update) => update,
            None => bail!("CI watch subscription ended unexpectedly"),
        }
    }
    pub async fn close(mut self) {
        self.receiver.close();
        self.pump.abort();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.client.unsubscribe(&self.id),
        )
        .await;
        self.client.disconnect().await;
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.observer.remove(&self.id);
        self.receiver.close();
        self.pump.abort();
    }
}

/// Passive tap after SDK signature/filter validation and before replacement or
/// duplicate reduction. Awaiting capacity applies backpressure instead of the
/// SDK notification stream's silent broadcast-lag discard.
#[derive(Clone, Debug, Default)]
pub struct Observer(
    std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<SubscriptionId, tokio::sync::mpsc::Sender<Result<Update>>>,
        >,
    >,
);
impl Observer {
    pub(crate) fn insert(
        &self,
        id: SubscriptionId,
        sender: tokio::sync::mpsc::Sender<Result<Update>>,
    ) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, sender);
    }
    fn remove(&self, id: &SubscriptionId) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }
}
impl nostr_sdk::policy::AdmitPolicy for Observer {
    fn admit_event<'a>(
        &'a self,
        relay: &'a RelayUrl,
        subscription: &'a SubscriptionId,
        event: &'a Event,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<
                        nostr_sdk::policy::AdmitStatus,
                        nostr_sdk::error::Error,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let sender = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subscription)
            .cloned();
        Box::pin(async move {
            if let Some(sender) = sender {
                let _ = sender
                    .send(Ok(Update::Event {
                        relay: relay.clone(),
                        event: Box::new(event.clone()),
                    }))
                    .await;
            }
            Ok(nostr_sdk::policy::AdmitStatus::Success)
        })
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Kind, event::FinalizeEvent};
    use nostr_sdk::policy::AdmitPolicy;

    use super::*;

    #[tokio::test]
    async fn observer_backpressures_instead_of_dropping_and_close_unblocks_it() {
        let observer = Observer::default();
        let id = SubscriptionId::generate();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let event = EventBuilder::new(Kind::Custom(9842), "")
            .finalize(&Keys::generate())
            .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        observer.insert(id.clone(), sender);
        observer.admit_event(&relay, &id, &event).await.unwrap();
        let second = observer.admit_event(&relay, &id, &event);
        tokio::pin!(second);
        assert!(futures::poll!(&mut second).is_pending());
        assert!(matches!(
            receiver.recv().await.unwrap().unwrap(),
            Update::Event { .. }
        ));
        second.await.unwrap();
        let third = observer.admit_event(&relay, &id, &event);
        tokio::pin!(third);
        assert!(futures::poll!(&mut third).is_pending());
        receiver.close();
        third.await.unwrap();
    }
}
