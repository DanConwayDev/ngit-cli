use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use console::Style;
use nostr::{
    FromBech32, PublicKey, Tag, TagStandard, ToBech32,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};
use nostr_sdk::{Kind, NostrSigner, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize};

#[cfg(not(test))]
use crate::client::Client;
use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms, PromptInputParms,
    },
    client::{Connect, consolidate_fetch_reports, get_repo_ref_from_cache, sign_event},
    git::{
        Repo, RepoActions,
        nostr_url::{NostrUrlDecoded, use_nip05_git_config_cache_to_find_nip05_from_public_key},
    },
    login::user::get_user_details,
};

#[derive(Clone)]
pub struct RepoRef {
    pub name: String,
    pub description: String,
    pub identifier: String,
    pub root_commit: String,
    pub git_server: Vec<String>,
    pub web: Vec<String>,
    pub relays: Vec<RelayUrl>,
    pub maintainers: Vec<PublicKey>,
    pub trusted_maintainer: PublicKey,
    pub events: HashMap<Nip19Coordinate, nostr::Event>,
    pub nostr_git_url: Option<NostrUrlDecoded>,
}

impl TryFrom<(nostr::Event, Option<PublicKey>)> for RepoRef {
    type Error = anyhow::Error;

    fn try_from((event, trusted_maintainer): (nostr::Event, Option<PublicKey>)) -> Result<Self> {
        // TODO: turn trusted maintainer into NostrUrlDecoded
        if !event.kind.eq(&Kind::GitRepoAnnouncement) {
            bail!("incorrect kind");
        }

        let mut r = Self {
            name: String::new(),
            description: String::new(),
            identifier: String::new(),
            root_commit: String::new(),
            git_server: Vec::new(),
            web: Vec::new(),
            relays: Vec::new(),
            maintainers: Vec::new(),
            trusted_maintainer: trusted_maintainer.unwrap_or(event.pubkey),
            events: HashMap::new(),
            nostr_git_url: None,
        };

        for tag in event.tags.iter() {
            match tag.as_slice() {
                [t, id, ..] if t == "d" => r.identifier = id.clone(),
                [t, name, ..] if t == "name" => r.name = name.clone(),
                [t, description, ..] if t == "description" => r.description = description.clone(),
                [t, clone @ ..] if t == "clone" => {
                    r.git_server = clone.to_vec();
                }
                [t, web @ ..] if t == "web" => {
                    r.web = web.to_vec();
                }
                [t, commit_id]
                    if t == "r"
                        && commit_id.len() == 40
                        && git2::Oid::from_str(commit_id).is_ok() =>
                {
                    r.root_commit = commit_id.clone();
                }
                [t, commit_id, marker]
                    if t == "r"
                        && marker == "euc"
                        && commit_id.len() == 40
                        && git2::Oid::from_str(commit_id).is_ok() =>
                {
                    r.root_commit = commit_id.clone();
                }
                [t, relays @ ..] if t == "relays" => {
                    for relay in relays {
                        if let Ok(relay_url) = RelayUrl::parse(relay) {
                            r.relays.push(relay_url);
                        }
                    }
                }
                [t, maintainers @ ..] if t == "maintainers" => {
                    if !maintainers.contains(&event.pubkey.to_string()) {
                        r.maintainers.push(event.pubkey);
                    }
                    for pk in maintainers {
                        r.maintainers.push(
                            nostr_sdk::prelude::PublicKey::from_str(pk)
                                .context(format!("failed to convert entry from maintainers tag {pk} into a valid nostr public key. it should be in hex format"))
                                .context("invalid repository event")?,
                        );
                    }
                }
                _ => {}
            }
        }

        // If no maintainers were added, add the event's public key
        if r.maintainers.is_empty() {
            r.maintainers.push(event.pubkey);
        }
        r.events = HashMap::new();
        r.events.insert(
            Nip19Coordinate {
                coordinate: Coordinate {
                    kind: event.kind,
                    identifier: event.tags.identifier().unwrap().to_string(),
                    public_key: event.pubkey,
                },
                relays: vec![],
            },
            event,
        );
        Ok(r)
    }
}

impl RepoRef {
    pub async fn to_event(&self, signer: &Arc<dyn NostrSigner>) -> Result<nostr::Event> {
        sign_event(
            nostr_sdk::EventBuilder::new(nostr::event::Kind::GitRepoAnnouncement, "").tags(
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
                            self.relays.iter().map(|r| r.to_string()),
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
            "repo announcement".to_string(),
        )
        .await
        .context("failed to create repository reference event")
    }
    /// coordinates without relay hints
    pub fn coordinates(&self) -> HashSet<Nip19Coordinate> {
        let mut res = HashSet::new();
        res.insert(Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: self.trusted_maintainer,
                identifier: self.identifier.clone(),
            },
            relays: vec![],
        });

