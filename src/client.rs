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
use anyhow::Result;
use async_trait::async_trait;
#[cfg(test)]
use mockall::*;
use nostr::Event;

pub struct Client {
    client: nostr_sdk::Client,
    pub fallback_relays: Vec<String>,
}

#[async_trait]
#[cfg_attr(test, automock)]
pub trait Connect {
    fn default() -> Self;
    fn new(opts: Params) -> Self;
    async fn connect(&self) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    async fn send_event_to(&self, url: &str, event: nostr::event::Event) -> Result<nostr::EventId>;
}

#[async_trait]
impl Connect for Client {
    fn default() -> Self {
        Client {
            client: nostr_sdk::Client::new(&nostr::Keys::generate()),
            fallback_relays: vec![
                "ws://localhost:8080".to_string(),
                "ws://localhost:8052".to_string(),
            ],
        }
    }
    fn new(opts: Params) -> Self {
        Client {
            client: nostr_sdk::Client::new(&opts.keys.unwrap_or(nostr::Keys::generate())),
            fallback_relays: opts.fallback_relays,
        }
    }
    async fn connect(&self) -> Result<()> {
        for relay in &self.fallback_relays {
            self.client.add_relay(relay.as_str(), None).await?;
        }
        self.client.connect().await;
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await?;
        Ok(())
    }

    async fn send_event_to(&self, url: &str, event: Event) -> Result<nostr::EventId> {
        Ok(self.client.send_event_to(url, event).await?)
    }
}

#[derive(Default)]
pub struct Params {
    pub keys: Option<nostr::Keys>,
    pub fallback_relays: Vec<String>,
}

impl Params {
    pub fn with_keys(mut self, keys: nostr::Keys) -> Self {
        self.keys = Some(keys);
        self
    }
    pub fn with_fallback_relays(mut self, fallback_relays: Vec<String>) -> Self {
        self.fallback_relays = fallback_relays;
        self
    }
}
