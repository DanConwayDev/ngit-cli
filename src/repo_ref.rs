use anyhow::{bail, Context, Result};
use nostr::Tag;

#[cfg(not(test))]
use crate::client::Client;
use crate::client::Connect;
#[cfg(test)]
use crate::client::MockConnect;

#[derive(Default)]
pub struct RepoRef {
    pub name: String,
    pub description: String,
    pub root_commit: String,
    pub relays: Vec<String>,
    // git_server: String,
    // other maintainers
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

        if let Some(t) = event.tags.iter().find(|t| t.as_vec()[0].eq("d")) {
            r.root_commit = t.as_vec()[1].clone();
        }

        r.relays = event
            .tags
            .iter()
            .filter(|t| t.as_vec()[0].eq("relay"))
            .map(|t| t.as_vec()[1].clone())
            .collect();

        Ok(r)
    }
}
static REPO_REF_KIND: u64 = 300_317;

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
                ],
                self.relays.iter().map(|r| Tag::Relay(r.into())).collect(),
                // git_servers
                // other maintainers
                // code languages and hashtags
            ]
            .concat(),
        )
        .to_event(keys)
        .context("failed to create repository reference event")
    }
}

pub async fn fetch(
    root_commit: String,
    #[cfg(test)] client: &MockConnect,
    #[cfg(not(test))] client: &Client,
    // TODO: more rubust way of finding repo events
    relays: Vec<String>,
) -> Result<RepoRef> {
    // TODO: fetch relay information from file

    let events: Vec<nostr::Event> = client
        .get_events(
            relays,
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(REPO_REF_KIND))
                    .identifier(root_commit),
            ],
        )
        .await?;

    RepoRef::try_from(
        events
            .iter()
            .filter(|e| e.kind.as_u64() == REPO_REF_KIND)
            .max_by_key(|e| e.created_at)
            .context("cannot find repository reference event")?
            .clone(),
    )
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
            relays: vec!["ws://relay1.io".to_string(), "ws://relay2.io".to_string()],
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
        fn relays() {
            assert_eq!(
                RepoRef::try_from(create()).unwrap().relays,
                vec!["ws://relay1.io".to_string(), "ws://relay2.io".to_string()],
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
            fn no_other_tags() {
                assert_eq!(create().tags.len(), 6)
            }
        }
    }
}