        for m in &self.maintainers {
            res.insert(Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: *m,
                    identifier: self.identifier.clone(),
                },
                relays: vec![],
            });
        }
        res
    }

    /// coordinates without relay hints
    pub fn coordinate_with_hint(&self) -> Nip19Coordinate {
        Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: self.trusted_maintainer,
                identifier: self.identifier.clone(),
            },
            relays: if let Some(relay) = self.relays.first() {
                vec![relay.clone()]
            } else {
                vec![]
            },
        }
    }

    /// coordinates without relay hints
    pub fn coordinates_with_timestamps(&self) -> Vec<(Nip19Coordinate, Option<Timestamp>)> {
        self.coordinates()
            .iter()
            .map(|c| (c.clone(), self.events.get(c).map(|e| e.created_at)))
            .collect::<Vec<(Nip19Coordinate, Option<Timestamp>)>>()
    }

    pub fn set_nostr_git_url(&mut self, nostr_git_url: NostrUrlDecoded) {
        self.nostr_git_url = Some(nostr_git_url)
    }

    pub fn to_nostr_git_url(&self, git_repo: &Option<&Repo>) -> NostrUrlDecoded {
        if let Some(nostr_git_url) = &self.nostr_git_url {
            return nostr_git_url.clone();
        }
        let c = self.coordinate_with_hint();
        NostrUrlDecoded {
            original_string: String::new(),
            nip05: use_nip05_git_config_cache_to_find_nip05_from_public_key(
                &c.public_key,
                git_repo,
            )
            .unwrap_or_default(),
            coordinate: c,
            protocol: None,
            user: None,
        }
    }
}

pub async fn get_repo_coordinates_when_remote_unknown(
    git_repo: &Repo,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Result<Nip19Coordinate> {
    if let Ok(c) = try_and_get_repo_coordinates_when_remote_unknown(git_repo).await {
        Ok(c)
    } else {
        get_repo_coordinate_from_user_prompt(git_repo, client).await
    }
}

pub async fn try_and_get_repo_coordinates_when_remote_unknown(
    git_repo: &Repo,
) -> Result<Nip19Coordinate> {
    let remote_coordinates = get_repo_coordinates_from_nostr_remotes(git_repo).await?;
    if remote_coordinates.is_empty() {
        if let Ok(c) = get_repo_coordinates_from_git_config(git_repo) {
            Ok(c)
        } else {
            get_repo_coordinates_from_maintainers_yaml(git_repo)
                .await
                // not mentioning maintainers.yaml as its not auto generated anymore
                .context("no nostr git remotes or git config \"nostr.repo\" value")
        }
    } else if remote_coordinates.len() == 1
        || remote_coordinates.values().all(|coordinate| {
            let first = remote_coordinates.values().next().unwrap();
            coordinate.public_key == first.public_key && coordinate.identifier == first.identifier
        })
    {
        Ok(remote_coordinates.values().next().unwrap().clone())
    } else {
        let choice_index = Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt("select nostr repository from those listed as git remotes")
                .with_default(0)
                .with_choices(
                    get_nostr_git_remote_selection_labels(git_repo, &remote_coordinates).await?,
                ),
        )?;

        Ok(remote_coordinates
            .get(
                remote_coordinates
                    .keys()
                    .cloned()
                    .collect::<Vec<String>>()
                    .get(choice_index)
                    .unwrap(),
            )
            .unwrap()
            .clone())
    }
}

async fn get_nostr_git_remote_selection_labels(
    git_repo: &Repo,
    remote_coordinates: &HashMap<String, Nip19Coordinate>,
) -> Result<Vec<String>> {
    let mut res = vec![];
    for (remote, c) in remote_coordinates {
        res.push(format!(
            "{remote} - {}/{}",
            get_user_details(&c.public_key, None, Some(git_repo.get_path()?), true, false)
                .await?
                .metadata
                .name,
            c.identifier
        ));
    }
    Ok(res)
}

fn get_repo_coordinates_from_git_config(git_repo: &Repo) -> Result<Nip19Coordinate> {
    Nip19Coordinate::from_bech32(
        &git_repo
            .get_git_config_item("nostr.repo", Some(false))?
            .context("git config item \"nostr.repo\" is not set in local repository")?,
    )
    .context("git config item \"nostr.repo\" is not an naddr")
}

