use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    str::FromStr,
};

use anyhow::{bail, Context, Result};
use console::Style;
use nostr::{nips::nip01::Coordinate, FromBech32, PublicKey, Tag, TagStandard, ToBech32};
use nostr_sdk::{Kind, NostrSigner, Timestamp};
use serde::{Deserialize, Serialize};

#[cfg(not(test))]
use crate::client::Client;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::{get_event_from_global_cache, get_events_from_cache, sign_event, Connect},
    git::{nostr_url::NostrUrlDecoded, Repo, RepoActions},
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
    pub events: HashMap<Coordinate, nostr::Event>,
    // code languages and hashtags
}

impl TryFrom<nostr::Event> for RepoRef {
    type Error = anyhow::Error;

    fn try_from(event: nostr::Event) -> Result<Self> {
        if !event.kind.eq(&Kind::GitRepoAnnouncement) {
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
            r.git_server = t.clone().to_vec();
            r.git_server.remove(0);
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("web")) {
            r.web = t.clone().to_vec();
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
            r.relays = t.clone().to_vec();
            r.relays.remove(0);
        }

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("maintainers")) {
            let mut maintainers = t.clone().to_vec();
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
        } else {
            r.maintainers = vec![event.pubkey];
        }
        r.events = HashMap::new();
        r.events.insert(
            Coordinate {
                kind: event.kind,
                identifier: event.identifier().unwrap().to_string(),
                public_key: event.author(),
                relays: vec![],
            },
            event,
        );
        Ok(r)
    }
}

impl RepoRef {
    pub async fn to_event(&self, signer: &NostrSigner) -> Result<nostr::Event> {
        sign_event(
            nostr_sdk::EventBuilder::new(
                nostr::event::Kind::GitRepoAnnouncement,
                "",
                [
                    vec![
                        Tag::identifier(if self.identifier.to_string().is_empty() {
                            // fiatjaf thought a random string. its not in the draft nip.
                            // thread_rng()
                            //     .sample_iter(&Alphanumeric)
                            //     .take(15)
                            //     .map(char::from)
                            //     .collect()

                            // an identifier based on first commit is better so that users dont
                            // accidentally create two seperate identifiers for the same repo
                            // there is a hesitancy to use the commit id
                            // in another conversaion with fiatjaf he suggested the first 6
                            // character of the commit id
                            // here we are using 7 which is the standard for shorthand commit id
                            self.root_commit.to_string()[..7].to_string()
                        } else {
                            self.identifier.to_string()
                        }),
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("r")),
                            vec![self.root_commit.to_string(), "euc".to_string()],
                        ),
                        Tag::from_standardized(TagStandard::Name(self.name.clone())),
                        Tag::from_standardized(TagStandard::Description(self.description.clone())),
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("clone")),
                            self.git_server.clone(),
                        ),
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("web")),
                            self.web.clone(),
                        ),
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("relays")),
                            self.relays.clone(),
                        ),
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("maintainers")),
                            self.maintainers
                                .iter()
                                .map(std::string::ToString::to_string)
                                .collect::<Vec<String>>(),
                        ),
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                            vec![format!("git repository: {}", self.name.clone())],
                        ),
                    ],
                    // code languages and hashtags
                ]
                .concat(),
            ),
            signer,
        )
        .await
        .context("failed to create repository reference event")
    }
    /// coordinates without relay hints
    pub fn coordinates(&self) -> HashSet<Coordinate> {
        let mut res = HashSet::new();
        for m in &self.maintainers {
            res.insert(Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: *m,
                identifier: self.identifier.clone(),
                relays: vec![],
            });
        }
        res
    }

    /// coordinates without relay hints
    pub fn coordinate_with_hint(&self) -> Coordinate {
        Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: *self
                .maintainers
                .first()
                .context("no maintainers in repo ref")
                .unwrap(),
            identifier: self.identifier.clone(),
            relays: if let Some(relay) = self.relays.first() {
                vec![relay.to_string()]
            } else {
                vec![]
            },
        }
    }

    /// coordinates without relay hints
    pub fn coordinates_with_timestamps(&self) -> Vec<(Coordinate, Option<Timestamp>)> {
        self.coordinates()
            .iter()
            .map(|c| (c.clone(), self.events.get(c).map(|e| e.created_at)))
            .collect::<Vec<(Coordinate, Option<Timestamp>)>>()
    }
}

pub async fn get_repo_coordinates(
    git_repo: &Repo,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Result<HashSet<Coordinate>> {
    try_and_get_repo_coordinates(git_repo, client, true).await
}

pub async fn try_and_get_repo_coordinates(
    git_repo: &Repo,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    prompt_user: bool,
) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinates = get_repo_coordinates_from_git_config(git_repo)?;

    if repo_coordinates.is_empty() {
        repo_coordinates = get_repo_coordinates_from_nostr_remotes(git_repo)?;
    }

    if repo_coordinates.is_empty() {
        repo_coordinates = get_repo_coordinates_from_maintainers_yaml(git_repo, client).await?;
    }

    if repo_coordinates.is_empty() {
        if prompt_user {
            repo_coordinates = get_repo_coordinates_from_user_prompt(git_repo)?;
        } else {
            bail!("couldn't find repo coordinates in git config nostr.repo or in maintainers.yaml");
        }
    }
    Ok(repo_coordinates)
}

