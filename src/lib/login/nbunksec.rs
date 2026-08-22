use anyhow::{Context, Result, bail};
use bech32::{Bech32, Hrp, primitives::decode::CheckedHrpstring};
use nostr::prelude::{Keys, NostrConnectUri, PublicKey, RelayUrl, SecretKey};

const HRP: &str = "nbunksec";
const MAX_ENCODED_LENGTH: usize = 1000;
const REMOTE_SIGNER_PUBKEY: u8 = 0;
const CLIENT_SECRET_KEY: u8 = 1;
const RELAY: u8 = 2;
const BUNKER_SECRET: u8 = 3;

/// The NIP-46 connection material carried by an `nbunksec` string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BunkerConnection {
    pub bunker_uri: String,
    pub client_key: String,
}

/// Decode the Applesauce/nsyte `nbunksec` TLV representation.
pub fn decode(value: &str) -> Result<BunkerConnection> {
    if value.len() > MAX_ENCODED_LENGTH {
        bail!("nbunksec exceeds the interoperable 1000-character limit");
    }
    let checked = CheckedHrpstring::new::<Bech32>(value).context("invalid nbunksec encoding")?;
    if checked.hrp().as_str() != HRP {
        bail!("invalid nbunksec prefix");
    }
    let data: Vec<u8> = checked.byte_iter().collect();
    let mut offset = 0;
    let mut remote_signer_public_key = None;
    let mut client_secret_key = None;
    let mut relays = Vec::new();
    let mut secret = None;

    while offset < data.len() {
        if data.len() - offset < 2 {
            bail!("nbunksec contains an incomplete TLV header");
        }
        let record_type = data[offset];
        let length = usize::from(data[offset + 1]);
        offset += 2;
        let end = offset
            .checked_add(length)
            .context("nbunksec TLV length overflow")?;
        if end > data.len() {
            bail!("nbunksec contains an incomplete TLV value");
        }
        let record = &data[offset..end];
        offset = end;

        match record_type {
            REMOTE_SIGNER_PUBKEY => {
                if remote_signer_public_key.is_some() {
                    bail!("nbunksec contains multiple remote signer pubkeys");
                }
                remote_signer_public_key = Some(
                    PublicKey::from_slice(record)
                        .context("nbunksec contains an invalid remote signer pubkey")?,
                );
            }
            CLIENT_SECRET_KEY => {
                if client_secret_key.is_some() {
                    bail!("nbunksec contains multiple client secret keys");
                }
                client_secret_key = Some(
                    SecretKey::from_slice(record)
                        .context("nbunksec contains an invalid client secret key")?,
                );
            }
            RELAY => {
                let relay = std::str::from_utf8(record)
                    .context("nbunksec contains a non-UTF-8 relay URL")?;
                relays.push(
                    RelayUrl::parse(relay).context("nbunksec contains an invalid relay URL")?,
                );
            }
            BUNKER_SECRET => {
                if secret.is_some() {
                    bail!("nbunksec contains multiple bunker secrets");
                }
                secret = Some(
                    std::str::from_utf8(record)
                        .context("nbunksec contains a non-UTF-8 bunker secret")?
                        .to_string(),
                );
            }
            // Unknown records are ignored so an optional future field, such
            // as an expected user pubkey, does not invalidate the connection.
            _ => {}
        }
    }

    let remote_signer_public_key =
        remote_signer_public_key.context("nbunksec is missing the remote signer pubkey")?;
    let client_secret_key =
        client_secret_key.context("nbunksec is missing the client secret key")?;
    if relays.is_empty() {
        bail!("nbunksec is missing a relay URL");
    }
    let bunker_uri = NostrConnectUri::Bunker {
        remote_signer_public_key,
        relays,
        secret,
    }
    .to_string();

    Ok(BunkerConnection {
        bunker_uri,
        client_key: client_secret_key.to_secret_hex(),
    })
}