async fn get_repo_coordinates_from_nostr_remotes(
    git_repo: &Repo,
) -> Result<HashMap<String, Nip19Coordinate>> {
    let mut repo_coordinates = HashMap::new();
    for remote_name in git_repo.git_repo.remotes()?.iter().flatten() {
        if let Some(remote_url) = git_repo.git_repo.find_remote(remote_name)?.url() {
            if let Ok(nostr_url_decoded) =
                NostrUrlDecoded::parse_and_resolve(remote_url, &Some(git_repo)).await
            {
                repo_coordinates.insert(remote_name.to_string(), nostr_url_decoded.coordinate);
            }
        }
    }
    Ok(repo_coordinates)
}

async fn get_repo_coordinates_from_maintainers_yaml(git_repo: &Repo) -> Result<Nip19Coordinate> {
    let repo_config = get_repo_config_from_yaml(git_repo)?;

    Ok(Nip19Coordinate {
        coordinate: Coordinate {
            identifier: repo_config
                .identifier
                .context("maintainers.yaml doesnt list the identifier")?,
            kind: Kind::GitRepoAnnouncement,
            public_key: PublicKey::from_bech32(
                repo_config
                    .maintainers
                    .first()
                    .context("maintainers.yaml doesnt list any maintainers")?,
            )
            .context("maintainers.yaml doesn't list the first maintainer using a valid npub")?,
        },
        relays: repo_config
            .relays
            .iter()
            .filter_map(|url| RelayUrl::parse(url).ok())
            .collect(),
    })
}

async fn get_repo_coordinate_from_user_prompt(
    git_repo: &Repo,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Result<Nip19Coordinate> {
    // TODO: present list of events filter by root_commit
    // TODO: fallback to search based on identifier
    let dim = Style::new().color256(247);
    println!(
        "{}",
        dim.apply_to(
            "hint: https://gitworkshop.dev/repos lists repositories and their nostr address"
        ),
    );
    let git_repo_path = git_repo.get_path()?;
    let coordinate = {
        loop {
            let input = Interactor::default()
                .input(PromptInputParms::default().with_prompt("nostr repository"))?;
            let coordinate = if let Ok(c) = Nip19Coordinate::from_bech32(&input) {
                c
            } else if let Ok(nostr_url) =
                NostrUrlDecoded::parse_and_resolve(&input, &Some(git_repo)).await
            {
                nostr_url.coordinate
            } else {
                eprintln!("not a valid naddr or git nostr remote URL starting nostr://");
                continue;
            };
            let term = console::Term::stderr();
            term.write_line("searching for repository...")?;
            let (relay_reports, progress_reporter) = client
                .fetch_all(
                    Some(git_repo_path),
                    Some(&coordinate),
                    &HashSet::from_iter(vec![coordinate.public_key]),
                )
                .await?;
            let relay_errs = relay_reports.iter().any(std::result::Result::is_err);
            let report = consolidate_fetch_reports(relay_reports);
            if !relay_errs && !report.to_string().is_empty() {
                let _ = progress_reporter.clear();
            }
            if report.to_string().is_empty() {
                eprintln!("couldn't find repository");
                continue;
            } else {
                eprintln!("repository found");
                break coordinate;
            }
        }
    };
    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &coordinate).await?;

    if Interactor::default().confirm(
        PromptConfirmParms::default()
            .with_default(true)
            .with_prompt("set git remote \"origin\" to nostr repository url?"),
    )? {
        set_or_create_git_remote_with_nostr_url("origin", &repo_ref, git_repo)?;
    } else if Interactor::default().confirm(
        PromptConfirmParms::default()
            .with_default(true)
            .with_prompt("set up new git remote for the nostr repository?"),
    )? {
        let name =
            Interactor::default().input(PromptInputParms::default().with_prompt("remote name"))?;
        set_or_create_git_remote_with_nostr_url(&name, &repo_ref, git_repo)?;
    }
    git_repo.save_git_config_item("nostr.repo", &coordinate.to_bech32()?, false)?;
    Ok(coordinate)
}