fn get_repo_coordinates_from_git_config(git_repo: &Repo) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinates = HashSet::new();
    if let Some(repo_override) = git_repo.get_git_config_item("nostr.repo", Some(false))? {
        for s in repo_override.split(',') {
            if let Ok(c) = Coordinate::parse(s) {
                repo_coordinates.insert(c);
            }
        }
    }
    Ok(repo_coordinates)
}

fn get_repo_coordinates_from_nostr_remotes(git_repo: &Repo) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinates = HashSet::new();
    for remote_name in git_repo.git_repo.remotes()?.iter().flatten() {
        if let Some(remote_url) = git_repo.git_repo.find_remote(remote_name)?.url() {
            if let Ok(nostr_url_decoded) = NostrUrlDecoded::from_str(remote_url) {
                for c in nostr_url_decoded.coordinates {
                    repo_coordinates.insert(c);
                }
            }
        }
    }
    Ok(repo_coordinates)
}

async fn get_repo_coordinates_from_maintainers_yaml(
    git_repo: &Repo,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinates = HashSet::new();
    if let Ok(repo_config) = get_repo_config_from_yaml(git_repo) {
        let maintainers = {
            let mut maintainers = HashSet::new();
            for m in &repo_config.maintainers {
                if let Ok(maintainer) = PublicKey::parse(m) {
                    maintainers.insert(maintainer);
                }
            }
            maintainers
        };
        if let Some(identifier) = repo_config.identifier {
            for public_key in maintainers {
                repo_coordinates.insert(Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key,
                    identifier: identifier.clone(),
                    relays: vec![],
                });
            }
        } else {
            // if repo_config.identifier.is_empty() {
            // this will only apply for a few repositories created before ngit v1.3
            // that haven't updated their maintainers.yaml
            if let Ok(Some(current_user_npub)) = git_repo.get_git_config_item("nostr.npub", None) {
                if let Ok(current_user) = PublicKey::parse(current_user_npub) {
                    for m in &repo_config.maintainers {
                        if let Ok(maintainer) = PublicKey::parse(m) {
                            if current_user.eq(&maintainer) {
                                println!(
                                    "please run `ngit init` to add the repo identifier to maintainers.yaml"
                                );
                            }
                        }
                    }
                }
            }
            // look find all repo refs with root_commit. for identifier
            let filter = nostr::Filter::default()
                .kind(nostr::Kind::GitRepoAnnouncement)
                .reference(git_repo.get_root_commit()?.to_string())
                .authors(maintainers.clone());
            let mut events =
                get_events_from_cache(git_repo.get_path()?, vec![filter.clone()]).await?;
            if events.is_empty() {
                events =
                    get_event_from_global_cache(git_repo.get_path()?, vec![filter.clone()]).await?;
            }
            if events.is_empty() {
                println!(
                    "finding repository events for this repository for npubs in maintainers.yaml"
                );
                events = client
                    .get_events(client.get_fallback_relays().clone(), vec![filter.clone()])
                    .await?;
            }
            if let Some(e) = events.first() {
                if let Some(identifier) = e.identifier() {
                    for m in &repo_config.maintainers {
                        if let Ok(maintainer) = PublicKey::parse(m) {
                            repo_coordinates.insert(Coordinate {
                                kind: Kind::GitRepoAnnouncement,
                                public_key: maintainer,
                                identifier: identifier.to_string(),
                                relays: vec![],
                            });
                        }
                    }
                }
            } else {
                let c = ask_for_naddr()?;
                git_repo.save_git_config_item("nostr.repo", &c.to_bech32()?, false)?;
                repo_coordinates.insert(c);
            }
        }
    }
    Ok(repo_coordinates)
}

fn get_repo_coordinates_from_user_prompt(git_repo: &Repo) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinates = HashSet::new();
    // TODO: present list of events filter by root_commit
    // TODO: fallback to search based on identifier
    let c = ask_for_naddr()?;
    // PROBLEM: we are saving this before checking whether it actually exists, which
    // means next time the user won't be prompted and may not know how to
    // change the selected repo
    git_repo.save_git_config_item("nostr.repo", &c.to_bech32()?, false)?;
    repo_coordinates.insert(c);
    Ok(repo_coordinates)
}

