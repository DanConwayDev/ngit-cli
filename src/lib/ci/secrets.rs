//! NIP-C1 secret provisioning: ephemeral encryption keys and bound payloads.
use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail};
use nostr::prelude::{
    Coordinate, Event, EventBuilder, Keys, Kind, PublicKey, RelayUrl, Tag, Timestamp, nip44,
};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

pub struct SecretInbox {
    pub advertisement: EventId,
    pub coordinator: PublicKey,
    pub recipient: PublicKey,
    pub relays: Vec<RelayUrl>,
    pub created_at: Timestamp,
    pub expiration: Timestamp,
}
use nostr::prelude::EventId;

/// Validate the exact signed advertisement used for encryption and routing.
pub fn inbox(event: &Event, coordinator: PublicKey, now: Timestamp) -> Result<SecretInbox> {
    event
        .verify()
        .context("invalid coordinator advertisement signature")?;
    if event.kind != Kind::Custom(19843) || event.pubkey != coordinator || !event.content.is_empty()
    {
        bail!("invalid coordinator advertisement");
    }
    let one = |name: &str| -> Result<Vec<String>> {
        let tags: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.as_slice()[0] == name)
            .collect();
        if tags.len() != 1 {
            bail!("coordinator advertisement has missing or duplicate required tags");
        }
        Ok(tags[0].as_slice().to_vec())
    };
    if event
        .tags
        .iter()
        .any(|t| matches!(t.as_slice()[0].as_str(), "d" | "runs_on"))
    {
        bail!("invalid coordinator advertisement identity or capability tags");
    }
    for (name, values) in [
        (
            "M",
            &["operator-selected", "maintainer-request", "open"][..],
        ),
        ("X", &["automatic", "request-required"][..]),
    ] {
        let tag = one(name)?;
        if tag.len() < 2 || !values.contains(&tag[1].as_str()) {
            bail!("unknown coordinator admission policy");
        }
    }
    let optional_b: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "B")
        .collect();
    if optional_b.len() > 1
        || optional_b.first().is_some_and(|t| {
            t.as_slice().len() < 2
                || !matches!(t.as_slice()[1].as_str(), "not-required" | "out-of-band")
        })
    {
        bail!("unknown coordinator billing policy");
    }
    let families: HashSet<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "W")
        .map(|t| {
            if t.as_slice().len() < 2 || t.as_slice()[1].is_empty() {
                bail!("invalid coordinator runner family");
            }
            Ok(t.as_slice()[1].to_ascii_lowercase())
        })
        .collect::<Result<_>>()?;
    let mut selected = HashSet::new();
    for tag in event.tags.iter().filter(|t| t.as_slice()[0] == "R") {
        if tag.as_slice().len() < 2 {
            bail!("invalid coordinator runner selector");
        }
        let (family, selector) = tag.as_slice()[1]
            .split_once(':')
            .context("invalid coordinator runner selector")?;
        if selector.is_empty() || !families.contains(&family.to_ascii_lowercase()) {
            bail!("coordinator selector has no runner family");
        }
        selected.insert(family.to_ascii_lowercase());
    }
    if selected != families {
        bail!("coordinator runner family has no selector");
    }
    let expires = one("expiration")?;
    if expires.len() < 2 {
        bail!("invalid advertisement expiration");
    }
    let expiration = Timestamp::from_secs(
        expires[1]
            .parse()
            .context("invalid advertisement expiration")?,
    );
    if event.created_at > now
        || expiration <= now
        || expiration
            .as_secs()
            .saturating_sub(event.created_at.as_secs())
            > 1800
    {
        bail!("coordinator secret advertisement is not live; refresh it before provisioning");
    }
    let tag = one("secrets-key")?;
    if tag.len() < 4 || tag[1] != "nip44-v2" {
        bail!("coordinator does not advertise a supported secret inbox");
    }
    let recipient = PublicKey::from_hex(&tag[2]).context("invalid secret recipient")?;
    let relays = tag[3..]
        .iter()
        .map(|v| RelayUrl::parse(v).context("invalid secret inbox relay"))
        .collect::<Result<Vec<_>>>()?;
    Ok(SecretInbox {
        advertisement: event.id,
        coordinator,
        recipient,
        relays,
        created_at: event.created_at,
        expiration,
    })
}

