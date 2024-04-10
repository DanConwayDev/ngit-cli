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
use std::{fmt::Write, time::Duration};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
#[cfg(test)]
use mockall::*;
use nostr::Event;
use nostr_sdk::NostrSigner;

#[allow(clippy::struct_field_names)]
pub struct Client {
    client: nostr_sdk::Client,
    fallback_relays: Vec<String>,
    more_fallback_relays: Vec<String>,
    blaster_relays: Vec<String>,
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
    fn get_blaster_relays(&self) -> &Vec<String>;
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
                "wss://relay.damus.io".to_string(), /* free, good reliability, have been known
                                                     * to delete all messages */
                "wss://nos.lol".to_string(),
                "wss://relay.nostr.band".to_string(),
                "wss://relay.f7z.io".to_string(),
            ]
        };

        let more_fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8055".to_string(),
                "ws://localhost:8056".to_string(),
            ]
        } else {
            vec![
                "wss://purplerelay.com".to_string(), // free but reliability not tested
                "wss://purplepages.es".to_string(),  // for profile events but unreliable
                "wss://relayable.org".to_string(),   // free but not always reliable
            ]
        };

        let blaster_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec!["ws://localhost:8057".to_string()]
        } else {
            vec!["wss://nostr.mutinywallet.com".to_string()]
        };
        Client {
            client: nostr_sdk::Client::new(&nostr::Keys::generate()),
            fallback_relays,
            more_fallback_relays,
            blaster_relays,
        }
    }
    fn new(opts: Params) -> Self {
        Client {
            client: nostr_sdk::Client::new(&opts.keys.unwrap_or(nostr::Keys::generate())),
            fallback_relays: opts.fallback_relays,
            more_fallback_relays: opts.more_fallback_relays,
            blaster_relays: opts.blaster_relays,
        }
    }

    async fn set_keys(&mut self, keys: &nostr::Keys) {
        self.client
            .set_signer(Some(NostrSigner::Keys(keys.clone())))
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

    fn get_blaster_relays(&self) -> &Vec<String> {
        &self.blaster_relays
    }

    async fn send_event_to(&self, url: &str, event: Event) -> Result<nostr::EventId> {
        self.client.add_relay(url).await?;
        #[allow(clippy::large_futures)]
        self.client.connect_relay(url).await?;
        Ok(self.client.send_event_to(vec![url], event).await?)
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

        let m = MultiProgress::new();
        let pb_style = ProgressStyle::with_template(" {spinner} {prefix} {msg} {timeout_in}")?
            .with_key("timeout_in", |state: &ProgressState, w: &mut dyn Write| {
                if state.elapsed().as_secs() > 3 && state.elapsed().as_secs() < GET_EVENTS_TIMEOUT {
                    write!(
                        w,
                        "timeout in {:.1}s",
                        GET_EVENTS_TIMEOUT - state.elapsed().as_secs()
                    )
                    .unwrap();
                }
            });

        let pb_after_style = |succeed| {
            ProgressStyle::with_template(
                format!(
                    " {} {}",
                    if succeed {
                        console::style("✔".to_string())
                            .for_stderr()
                            .green()
                            .to_string()
                    } else {
                        console::style("✘".to_string())
                            .for_stderr()
                            .red()
                            .to_string()
                    },
                    "{prefix} {msg}",
                )
                .as_str(),
            )
        };

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
                let pb = if std::env::var("NGITTEST").is_err() {
                    let pb = m.add(
                        ProgressBar::new(1)
                            .with_prefix(format!("{: <11}{}", "connecting", relay.url()))
                            .with_style(pb_style.clone()),
                    );
                    pb.enable_steady_tick(Duration::from_millis(300));
                    Some(pb)
                } else {
                    None
                };
                #[allow(clippy::large_futures)]
                match get_events_of(relay, filters, &pb).await {
                    Err(error) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(false)?);
                            pb.set_prefix(format!("{: <11}{}", "error", relay.url()));
                            pb.finish_with_message(
                                console::style(
                                    error.to_string().replace("relay pool error:", "error:"),
                                )
                                .for_stderr()
                                .red()
                                .to_string(),
                            );
                        }
                        Err(error)
                    }
                    Ok(res) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(true)?);
                            pb.set_prefix(format!(
                                "{: <11}{}",
                                format!("{} events", res.len()),
                                relay.url()
                            ));
                            pb.finish_with_message("");
                        }
                        Ok(res)
                    }
                }
            })
            .collect();

        let relay_results = stream::iter(futures).buffer_unordered(15).collect().await;

        Ok(get_dedup_events(relay_results))
    }
}

static CONNECTION_TIMEOUT: u64 = 3;
static GET_EVENTS_TIMEOUT: u64 = 7;

async fn get_events_of(
    relay: &nostr_sdk::Relay,
    filters: Vec<nostr::Filter>,
    pb: &Option<ProgressBar>,
) -> Result<Vec<Event>> {
    if !relay.is_connected().await {
        #[allow(clippy::large_futures)]
        relay
            .connect(Some(std::time::Duration::from_secs(CONNECTION_TIMEOUT)))
            .await;
    }

    if !relay.is_connected().await {
        bail!("connection timeout");
    } else if let Some(pb) = pb {
        pb.set_prefix(format!("connected  {}", relay.url()));
    }
    let events = relay
        .get_events_of(
            filters,
            // 20 is nostr_sdk default
            std::time::Duration::from_secs(GET_EVENTS_TIMEOUT),
            nostr_sdk::FilterOptions::ExitOnEOSE,
        )
        .await?;
    Ok(events)
}

#[derive(Default)]
pub struct Params {
    pub keys: Option<nostr::Keys>,
    pub fallback_relays: Vec<String>,
    pub more_fallback_relays: Vec<String>,
    pub blaster_relays: Vec<String>,
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
