use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path};
use std::time::Duration;

use dialoguer::{Select, Input};
use dialoguer::theme::ColorfulTheme;
use nostr_sdk::blocking::Client;
use nostr_sdk::prelude::*;

use crate::config::{MyConfig, save_conifg};

pub fn handle_keys(private_key: Option<String>, hex: bool) -> Result<Keys> {
    // Parse and validate private key
    let keys = match private_key {
        Some(pk) => {
            // create a new identity using the provided private key
            Keys::from_sk_str(pk.as_str())?
        }
        None => {
            // create a new identity with a new keypair
            println!("No private key provided, creating new identity");
            Keys::generate()
        }
    };

    if !hex {
        println!("Private key: {}", keys.secret_key()?.to_bech32()?);
        println!("Public key: {}", keys.public_key().to_bech32()?);
    } else {
        println!("Private key: {}", keys.secret_key()?.display_secret());
        println!("Public key: {}", keys.public_key());
    }
    Ok(keys)
}

// Creates the websocket client that is used for communicating with relays
pub fn create_client(keys: &Keys, relays: Vec<String>) -> Result<Client> {
    let opts = Options::new()
        .wait_for_send(true)
        .timeout(Some(Duration::from_secs(7)));
    let client = Client::with_opts(keys, opts);
    let relays = relays.iter().map(|url| (url, None)).collect();
    client.add_relays(relays)?;
    client.connect();
    Ok(client)
}

// Accepts both hex and bech32 keys and returns the hex encoded key
pub fn parse_key(key: String) -> Result<String> {
    // Check if the key is a bech32 encoded key
    let parsed_key = if key.starts_with("npub") {
        XOnlyPublicKey::from_bech32(key)?.to_string()
    } else if key.starts_with("nsec") {
        SecretKey::from_bech32(key)?.display_secret().to_string()
    } else if key.starts_with("note") {
        EventId::from_bech32(key)?.to_hex()
    } else if key.starts_with("nchannel") {
        ChannelId::from_bech32(key)?.to_hex()
    } else {
        // If the key is not bech32 encoded, return it as is
        key
    };
    Ok(parsed_key)
}

pub fn get_stored_keys(cfg:&mut MyConfig) -> Option<Keys> {
    match &cfg.private_key {
        None => None,
        Some(k) => Some(Keys::new(*k)),
    }
}

pub fn get_or_generate_keys(cfg:&mut MyConfig) -> Keys {
    match cfg.private_key {
        None => {
            let selection = Select::with_theme(&ColorfulTheme::default())
                .items(&vec!["enter existing private key", "generate new keys"])
                .default(0)
                .with_prompt("no keys are stored")
                .interact().unwrap();
            let key = match selection {
                0 => {
                    let mut prompt = "secret key (nsec, hex, etc)";
                    loop {
                        let pk: String = Input::with_theme(&ColorfulTheme::default())
                            .with_prompt(prompt)
                            .interact_text()
                            .unwrap();
                        match Keys::from_sk_str(&pk) {
                            Ok(key) => { break key; },
                            Err(_e) => { prompt = "error interpeting secret key. try again with nsec, hex, etc"; },
                        }
                    }

                }
                _ => Keys::generate(),
            };
            cfg.private_key = Some(key.secret_key().unwrap());
            save_conifg(&cfg);
            key
        }
        Some(k) => Keys::new(k),
    }
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum Prefix {
    Npub,
    Nsec,
    Note,
    Nchannel,
}


/// [`LoadFile`] error
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error loading event file
    #[error("cannot load event file.")]
    // LoadFile(#[from] init::Error),
    LoadFile(),
}

pub fn load_file<P: AsRef<Path>>(path: P) -> Result<String,Error> {
    let mut buf = vec![];
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut buf)
                .expect("read_to_end not to error on file");
            Ok(
            std::str::from_utf8(&buf[..])
                .expect("file contents u8 to convert to str")
                .to_string(),
            )
        },
        Err(_e) => { Err(Error::LoadFile()) },
    }
    
}

pub fn load_event<P: AsRef<Path>>(path: P) -> Result<Event,Error> {
    if let Ok(mut file) = File::open(path) {
        let mut buf = vec![];
        if file.read_to_end(&mut buf).is_ok() {
            if let Ok(event) = Event::from_json(std::str::from_utf8(&buf[..]).unwrap()) {
                return Ok(event)
            }
        }
    }
    Err(Error::LoadFile())
}

pub fn save_event<P: AsRef<Path>>(path: P, event: &Event) -> Result<()> {
    let mut f = File::create(path)?;
    f.write_all(&event.as_json().as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_hex_input() {
        let hex_key =
            String::from("f4deaad98b61fa24d86ef315f1d5d57c1a6a533e1e87e777e5d0b48dcd332cdb");
        let result = parse_key(hex_key.clone());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), hex_key);
    }

    #[test]
    fn test_parse_key_bech32_note_input() {
        let bech32_note_id =
            String::from("note1h445ule4je70k7kvddate8kpsh2fd6n77esevww5hmgda2qwssjsw957wk");
        let result = parse_key(bech32_note_id);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            String::from("bd6b4e7f35967cfb7acc6b7abc9ec185d496ea7ef6619639d4bed0dea80e8425")
        );
    }

    #[test]
    fn test_parse_bech32_public_key_input() {
        let bech32_encoded_key =
            String::from("npub1ktt8phjnkfmfrsxrgqpztdjuxk3x6psf80xyray0l3c7pyrln49qhkyhz0");
        let result = parse_key(bech32_encoded_key);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            String::from("b2d670de53b27691c0c3400225b65c35a26d06093bcc41f48ffc71e0907f9d4a")
        );
    }

    #[test]
    fn test_parse_bech32_private_key() {
        let bech32_encoded_key =
            String::from("nsec1hdeqm0y8vgzuucqv4840h7rlpy4qfu928ulxh3dzj6s2nqupdtzqagtew3");
        let result = parse_key(bech32_encoded_key);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            String::from("bb720dbc876205ce600ca9eafbf87f092a04f0aa3f3e6bc5a296a0a983816ac4")
        );
    }
}