fn set_or_create_git_remote_with_nostr_url(
    name: &str,
    repo_ref: &RepoRef,
    git_repo: &Repo,
) -> Result<()> {
    let url = repo_ref.to_nostr_git_url(&Some(git_repo)).to_string();
    if git_repo.git_repo.remote_set_url(name, &url).is_err() {
        git_repo.git_repo.remote(name, &url)?;
    }
    eprintln!("set git remote \"{name}\" to {url}");
    Ok(())
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
            nostr_sdk::prelude::PublicKey::from_bech32(&s).context(format!(
                "failed to convert {s} into a valid nostr public key"
            ))?,
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
            .context("failed to open maintainers.yaml file with write and truncate options")?
    } else {
        std::fs::File::create(path).context("failed to create maintainers.yaml file")?
    };
    let mut maintainers_npubs = vec![];
    for m in maintainers {
        maintainers_npubs.push(
            m.to_bech32()
                .context("failed to convert public key into npub")?,
        );
    }
    serde_yaml::to_writer(file, &RepoConfigYaml {
        identifier: Some(identifier),
        maintainers: maintainers_npubs,
        relays,
    })
    .context("failed to write maintainers to maintainers.yaml file serde_yaml")
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
            relays: vec![
                RelayUrl::parse("ws://relay1.io").unwrap(),
                RelayUrl::parse("ws://relay2.io").unwrap(),
            ],
            trusted_maintainer: TEST_KEY_1_KEYS.public_key(),
            maintainers: vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
            events: HashMap::new(),
            nostr_git_url: None,
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
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .identifier,
                "123412341",
            )
        }

        #[tokio::test]
        async fn name() {
            assert_eq!(
                RepoRef::try_from((create().await, None)).unwrap().name,
                "test name",
            )
        }

        #[tokio::test]
        async fn description() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .description,
                "test description",
            )
        }

        #[tokio::test]
        async fn root_commit_is_r_tag() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .root_commit,
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
                    RepoRef::try_from((create_with_incorrect_first_commit_ref(s).await, None))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[tokio::test]
            async fn more_than_40_characters() {
                let s = "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2111111111";
                assert_eq!(
                    RepoRef::try_from((create_with_incorrect_first_commit_ref(s).await, None))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[tokio::test]
            async fn not_hex_characters() {
                let s = "xxx64e5a7845cd1373c79f580ca4fe29ab5b34d2";
                assert_eq!(
                    RepoRef::try_from((create_with_incorrect_first_commit_ref(s).await, None))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }
        }

        #[tokio::test]
        async fn git_server() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .git_server,
                vec!["https://localhost:1000"],
            )
        }

        #[tokio::test]
        async fn web() {
            assert_eq!(
                RepoRef::try_from((create().await, None)).unwrap().web,
                vec![
                    "https://exampleproject.xyz".to_string(),
                    "https://gitworkshop.dev/123".to_string()
                ],
            )
        }

        #[tokio::test]
        async fn relays() {
            assert_eq!(
                RepoRef::try_from((create().await, None)).unwrap().relays,
                vec![
                    RelayUrl::parse("ws://relay1.io").unwrap(),
                    RelayUrl::parse("ws://relay2.io").unwrap(),
                ],
            )
        }

        #[tokio::test]
        async fn maintainers() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .maintainers,
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
                        .any(|t| t.as_slice()[0].eq("d") && t.as_slice()[1].eq("123412341"))
                )
            }

            #[tokio::test]
            async fn name() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("name") && t.as_slice()[1].eq("test name"))
                )
            }

            #[tokio::test]
            async fn alt() {
                assert!(create().await.tags.iter().any(|t| t.as_slice()[0].eq("alt")
                    && t.as_slice()[1].eq("git repository: test name")))
            }

            #[tokio::test]
            async fn description() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("description")
                            && t.as_slice()[1].eq("test description"))
                )
            }

            #[tokio::test]
            async fn root_commit_as_reference() {
                assert!(create().await.tags.iter().any(|t| t.as_slice()[0].eq("r")
                    && t.as_slice()[1].eq("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2")))
            }

            #[tokio::test]
            async fn git_server() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("clone")
                            && t.as_slice()[1].eq("https://localhost:1000"))
                )
            }

            #[tokio::test]
            async fn relays() {
                let event = create().await;
                let relays_tag: &nostr::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0].eq("relays"))
                    .unwrap();
                assert_eq!(relays_tag.as_slice().len(), 3);
                assert_eq!(relays_tag.as_slice()[1], "ws://relay1.io");
                assert_eq!(relays_tag.as_slice()[2], "ws://relay2.io");
            }

            #[tokio::test]
            async fn web() {
                let event = create().await;
                let web_tag: &nostr::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0].eq("web"))
                    .unwrap();
                assert_eq!(web_tag.as_slice().len(), 3);
                assert_eq!(web_tag.as_slice()[1], "https://exampleproject.xyz");
                assert_eq!(web_tag.as_slice()[2], "https://gitworkshop.dev/123");
            }

            #[tokio::test]
            async fn maintainers() {
                let event = create().await;
                let maintainers_tag: &nostr::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0].eq("maintainers"))
                    .unwrap();
                assert_eq!(maintainers_tag.as_slice().len(), 3);
                assert_eq!(
                    maintainers_tag.as_slice()[1],
                    TEST_KEY_1_KEYS.public_key().to_string()
                );
                assert_eq!(
                    maintainers_tag.as_slice()[2],
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
