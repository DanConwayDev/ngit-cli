// have you considered

// TO USE ASYNC

// in traits (required for mocking unit tests)
// https://rust-lang.github.io/async-book/07_workarounds/05_async_in_traits.html
// https://github.com/dtolnay/async-trait
// see https://blog.rust-lang.org/inside-rust/2022/11/17/async-fn-in-trait-nightly.html
// I think we can use the async-trait crate and switch to the native feature
// which is currently in nightly. alternatively we can use nightly as it looks
// certain that the implementation is going to make it to stable but we don't
// want to inadvertlty use other features of nightly that might be removed.
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
#[cfg(test)]
use mockall::*;
use nostr::Event;
use nostr_sdk::ClientSigner;

#[allow(clippy::struct_field_names)]
pub struct Client {
    client: nostr_sdk::Client,
    fallback_relays: Vec<String>,
    more_fallback_relays: Vec<String>,
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Connect {
    fn default() -> Self;
    fn new(opts: Params) -> Self;
    async fn set_keys(&mut self, keys: &nostr::Keys);
    async fn disconnect(&self) -> Result<()>;
    fn get_fallback_relays(&self) -> &Vec<String>;
    fn get_more_fallback_relays(&self) -> &Vec<String>;
    async fn send_event_to(&self, url: &str, event: nostr::event::Event) -> Result<nostr::EventId>;
    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>>;
}

#[async_trait]
impl Connect for Client {
    fn default() -> Self {
        let fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8051".to_string(),
                "ws://localhost:8052".to_string(),
            ]
        } else {
            vec![
                "wss://purplepages.es".to_string(),
                "wss://relay.damus.io".to_string(),
                "wss://nostr-pub.wellorder.net".to_string(),
                "wss://nos.lol".to_string(),
                // "ws://localhost:8080".to_string()
            ]
        };

        let more_fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8055".to_string(),
                "ws://localhost:8056".to_string(),
            ]
        } else {
            vec![
                "wss://nostr.wine/".to_string(),
                "wss://eden.nostr.land/".to_string(),
                "wss://relay.nostr.band/".to_string(),
                // "ws://localhost:8080".to_string()
            ]
        };

        Client {
            client: nostr_sdk::Client::new(&nostr::Keys::generate()),
            fallback_relays,
            more_fallback_relays,
        }
    }
    fn new(opts: Params) -> Self {
        Client {
            client: nostr_sdk::Client::new(&opts.keys.unwrap_or(nostr::Keys::generate())),
            fallback_relays: opts.fallback_relays,
            more_fallback_relays: opts.more_fallback_relays,
        }
    }

    async fn set_keys(&mut self, keys: &nostr::Keys) {
        self.client
            .set_signer(Some(ClientSigner::Keys(keys.clone())))
            .await;
    }

    async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await?;
        Ok(())
    }

    fn get_fallback_relays(&self) -> &Vec<String> {
        &self.fallback_relays
    }

    fn get_more_fallback_relays(&self) -> &Vec<String> {
        &self.more_fallback_relays
    }

    async fn send_event_to(&self, url: &str, event: Event) -> Result<nostr::EventId> {
        self.client.add_relay(url).await?;
        self.client.connect_relay(url).await?;
        Ok(self.client.send_event_to(url, event).await?)
    }

    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>> {
        // add relays
        for relay in &relays {
            self.client
                .add_relay(relay.as_str())
                .await
                .context("cannot add relay")?;
        }

        let relays_map = self.client.relays().await;

        let futures: Vec<_> = relays
            .clone()
            .iter()
            // don't look for events on blaster
            .filter(|r| !r.contains("nostr.mutinywallet.com"))
            .map(|r| {
                (
                    relays_map.get(&nostr::Url::parse(r).unwrap()).unwrap(),
                    filters.clone(),
                )
            })
            .map(|(relay, filters)| async {
                match get_events_of(relay, filters).await {
                    Err(error) => {
                        println!("{} {}", error, relay.url());
                        Err(error)
                    }
                    res => res,
                }
            })
            .collect();

        let relay_results = stream::iter(futures).buffer_unordered(15).collect().await;

        Ok(get_dedup_events(relay_results))
    }
}

async fn get_events_of(
    relay: &nostr_sdk::Relay,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<Event>> {
    println!("fetching from {}", relay.url());
    if !relay.is_connected().await {
        relay.connect(None).await;
    }
    let events = relay
        .get_events_of(
            filters,
            // 20 is nostr_sdk default
            std::time::Duration::from_secs(10),
            nostr_sdk::FilterOptions::ExitOnEOSE,
        )
        .await
        .context("failed to get events from relay")?;
    println!("fetched {} events from {}", events.len(), relay.url());
    Ok(events)
}

#[derive(Default)]
pub struct Params {
    pub keys: Option<nostr::Keys>,
    pub fallback_relays: Vec<String>,
    pub more_fallback_relays: Vec<String>,
}

fn get_dedup_events(relay_results: Vec<Result<Vec<nostr::Event>>>) -> Vec<Event> {
    let mut dedup_events: Vec<Event> = vec![];
    for events in relay_results.into_iter().flatten() {
        for event in events {
            if !dedup_events.iter().any(|e| event.id.eq(&e.id)) {
                dedup_events.push(event);
            }
        }
    }
    dedup_events
}
