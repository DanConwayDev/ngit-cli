use std::{fs::File, io::BufReader, str::FromStr};

use anyhow::{bail, Context, Result};
use nostr::{nips::nip19::Nip19, FromBech32, PublicKey, Tag, ToBech32};
use serde::{Deserialize, Serialize};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::Connect,
    git::{Repo, RepoActions},
};

#[derive(Default)]
pub struct RepoRef {
    pub name: String,
    pub description: String,
    pub identifier: String,
    pub root_commit: String,
    pub git_server: Vec<String>,
    pub web: Vec<String>,
    pub relays: Vec<String>,
    pub maintainers: Vec<PublicKey>,
    // code languages and hashtags
}

impl TryFrom<nostr::Event> for RepoRef {
    type Error = anyhow::Error;

    fn try_from(event: nostr::Event) -> Result<Self> {
        if !event.kind.as_u64().eq(&REPO_REF_KIND) {
            bail!("incorrect kind");
        }
        let mut r = Self::default();

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("d")) {
            r.identifier = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("name")) {
            r.name = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("description")) {
            r.description = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("clone")) {
            r.git_server = t.as_vec().clone();
            r.git_server.remove(0);
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("web")) {
            r.web = t.as_vec().clone();
            r.web.remove(0);
        }

        if let Some(t) = event.tags.iter().find(|t| {
            t.as_vec()[0].eq("r")
                && t.as_vec()[1].len().eq(&40)
                && git2::Oid::from_str(t.as_vec()[1].as_str()).is_ok()
        }) {
            r.root_commit = t.as_vec()[1].clone();
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("relays")) {
            r.relays = t.as_vec().clone();
            r.relays.remove(0);
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("maintainers")) {
            let mut maintainers = t.as_vec().clone();
            maintainers.remove(0);
            if !maintainers.contains(&event.pubkey.to_string()) {
                r.maintainers.push(event.pubkey);
            }
            for pk in maintainers {
                r.maintainers.push(
                nostr_sdk::prelude::PublicKey::from_str(&pk)
                    .context(format!("cannot convert entry from maintainers tag {pk} into a valid nostr public key. it should be in hex format"))
                    .context("invalid repository event")?,
                );
            }
        }

        Ok(r)
    }
}

pub static REPO_REF_KIND: u64 = 30_617;

