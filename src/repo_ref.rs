use std::{fs::File, io::BufReader, str::FromStr};

use anyhow::{bail, Context, Result};
use nostr::{secp256k1::XOnlyPublicKey, FromBech32, Tag, ToBech32};
use serde::{Deserialize, Serialize};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::Connect,
    git::{Repo, RepoActions},
};

#[derive(Default)]
pub struct RepoRef {
    pub name: String,
    pub description: String,
    pub root_commit: String,
    pub git_server: String,
    pub relays: Vec<String>,
    pub maintainers: Vec<XOnlyPublicKey>,
    // code languages and hashtags
}

impl TryFrom<nostr::Event> for RepoRef {
    type Error = anyhow::Error;

    fn try_from(event: nostr::Event) -> Result<Self> {
        if !event.kind.as_u64().eq(&REPO_REF_KIND) {
            bail!("incorrect kind");
        }
        let mut r = Self::default();

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("name")) {
            r.name = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("description")) {
            r.description = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("git-server")) {
            r.git_server = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("d")) {
            r.root_commit = t.as_vec()[1].clone();
        }

        r.relays = event
            .tags
            .iter()
            .filter(|t| t.as_vec()[0].eq("relay"))
            .map(|t| t.as_vec()[1].clone())
            .collect();

        for tag in event.tags.iter().filter(|t| t.as_vec()[0].eq("p")) {
            let pk = tag.as_vec()[1].clone();
            r.maintainers.push(
                nostr_sdk::prelude::XOnlyPublicKey::from_str(&pk)
                    .context(format!("cannot convert {pk} into a valid nostr public key"))
                    .context("invalid repository event")?,
            );
        }

        Ok(r)
    }
}
static REPO_REF_KIND: u64 = 30_317;

impl RepoRef {
    pub fn to_event(&self, keys: &nostr::Keys) -> Result<nostr::Event> {
        nostr_sdk::EventBuilder::new(
            nostr::event::Kind::Custom(REPO_REF_KIND),
            "",
            &[
                vec![
                    Tag::Identifier(self.root_commit.to_string()),
                    Tag::Reference(format!("r-{}", self.root_commit)),
                    Tag::Name(self.name.clone()),
                    Tag::Description(self.description.clone()),
                    Tag::Generic(
                        nostr::TagKind::Custom("git-server".to_string()),
                        vec![self.git_server.clone()],
                    ),
                    Tag::Reference(self.git_server.clone()),
                ],
                self.relays.iter().map(|r| Tag::Relay(r.into())).collect(),
                self.maintainers
                    .iter()
                    .map(|pk| Tag::PubKey(*pk, None))
                    .collect(),
                // code languages and hashtags
            ]
            .concat(),
        )
        .to_event(keys)
        .context("failed to create repository reference event")
    }
}