/// Encode an established NIP-46 connection in the Applesauce/nsyte format.
pub fn encode(bunker_uri: &str, client_key: &str) -> Result<String> {
    let uri = NostrConnectUri::parse(bunker_uri).context("invalid bunker URI")?;
    let NostrConnectUri::Bunker {
        remote_signer_public_key,
        relays,
        secret,
    } = uri
    else {
        bail!("nbunksec requires a bunker:// URI");
    };
    if relays.is_empty() {
        bail!("nbunksec requires at least one relay URL");
    }
    let client_keys = Keys::parse(client_key).context("invalid bunker client key")?;
    let mut data = Vec::new();
    push_tlv(
        &mut data,
        REMOTE_SIGNER_PUBKEY,
        remote_signer_public_key.as_bytes(),
    )?;
    push_tlv(
        &mut data,
        CLIENT_SECRET_KEY,
        client_keys.secret_key().as_secret_bytes(),
    )?;
    for relay in relays {
        push_tlv(&mut data, RELAY, relay.as_str().as_bytes())?;
    }
    if let Some(secret) = secret {
        push_tlv(&mut data, BUNKER_SECRET, secret.as_bytes())?;
    }

    let encoded =
        bech32::encode::<Bech32>(Hrp::parse(HRP)?, &data).context("failed to encode nbunksec")?;
    if encoded.len() > MAX_ENCODED_LENGTH {
        bail!("nbunksec exceeds the interoperable 1000-character limit");
    }
    Ok(encoded)
}

fn push_tlv(target: &mut Vec<u8>, record_type: u8, value: &[u8]) -> Result<()> {
    let length = u8::try_from(value.len()).context("nbunksec TLV value exceeds 255 bytes")?;
    target.extend([record_type, length]);
    target.extend(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> (String, String) {
        let remote = Keys::parse(&"1".repeat(64)).unwrap();
        let client = Keys::parse(&"2".repeat(64)).unwrap();
        let uri = NostrConnectUri::Bunker {
            remote_signer_public_key: remote.public_key(),
            relays: vec![
                RelayUrl::parse("wss://relay.example.com").unwrap(),
                RelayUrl::parse("wss://relay2.example.com/path").unwrap(),
            ],
            secret: Some("pairing-secret".to_string()),
        };
        (uri.to_string(), client.secret_key().to_secret_hex())
    }

    #[test]
    fn round_trips_the_shared_nbunksec_fields() {
        let (uri, client_key) = connection();
        let encoded = encode(&uri, &client_key).unwrap();
        assert!(encoded.starts_with("nbunksec1"));
        assert_eq!(
            decode(&encoded).unwrap(),
            BunkerConnection {
                bunker_uri: uri,
                client_key
            }
        );
    }

    #[test]
    fn ignores_unknown_optional_fields() {
        let (uri, client_key) = connection();
        let encoded = encode(&uri, &client_key).unwrap();
        let checked = CheckedHrpstring::new::<Bech32>(&encoded).unwrap();
        let mut data: Vec<u8> = checked.byte_iter().collect();
        push_tlv(&mut data, 4, &[42; 32]).unwrap();
        let extended = bech32::encode::<Bech32>(Hrp::parse(HRP).unwrap(), &data).unwrap();

        assert_eq!(
            decode(&extended).unwrap(),
            BunkerConnection {
                bunker_uri: uri,
                client_key
            }
        );
    }

    #[test]
    fn rejects_missing_or_ambiguous_required_fields() {
        let remote = Keys::parse(&"1".repeat(64)).unwrap();
        let mut missing_client = Vec::new();
        push_tlv(
            &mut missing_client,
            REMOTE_SIGNER_PUBKEY,
            remote.public_key().as_bytes(),
        )
        .unwrap();
        push_tlv(&mut missing_client, RELAY, b"wss://relay.example.com").unwrap();
        let missing = bech32::encode::<Bech32>(Hrp::parse(HRP).unwrap(), &missing_client).unwrap();
        assert!(decode(&missing).is_err());

        let (uri, client_key) = connection();
        let encoded = encode(&uri, &client_key).unwrap();
        let checked = CheckedHrpstring::new::<Bech32>(&encoded).unwrap();
        let mut duplicated: Vec<u8> = checked.byte_iter().collect();
        push_tlv(
            &mut duplicated,
            REMOTE_SIGNER_PUBKEY,
            remote.public_key().as_bytes(),
        )
        .unwrap();
        let duplicated = bech32::encode::<Bech32>(Hrp::parse(HRP).unwrap(), &duplicated).unwrap();
        assert!(decode(&duplicated).is_err());
    }
}