#[derive(Default, Deserialize)]
#[serde(transparent)]
pub struct SecretValues(#[serde(deserialize_with = "unique_values")] pub BTreeMap<String, String>);
impl Drop for SecretValues {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}
fn unique_values<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique secret names and string values")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut values = SecretValues::default();
            while let Some((name, mut value)) = map.next_entry::<String, String>()? {
                if values.0.contains_key(&name) {
                    value.zeroize();
                    return Err(serde::de::Error::custom("duplicate secret name"));
                }
                values.0.insert(name, value);
            }
            Ok(std::mem::take(&mut values.0))
        }
    }
    deserializer.deserialize_map(Visitor)
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'_'))
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && ![
            "PATH",
            "HOME",
            "CI",
            "DOCKER_HOST",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "NIX_CONFIG",
        ]
        .contains(&name)
        && !["GITHUB_", "NGIT_CI_", "RUNNER_", "ACTIONS_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Encrypt one atomic update. Secret values never enter event tags or
/// diagnostics.
pub fn update(
    author: PublicKey,
    repository: Coordinate,
    relay_hint: &str,
    inbox: &SecretInbox,
    values: &SecretValues,
    remove: &[String],
    now: Timestamp,
) -> Result<EventBuilder> {
    if repository.kind != Kind::GitRepoAnnouncement || repository.public_key != author {
        bail!("secret scope must be rooted in the provisioning maintainer");
    }
    if now < inbox.created_at || now >= inbox.expiration {
        bail!("secret inbox expired before publication");
    }
    let count = values.0.len() + remove.len();
    if !(1..=100).contains(&count) {
        bail!("secret update requires between 1 and 100 names");
    }
    let mut names = HashSet::new();
    for (name, value) in &values.0 {
        if !valid_name(name) || value.is_empty() || value.len() > 16_384 {
            bail!("invalid secret name, reserved name or value size");
        }
        names.insert(name.as_str());
    }
    for name in remove {
        if !valid_name(name) || !names.insert(name) {
            bail!("invalid, reserved, overlapping or duplicate secret name");
        }
    }
    #[derive(Serialize)]
    struct Payload<'a> {
        author: String,
        created_at: u64,
        set: &'a BTreeMap<String, String>,
        remove: &'a [String],
    }
    let payload = Zeroizing::new(serde_json::to_string(&Payload {
        author: author.to_hex(),
        created_at: now.as_secs(),
        set: &values.0,
        remove,
    })?);
    if payload.len() > 65_535 {
        bail!("secret update exceeds the plaintext size limit");
    }
    let sender = Keys::generate();
    let ciphertext = nip44::encrypt(
        sender.secret_key(),
        &inbox.recipient,
        payload.as_bytes(),
        nip44::Version::V2,
    )
    .context("failed to encrypt CI secrets")?;
    if ciphertext.len() > 100 * 1024 {
        bail!("secret ciphertext exceeds the protocol limit");
    }
    Ok(EventBuilder::new(Kind::Custom(29846), ciphertext)
        .custom_created_at(now)
        .tags([
            Tag::parse(vec!["a".into(), repository.to_string(), relay_hint.into()])?,
            Tag::public_key(inbox.coordinator),
            Tag::parse(vec![
                "e".into(),
                inbox.advertisement.to_hex(),
                relay_hint.into(),
                "secrets-key".into(),
            ])?,
            Tag::custom("sender", [sender.public_key().to_hex()]),
            Tag::custom("recipient", [inbox.recipient.to_hex()]),
            Tag::custom("encryption", ["nip44-v2"]),
        ]))
}

#[cfg(test)]
mod tests {
    use nostr::prelude::event::FinalizeEvent;

    use super::*;
    #[test]
    fn encrypted_update_binds_author_time_and_fresh_sender() {
        let author = Keys::generate();
        let recipient = Keys::generate();
        let coordinator = Keys::generate();
        let ad = EventBuilder::new(Kind::Custom(19843), "")
            .custom_created_at(Timestamp::from_secs(100))
            .tags([
                Tag::custom("M", ["maintainer-request"]),
                Tag::custom("X", ["request-required"]),
                Tag::custom("expiration", ["1000"]),
                Tag::custom(
                    "secrets-key",
                    [
                        "nip44-v2".into(),
                        recipient.public_key().to_hex(),
                        "wss://inbox.example".into(),
                    ],
                ),
            ])
            .finalize(&coordinator)
            .unwrap();

        let extended = crate::ci::kinds::test_events::with_tags(&coordinator, &ad, |tags| {
            for tag in tags.iter_mut().filter(|t| t.as_slice()[0] != "secrets-key") {
                let mut fields = tag.as_slice().to_vec();
                fields.push("future".into());
                *tag = Tag::parse(fields).unwrap();
            }
            tags.extend([
                Tag::custom("future", ["one"]),
                Tag::custom("future", ["two"]),
            ]);
        });
        let extended_inbox = inbox(
            &extended,
            coordinator.public_key(),
            Timestamp::from_secs(200),
        )
        .unwrap();
        assert_eq!(extended_inbox.recipient, recipient.public_key());
        let bad = crate::ci::kinds::test_events::with_tags(&coordinator, &extended, |tags| {
            tags.push(Tag::custom("M", ["open", "future"]));
        });
        assert!(inbox(&bad, coordinator.public_key(), Timestamp::from_secs(200)).is_err());
        let inbox = inbox(&ad, coordinator.public_key(), Timestamp::from_secs(200)).unwrap();
        let scope =
            Coordinate::new(Kind::GitRepoAnnouncement, author.public_key()).identifier("repo");
        let values = SecretValues(BTreeMap::from([("TOKEN".into(), "sensitive-value".into())]));
        let event = update(
            author.public_key(),
            scope.clone(),
            "",
            &inbox,
            &values,
            &[],
            Timestamp::from_secs(200),
        )
        .unwrap()
        .finalize(&author)
        .unwrap();
        let sender = event
            .tags
            .iter()
            .find(|t| t.as_slice()[0] == "sender")
            .unwrap();
        let sender = PublicKey::from_hex(&sender.as_slice()[1]).unwrap();
        assert_ne!(sender, author.public_key());
        let plaintext = nip44::decrypt(recipient.secret_key(), &sender, &event.content).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&plaintext).unwrap();
        assert_eq!(payload["author"], author.public_key().to_hex());
        assert_eq!(payload["created_at"], 200);
        assert_eq!(payload["set"]["TOKEN"], "sensitive-value");
        assert!(
            !event
                .tags
                .iter()
                .any(|t| t.as_slice().iter().any(|v| v.contains("sensitive-value")))
        );
        assert!(
            update(
                author.public_key(),
                scope,
                "",
                &inbox,
                &values,
                &["TOKEN".into()],
                Timestamp::from_secs(200)
            )
            .is_err()
        );
        assert!(super::inbox(&ad, coordinator.public_key(), Timestamp::from_secs(1000)).is_err());
        assert!(serde_json::from_str::<SecretValues>(r#"{"TOKEN":"one","TOKEN":"two"}"#).is_err());
    }
}