pub async fn fetch(
    git_repo: &Repo,
    root_commit: String,
    #[cfg(test)] client: &MockConnect,
    #[cfg(not(test))] client: &Client,
    // TODO: more rubust way of finding repo events
    fallback_relays: Vec<String>,
) -> Result<RepoRef> {
    let repo_config = get_repo_config_from_yaml(git_repo);

    // TODO: check events only from maintainers. get relay list of maintainters.
    // check those relays.

    let mut repo_event_filter = nostr::Filter::default()
        .kind(nostr::Kind::Custom(REPO_REF_KIND))
        .identifier(root_commit);

    let mut relays = fallback_relays;
    if let Ok(repo_config) = repo_config {
        repo_event_filter =
            repo_event_filter.pubkeys(extract_pks(repo_config.maintainers.clone())?);
        relays = repo_config.relays.clone();
    }

    let events: Vec<nostr::Event> = client.get_events(relays, vec![repo_event_filter]).await?;

    RepoRef::try_from(
        events
            .iter()
            .filter(|e| e.kind.as_u64() == REPO_REF_KIND)
            .max_by_key(|e| e.created_at)
            .context("cannot find repository reference event")?
            .clone(),
    )
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct RepoConfigYaml {
    pub maintainers: Vec<String>,
    pub relays: Vec<String>,
}

pub fn get_repo_config_from_yaml(git_repo: &Repo) -> Result<RepoConfigYaml> {
    let path = git_repo.get_path()?.join("maintainers.yaml");
    let file = File::open(path)
        .context("should open maintainers.yaml if it exists")
        .context("maintainers.yaml doesnt exist")?;
    let reader = BufReader::new(file);
    let repo_config_yaml: RepoConfigYaml = serde_yaml::from_reader(reader)
        .context("should read maintainers.yaml with serde_yaml")
        .context("maintainers.yaml incorrectly formatted")?;
    Ok(repo_config_yaml)
}

pub fn extract_pks(pk_strings: Vec<String>) -> Result<Vec<XOnlyPublicKey>> {
    let mut pks: Vec<XOnlyPublicKey> = vec![];
    for s in pk_strings {
        pks.push(
            nostr_sdk::prelude::XOnlyPublicKey::from_bech32(s.clone())
                .context(format!("cannot convert {s} into a valid nostr public key"))?,
        );
    }
    Ok(pks)
}

pub fn save_repo_config_to_yaml(
    git_repo: &Repo,
    maintainers: Vec<XOnlyPublicKey>,
    relays: Vec<String>,
) -> Result<()> {
    let path = git_repo.get_path()?.join("maintainers.yaml");
    let file = if path.exists() {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .context("cannot open maintainers.yaml file with write and truncate options")?
    } else {
        std::fs::File::create(path).context("cannot create maintainers.yaml file")?
    };
    let mut maintainers_npubs = vec![];
    for m in maintainers {
        maintainers_npubs.push(
            m.to_bech32()
                .context("cannot convert public key into npub")?,
        );
    }
    serde_yaml::to_writer(
        file,
        &RepoConfigYaml {
            maintainers: maintainers_npubs,
            relays,
        },
    )
    .context("cannot write maintainers to maintainers.yaml file serde_yaml")
}

#[cfg(test)]
mod tests {
    use test_utils::*;

    use super::*;

    fn create() -> nostr::Event {
        RepoRef {
            name: "test name".to_string(),
            description: "test description".to_string(),
            root_commit: "23471389461".to_string(),
            git_server: "https://localhost:1000".to_string(),
            relays: vec!["ws://relay1.io".to_string(), "ws://relay2.io".to_string()],
            maintainers: vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
        }
        .to_event(&TEST_KEY_1_KEYS)
        .unwrap()
    }
    mod try_from {
        use super::*;

        #[test]
        fn name() {
            assert_eq!(RepoRef::try_from(create()).unwrap().name, "test name",)
        }

        #[test]
        fn description() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().description,
                "test description",
            )
        }

        #[test]
        fn root_commit() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().root_commit,
                "23471389461",
            )
        }

        #[test]
        fn git_server() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().git_server,
                "https://localhost:1000",
            )
        }

        #[test]
        fn relays() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().relays,
                vec!["ws://relay1.io".to_string(), "ws://relay2.io".to_string()],
            )
        }

        #[test]
        fn maintainers() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().maintainers,
                vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
            )
        }
    }

    mod to_event {
        use super::*;
        mod tags {
            use super::*;

            #[test]
            fn name() {
                assert!(
                    create()
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("name") && t.as_vec()[1].eq("test name"))
                )
            }
            #[test]
            fn description() {
                assert!(create().tags.iter().any(
                    |t| t.as_vec()[0].eq("description") && t.as_vec()[1].eq("test description")
                ))
            }

            #[test]
            fn root_commit_as_d_replaceable_event_identifier() {
                assert!(
                    create()
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("d") && t.as_vec()[1].eq("23471389461"))
                )
            }

            #[test]
            fn git_server() {
                assert!(create().tags.iter().any(|t| t.as_vec()[0].eq("git-server")
                    && t.as_vec()[1].eq("https://localhost:1000")))
            }

            #[test]
            fn git_server_as_reference() {
                assert!(
                    create().tags.iter().any(
                        |t| t.as_vec()[0].eq("r") && t.as_vec()[1].eq("https://localhost:1000")
                    )
                )
            }

            #[test]
            fn root_commit_as_reference() {
                assert!(
                    create()
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("r") && t.as_vec()[1].eq("r-23471389461"))
                )
            }

            #[test]
            fn relays() {
                let event = create();
                let relay_tags = event
                    .tags
                    .iter()
                    .filter(|t| t.as_vec()[0].eq("relay"))
                    .collect::<Vec<&nostr::Tag>>();
                assert_eq!(relay_tags[0].as_vec().len(), 2);
                assert_eq!(relay_tags[0].as_vec()[1], "ws://relay1.io");
                assert_eq!(relay_tags[1].as_vec()[1], "ws://relay2.io");
            }

            #[test]
            fn maintainers() {
                let event = create();
                let p_tags = event
                    .tags
                    .iter()
                    .filter(|t| t.as_vec()[0].eq("p"))
                    .collect::<Vec<&nostr::Tag>>();
                assert_eq!(p_tags[0].as_vec().len(), 2);
                assert_eq!(
                    p_tags[0].as_vec()[1],
                    TEST_KEY_1_KEYS.public_key().to_string()
                );
                assert_eq!(
                    p_tags[1].as_vec()[1],
                    TEST_KEY_2_KEYS.public_key().to_string()
                );
            }

            #[test]
            fn no_other_tags() {
                assert_eq!(create().tags.len(), 10)
            }
        }
    }
}