fn ask_for_naddr() -> Result<Coordinate> {
    let dim = Style::new().color256(247);
    println!(
        "{}",
        dim.apply_to("hint: https://gitworkshop.dev/repos lists repositories and their naddr"),
    );

    Ok(loop {
        if let Ok(c) = Coordinate::parse(
            Interactor::default()
                .input(PromptInputParms::default().with_prompt("repository naddr"))?,
        ) {
            break c;
        }
        println!("not a valid naddr");
    })
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct RepoConfigYaml {
    pub identifier: Option<String>,
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
    identifier: String,
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
            identifier: Some(identifier),
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

    async fn create() -> nostr::Event {
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
            events: HashMap::new(),
        }
        .to_event(&TEST_KEY_1_SIGNER)
        .await
        .unwrap()
    }
    mod try_from {
        use super::*;

        #[tokio::test]
        async fn identifier() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().identifier,
                "123412341",
            )
        }

        #[tokio::test]
        async fn name() {
            assert_eq!(RepoRef::try_from(create().await).unwrap().name, "test name",)
        }

        #[tokio::test]
        async fn description() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().description,
                "test description",
            )
        }

        #[tokio::test]
        async fn root_commit_is_r_tag() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().root_commit,
                "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2",
            )
        }

        mod root_commit_is_empty_if_no_r_tag_which_is_sha1_format {
            use nostr::JsonUtil;

            use super::*;
            async fn create_with_incorrect_first_commit_ref(s: &str) -> nostr::Event {
                nostr::Event::from_json(
                    create()
                        .await
                        .as_json()
                        .replace("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2", s),
                )
                .unwrap()
            }

            #[tokio::test]
            async fn less_than_40_characters() {
                let s = "5e664e5a7845cd1373";
                assert_eq!(
                    RepoRef::try_from(create_with_incorrect_first_commit_ref(s).await)
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[tokio::test]
            async fn more_than_40_characters() {
                let s = "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2111111111";
                assert_eq!(
                    RepoRef::try_from(create_with_incorrect_first_commit_ref(s).await)
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[tokio::test]
            async fn not_hex_characters() {
                let s = "xxx64e5a7845cd1373c79f580ca4fe29ab5b34d2";
                assert_eq!(
                    RepoRef::try_from(create_with_incorrect_first_commit_ref(s).await)
                        .unwrap()
                        .root_commit,
                    "",
                )
            }
        }

        #[tokio::test]
        async fn git_server() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().git_server,
                vec!["https://localhost:1000"],
            )
        }

        #[tokio::test]
        async fn web() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().web,
                vec![
                    "https://exampleproject.xyz".to_string(),
                    "https://gitworkshop.dev/123".to_string()
                ],
            )
        }

        #[tokio::test]
        async fn relays() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().relays,
                vec!["ws://relay1.io".to_string(), "ws://relay2.io".to_string()],
            )
        }

        #[tokio::test]
        async fn maintainers() {
            assert_eq!(
                RepoRef::try_from(create().await).unwrap().maintainers,
                vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
            )
        }
    }

    mod to_event {
        use super::*;
        mod tags {
            use super::*;

            #[tokio::test]
            async fn identifier() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("d") && t.as_vec()[1].eq("123412341"))
                )
            }

            #[tokio::test]
            async fn name() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_vec()[0].eq("name") && t.as_vec()[1].eq("test name"))
                )
            }

            #[tokio::test]
            async fn alt() {
                assert!(
                    create().await.tags.iter().any(|t| t.as_vec()[0].eq("alt")
                        && t.as_vec()[1].eq("git repository: test name"))
                )
            }

            #[tokio::test]
            async fn description() {
                assert!(create().await.tags.iter().any(
                    |t| t.as_vec()[0].eq("description") && t.as_vec()[1].eq("test description")
                ))
            }

            #[tokio::test]
            async fn root_commit_as_reference() {
                assert!(create().await.tags.iter().any(|t| t.as_vec()[0].eq("r")
                    && t.as_vec()[1].eq("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2")))
            }

            #[tokio::test]
            async fn git_server() {
                assert!(create().await.tags.iter().any(
                    |t| t.as_vec()[0].eq("clone") && t.as_vec()[1].eq("https://localhost:1000")
                ))
            }

            #[tokio::test]
            async fn relays() {
                let event = create().await;
                let relays_tag: &nostr::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_vec()[0].eq("relays"))
                    .unwrap();
                assert_eq!(relays_tag.as_vec().len(), 3);
                assert_eq!(relays_tag.as_vec()[1], "ws://relay1.io");
                assert_eq!(relays_tag.as_vec()[2], "ws://relay2.io");
            }

            #[tokio::test]
            async fn web() {
                let event = create().await;
                let web_tag: &nostr::Tag =
                    event.tags.iter().find(|t| t.as_vec()[0].eq("web")).unwrap();
                assert_eq!(web_tag.as_vec().len(), 3);
                assert_eq!(web_tag.as_vec()[1], "https://exampleproject.xyz");
                assert_eq!(web_tag.as_vec()[2], "https://gitworkshop.dev/123");
            }

            #[tokio::test]
            async fn maintainers() {
                let event = create().await;
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

            #[tokio::test]
            async fn no_other_tags() {
                assert_eq!(create().await.tags.len(), 9)
            }
        }
    }
}