impl RepoRef {
    pub fn to_event(&self, keys: &nostr::Keys) -> Result<nostr::Event> {
        nostr_sdk::EventBuilder::new(
            nostr::event::Kind::Custom(REPO_REF_KIND),
            "",
            [
                vec![
                    Tag::Identifier(if self.identifier.to_string().is_empty() {
                        // fiatjaf thought a random string. its not in the draft nip.
                        // thread_rng()
                        //     .sample_iter(&Alphanumeric)
                        //     .take(15)
                        //     .map(char::from)
                        //     .collect()

                        // an identifier based on first commit is better so that users dont
                        // accidentally create two seperate identifiers for the same repo
                        // there is a hesitancy to use the commit id
                        // in another conversaion with fiatjaf he suggested the first 6 character of
                        // the commit id
                        // here we are using 7 which is the standard for shorthand commit id
                        self.root_commit.to_string()[..7].to_string()
                    } else {
                        self.identifier.to_string()
                    }),
                    Tag::Reference(self.root_commit.to_string()),
                    Tag::Name(self.name.clone()),
                    Tag::Description(self.description.clone()),
                    Tag::Generic(
                        nostr::TagKind::Custom("clone".to_string()),
                        self.git_server.clone(),
                    ),
                    Tag::Generic(nostr::TagKind::Custom("web".to_string()), self.web.clone()),
                    Tag::Generic(
                        nostr::TagKind::Custom("relays".to_string()),
                        self.relays.clone(),
                    ),
                    Tag::Generic(
                        nostr::TagKind::Custom("maintainers".to_string()),
                        self.maintainers
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect(),
                    ),
                ],
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
    prompt_for_nevent_if_cant_event: bool,
) -> Result<RepoRef> {
    let repo_config = get_repo_config_from_yaml(git_repo);

    // TODO: check events only from maintainers. get relay list of maintainters.
    // check those relays.

    let mut repo_event_filter = nostr::Filter::default()
        .kind(nostr::Kind::Custom(REPO_REF_KIND))
        .reference(root_commit);

    let mut relays = fallback_relays;
    if let Ok(repo_config) = repo_config {
        repo_event_filter =
            repo_event_filter.authors(extract_pks(repo_config.maintainers.clone())?);
        relays = repo_config.relays.clone();
    }

    let event = loop {
        let events: Vec<nostr::Event> = client
            .get_events(relays.clone(), vec![repo_event_filter.clone()])
            .await?;

        // TODO: if maintainers.yaml isn't present, as the user to select from the
        // pubkeys they want to use. could use WoT as an indicator as well as the repo
        // and user name.

        // TODO: if maintainers.yaml isn't present, save the selected repo pubkey
        // somewhere within .git folder for future use and seek to get that next time
        if let Some(event) = events
            .iter()
            .filter(|e| e.kind.as_u64() == REPO_REF_KIND)
            .max_by_key(|e| e.created_at)
        {
            break event.clone();
        }
        if !prompt_for_nevent_if_cant_event {
            bail!("cannot find repo event");
        }
        println!("cannot find repo event");
        loop {
            let bech32 = Interactor::default()
                .input(PromptInputParms::default().with_prompt("repository naddr or nevent"))?;
            if let Ok(nip19) = Nip19::from_bech32(bech32) {
                repo_event_filter =
                    nostr::Filter::default().kind(nostr::Kind::Custom(REPO_REF_KIND));
                match nip19 {
                    Nip19::Coordinate(c) => {
                        repo_event_filter = repo_event_filter
                            .identifier(c.identifier)
                            .author(c.public_key);
                        for r in c.relays {
                            relays.push(r);
                        }
                    }
                    Nip19::Event(n) => {
                        if let Some(author) = n.author {
                            repo_event_filter = repo_event_filter.id(n.event_id).author(author);
                        }
                        for r in n.relays {
                            relays.push(r);
                        }
                    }
                    Nip19::EventId(id) => repo_event_filter = repo_event_filter.id(id),
                    _ => (),
                }
            } else {
                println!("not a valid nevent or naddr");
                continue;
            }
            break;
        }
    };

    RepoRef::try_from(event.clone()).context("cannot parse event as repo reference")
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

pub fn extract_pks(pk_strings: Vec<String>) -> Result<Vec<PublicKey>> {
    let mut pks: Vec<PublicKey> = vec![];
    for s in pk_strings {
        pks.push(
            nostr_sdk::prelude::PublicKey::from_bech32(s.clone())
                .context(format!("cannot convert {s} into a valid nostr public key"))?,
        );
    }
    Ok(pks)
}

pub fn save_repo_config_to_yaml(
    git_repo: &Repo,
    maintainers: Vec<PublicKey>,
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
            identifier: "123412341".to_string(),
            name: "test name".to_string(),
            description: "test description".to_string(),
            root_commit: "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2".to_string(),
            git_server: vec!["https://localhost:1000".to_string()],
            web: vec![
                "https://exampleproject.xyz".to_string(),
                "https://gitworkshop.dev/123".to_string(),
            ],
            relays: vec!["ws://relay1.io".to_string(), "ws://relay2.io".to_string()],
            maintainers: vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
        }
        .to_event(&TEST_KEY_1_KEYS)
        .unwrap()
    }
    mod try_from {
        use super::*;

        #[test]
        fn identifier() {
            assert_eq!(RepoRef::try_from(create()).unwrap().identifier, "123412341",)
        }

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
        fn root_commit_is_r_tag() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().root_commit,
                "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2",
            )
        }

        mod root_commit_is_empty_if_no_r_tag_which_is_sha1_format {
            use nostr::JsonUtil;

            use super::*;
            fn create_with_incorrect_first_commit_ref(s: &str) -> nostr::Event {
                nostr::Event::from_json(
                    create()
                        .as_json()
                        .replace("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2", s),
                )
                .unwrap()
            }

            #[test]
            fn less_than_40_characters() {
                let s = "5e664e5a7845cd1373";
                assert_eq!(
                    RepoRef::try_from(create_with_incorrect_first_commit_ref(s))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[test]
            fn more_than_40_characters() {
                let s = "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2111111111";
                assert_eq!(
                    RepoRef::try_from(create_with_incorrect_first_commit_ref(s))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[test]
            fn not_hex_characters() {
                let s = "xxx64e5a7845cd1373c79f580ca4fe29ab5b34d2";
                assert_eq!(
                    RepoRef::try_from(create_with_incorrect_first_commit_ref(s))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }
        }

        #[test]
        fn git_server() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().git_server,
                vec!["https://localhost:1000"],
            )
        }

        #[test]
        fn web() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().web,
                vec![
                    "https://exampleproject.xyz".to_string(),
                    "https://gitworkshop.dev/123".to_string()
                ],
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
            fn identifier() {
                assert!(
                    create()
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("d") && t.as_vec()[1].eq("123412341"))
                )
            }

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
            fn root_commit_as_reference() {
                assert!(create().tags.iter().any(|t| t.as_vec()[0].eq("r")
                    && t.as_vec()[1].eq("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2")))
            }

            #[test]
            fn git_server() {
                assert!(create().tags.iter().any(
                    |t| t.as_vec()[0].eq("clone") && t.as_vec()[1].eq("https://localhost:1000")
                ))
            }

            #[test]
            fn relays() {
                let event = create();
                let relays_tag: &nostr::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("relays"))
                    .unwrap();
                assert_eq!(relays_tag.as_vec().len(), 3);
                assert_eq!(relays_tag.as_vec()[1], "ws://relay1.io");
                assert_eq!(relays_tag.as_vec()[2], "ws://relay2.io");
            }

            #[test]
            fn web() {
                let event = create();
                let web_tag: &nostr::Tag =
                    event.tags.iter().find(|t| t.as_vec()[0].eq("web")).unwrap();
                assert_eq!(web_tag.as_vec().len(), 3);
                assert_eq!(web_tag.as_vec()[1], "https://exampleproject.xyz");
                assert_eq!(web_tag.as_vec()[2], "https://gitworkshop.dev/123");
            }

            #[test]
            fn maintainers() {
                let event = create();
                let maintainers_tag: &nostr::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("maintainers"))
                    .unwrap();
                assert_eq!(maintainers_tag.as_vec().len(), 3);
                assert_eq!(
                    maintainers_tag.as_vec()[1],
                    TEST_KEY_1_KEYS.public_key().to_string()
                );
                assert_eq!(
                    maintainers_tag.as_vec()[2],
                    TEST_KEY_2_KEYS.public_key().to_string()
                );
            }

            #[test]
            fn no_other_tags() {
                assert_eq!(create().tags.len(), 8)
            }
        }
    }
}
