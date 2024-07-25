use std::collections::HashMap;

use anyhow::{bail, Result};
use nostr::{ClientMessage, JsonUtil, RelayMessage};

use crate::CliTester;

type ListenerEventFunc<'a> = &'a dyn Fn(&mut Relay, u64, nostr::Event) -> Result<()>;
pub type ListenerReqFunc<'a> =
    &'a dyn Fn(&mut Relay, u64, nostr::SubscriptionId, Vec<nostr::Filter>) -> Result<()>;

pub struct Relay<'a> {
    port: u16,
    event_hub: simple_websockets::EventHub,
    clients: HashMap<u64, simple_websockets::Responder>,
    pub events: Vec<nostr::Event>,
    pub reqs: Vec<Vec<nostr::Filter>>,
    event_listener: Option<ListenerEventFunc<'a>>,
    req_listener: Option<ListenerReqFunc<'a>>,
}

impl<'a> Relay<'a> {
    pub fn new(
        port: u16,
        event_listener: Option<ListenerEventFunc<'a>>,
        req_listener: Option<ListenerReqFunc<'a>>,
    ) -> Self {
        let event_hub = simple_websockets::launch(port)
            .unwrap_or_else(|_| panic!("failed to listen on port {port}"));
        Self {
            port,
            events: vec![],
            reqs: vec![],
            event_hub,
            clients: HashMap::new(),
            event_listener,
            req_listener,
        }
    }
    pub fn respond_ok(
        &self,
        client_id: u64,
        event: nostr::Event,
        error: Option<&str>,
    ) -> Result<bool> {
        let responder = self.clients.get(&client_id).unwrap();

        let ok_json = RelayMessage::Ok {
            event_id: event.id,
            status: error.is_none(),
            message: error.unwrap_or("").to_string(),
        }
        .as_json();
        // bail!(format!("{}", &ok_json));
        Ok(responder.send(simple_websockets::Message::Text(ok_json)))
    }

    pub fn respond_eose(
        &self,
        client_id: u64,
        subscription_id: nostr::SubscriptionId,
    ) -> Result<bool> {
        let responder = self.clients.get(&client_id).unwrap();

        Ok(responder.send(simple_websockets::Message::Text(
            RelayMessage::EndOfStoredEvents(subscription_id).as_json(),
        )))
    }

    /// send events and eose
    pub fn respond_events(
        &self,
        client_id: u64,
        subscription_id: &nostr::SubscriptionId,
        events: &Vec<nostr::Event>,
    ) -> Result<bool> {
        let responder = self.clients.get(&client_id).unwrap();

        for event in events {
            let res = responder.send(simple_websockets::Message::Text(
                RelayMessage::Event {
                    subscription_id: subscription_id.clone(),
                    event: Box::new(event.clone()),
                }
                .as_json(),
            ));
            if !res {
                return Ok(false);
            }
        }
        self.respond_eose(client_id, subscription_id.clone())
    }

    /// send collected events, filtered by filters, and eose
    pub fn respond_standard_req(
        &self,
        client_id: u64,
        subscription_id: &nostr::SubscriptionId,
        // TODO: enable filters
        filters: &[nostr::Filter],
    ) -> Result<bool> {
        self.respond_events(
            client_id,
            subscription_id,
            &self
                .events
                .iter()
                .filter(|e| filters.iter().any(|filter| filter.match_event(e)))
                .filter(|_| true)
                .cloned()
                .collect(),
        )
    }
    /// listen, collect events and responds with event_listener to events or
    /// Ok(eventid) if event_listner is None
    pub async fn listen_until_close(&mut self) -> Result<()> {
        loop {
            println!("{} polling", self.port);
            match self.event_hub.poll_async().await {
                simple_websockets::Event::Connect(client_id, responder) => {
                    // add their Responder to our `clients` map:
                    self.clients.insert(client_id, responder);
                }
                simple_websockets::Event::Disconnect(client_id) => {
                    // remove the disconnected client from the clients map:
                    println!("{} disconnected", self.port);
                    self.clients.remove(&client_id);
                    // break;
                }
                simple_websockets::Event::Message(client_id, message) => {
                    // println!("bla{:?}", &message);

                    println!(
                        "{} Received a message from client #{}: {:?}",
                        self.port, client_id, message
                    );
                    if let simple_websockets::Message::Text(s) = message.clone() {
                        if s.eq("shut me down") {
                            println!("{} recieved shut me down", self.port);
                            break;
                        }
                    }
                    // println!("{:?}", &message);
                    if let Ok(event) = get_nevent(&message) {
                        // println!("{:?}", &event);
                        // let t: Vec<nostr::Kind> = self.events.iter().map(|e| e.kind).collect();
                        // println!("before{:?}", t);
                        self.events.push(event.clone());
                        // let t: Vec<nostr::Kind> = self.events.iter().map(|e| e.kind).collect();
                        // println!("after{:?}", t);

                        if let Some(listner) = self.event_listener {
                            listner(self, client_id, event)?;
                        } else {
                            self.respond_ok(client_id, event, None)?;
                        }
                    }

                    if let Ok((subscription_id, filters)) = get_nreq(&message) {
                        self.reqs.push(filters.clone());
                        if let Some(listner) = self.req_listener {
                            listner(self, client_id, subscription_id, filters)?;
                        } else {
                            self.respond_standard_req(client_id, &subscription_id, &filters)?;
                            // self.respond_eose(client_id, subscription_id)?;
                        }
                        // respond with events
                        // respond with EOSE
                    }
                    if is_nclose(&message) {
                        println!("{} recieved nostr close", self.port);
                        // break;
                    }
                }
            }
        }
        println!(
            "{} stop polling. we may not be polling but the tcplistner is still listening",
            self.port
        );
        Ok(())
    }
}

