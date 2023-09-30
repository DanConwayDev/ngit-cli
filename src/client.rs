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
}

#[async_trait]
#[cfg_attr(test, automock)]
pub trait Connect {
    fn default() -> Self;
    fn new(opts: Params) -> Self;
    async fn connect(&self) -> Result<()>;
    async fn send_event_to(&self, url: &str, event: nostr::event::Event) -> Result<nostr::EventId>;
}

#[async_trait]
impl Connect for Client {
    fn default() -> Self {
        Client {
            client: nostr_sdk::Client::new(&nostr::Keys::generate()),
        }
    }
    fn new(opts: Params) -> Self {
        Client {
            client: nostr_sdk::Client::new(&opts.keys.unwrap_or(nostr::Keys::generate())),
        }
    }
    async fn connect(&self) -> Result<()> {
        self.client.add_relay("ws://localhost:8080", None).await?;
        self.client.connect().await;
        // self.client.s
        Ok(())
    }
    async fn send_event_to(&self, url: &str, event: Event) -> Result<nostr::EventId> {
        Ok(self.client.send_event_to(url, event).await?)
    }
}

#[derive(Default)]
pub struct Params {
    pub keys: Option<nostr::Keys>,
}

impl Params {
    pub fn with_keys(mut self, keys: nostr::Keys) -> Self {
        self.keys = Some(keys);
        self
    }
}
