use std::collections::HashMap;

use anyhow::{bail, Result};
use nostr::{ClientMessage, RelayMessage};

use crate::CliTester;

type ListenerFunc<'a> = &'a dyn Fn(&mut Relay, u64, nostr::Event) -> Result<()>;

pub struct Relay<'a> {
    port: u16,
    event_hub: simple_websockets::EventHub,
    clients: HashMap<u64, simple_websockets::Responder>,
    pub events: Vec<nostr::Event>,
    event_listener: Option<ListenerFunc<'a>>,
}

impl<'a> Relay<'a> {
    pub fn new(port: u16, event_listener: Option<ListenerFunc<'a>>) -> Self {
        let event_hub = simple_websockets::launch(port)
            .unwrap_or_else(|_| panic!("failed to listen on port {port}"));
        Self {
            port,
            events: vec![],
            event_hub,
            clients: HashMap::new(),
            event_listener,
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
    /// listen, collect events and responds with event_listener to events or
    /// Ok(eventid) if event_listner is None
    pub async fn listen_until_close(&mut self) -> Result<()> {
        loop {
            println!("polling");
            match self.event_hub.poll_async().await {
                simple_websockets::Event::Connect(client_id, responder) => {
                    // add their Responder to our `clients` map:
                    self.clients.insert(client_id, responder);
                }
                simple_websockets::Event::Disconnect(client_id) => {
                    // remove the disconnected client from the clients map:
                    println!("{} disconnected", self.port);
                    self.clients.remove(&client_id);
                    break;
                }
                simple_websockets::Event::Message(client_id, message) => {
                    println!(
                        "Received a message from client #{}: {:?}",
                        client_id, message
                    );

                    if let Ok(event) = get_nevent(message) {
                        self.events.push(event.clone());
                        if let Some(listner) = self.event_listener {
                            listner(self, client_id, event)?;
                        } else {
                            self.respond_ok(client_id, event, None)?;
                        }
                    }
                }
            }
        }
        println!("stop polling");
        println!("we may not be polling but the tcplistner is still listening");
        Ok(())
    }
}

fn get_nevent(message: simple_websockets::Message) -> Result<nostr::Event> {
    if let simple_websockets::Message::Text(s) = message.clone() {
        let cm_result = ClientMessage::from_json(s);
        if let Ok(ClientMessage::Event(event)) = cm_result {
            let e = *event;
            return Ok(e.clone());
        }
    }
    bail!("not nostr event")
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
    for relay in &relays {
        // if successful
        if relay.1 {
            p.expect_eventually(format!(" y {}", relay.0))?;
        } else {
            p.expect_eventually(format!(" x {} {}", relay.0, relay.2))?;
        }
        // could check that before only contains titles:
        // - # y x n/n and whitespace
        // let before = p.expect_eventually(format!(" â {title}"))?;
        // assert_eq!("", before.trim());
    }
    Ok(())
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