pub fn shutdown_relay(port: u64) -> Result<()> {
    let mut counter = 0;
    while let Ok((mut socket, _)) = tungstenite::connect(format!("ws://localhost:{}", port)) {
        counter += 1;
        if counter == 1 {
            socket.write(tungstenite::Message::text("shut me down"))?;
        }
        socket.close(None)?;
    }
    Ok(())
}

fn get_nevent(message: &simple_websockets::Message) -> Result<nostr::Event> {
    if let simple_websockets::Message::Text(s) = message.clone() {
        let cm_result = ClientMessage::from_json(s);
        if let Ok(ClientMessage::Event(event)) = cm_result {
            let e = *event;
            return Ok(e.clone());
        }
    }
    bail!("not nostr event")
}

fn get_nreq(
    message: &simple_websockets::Message,
) -> Result<(nostr::SubscriptionId, Vec<nostr::Filter>)> {
    if let simple_websockets::Message::Text(s) = message.clone() {
        let cm_result = ClientMessage::from_json(s);
        if let Ok(ClientMessage::Req {
            subscription_id,
            filters,
        }) = cm_result
        {
            return Ok((subscription_id, filters));
        }
    }
    bail!("not nostr event")
}

fn is_nclose(message: &simple_websockets::Message) -> bool {
    if let simple_websockets::Message::Text(s) = message.clone() {
        let cm_result = ClientMessage::from_json(s);
        if let Ok(ClientMessage::Close(_)) = cm_result {
            return true;
        }
    }
    false
}

pub enum Message {
    Event,
    // Request,
}

/// leaves trailing whitespace and only compatible with --no-cli-spinners flag
/// relays tuple: (title,successful,message)
pub fn expect_send_with_progress(
    p: &mut CliTester,
    relays: Vec<(&str, bool, &str)>,
    event_count: u16,
) -> Result<()> {
    p.expect(format!(
        " - {} -------------------- 0/{event_count}",
        &relays[0].0
    ))?;
    let last_relay_outcome = outcome_message(relays.last().unwrap());
    let mut s = String::new();
    loop {
        s.push_str(&p.expect_eventually(&last_relay_outcome)?);
        s.push_str(&last_relay_outcome);
        if relays.iter().all(|r| s.contains(&outcome_message(r))) {
            // all responses have been received with correct outcome
            break;
        }
    }
    Ok(())
}

fn outcome_message(relay: &(&str, bool, &str)) -> String {
    if relay.1 {
        format!(" y {}", relay.0)
    } else {
        format!(" x {} {}", relay.0, relay.2)
    }
}

pub fn expect_send_with_progress_exact_interaction(
    p: &mut CliTester,
    titles: Vec<&str>,
    count: u16,
) -> Result<()> {
    let whitespace_mid = " \r\n";
    let whitespace_end = "                   \r\r\r";

    p.expect(format!(
        " - {} -------------------- 0/{count}        \r",
        titles[0]
    ))?;
    p.expect(format!(
        " - {} -------------------- 0/{count}{whitespace_mid}",
        titles[0]
    ))?;
    p.expect(format!(
        " - {} -------------------- 0/{count}                     \r\r",
        titles[1]
    ))?;

    let generate_text = |title: &str, num: u16, confirmed_complete: bool| -> String {
        let symbol = if confirmed_complete && num.eq(&count) {
            "â"
        } else {
            "-"
        };
        let bar = match (num, count) {
            (0, _) => "--------------------",
            (1, 2) => "###########---------",
            (x, y) => {
                if x.eq(&y) {
                    "####################"
                } else {
                    "--unknown--"
                }
            }
        };
        format!(
            " {symbol} {title} {bar} {num}/{count}{}",
            if (&title).eq(titles.last().unwrap()) {
                whitespace_end
            } else {
                whitespace_mid
            }
        )
    };
    let mut nums: HashMap<&str, u16> = HashMap::new();
    for title in &titles {
        nums.insert(title, 0);
        p.expect(generate_text(title, 0, false))?;
    }
    loop {
        for selected_title in &titles {
            for title in &titles {
                if title.eq(selected_title) {
                    let new_num = nums.get(title).unwrap() + 1;
                    if new_num.gt(&count) {
                        return Ok(());
                    }
                    nums.insert(title, new_num);
                    p.expect(generate_text(title, *nums.get(title).unwrap(), false))?;
                } else {
                    p.expect(generate_text(title, *nums.get(title).unwrap(), true))?;
                }
            }
        }
    }
}
