use anyhow::{Context, Result};
use nostr::Tag;

#[derive(Default)]
pub struct RepoRef {
    name: String,
    description: String,
    root_commit: String,
    relays: Vec<String>,
    // git_server: String,
    // other maintainers
    // code languages and hashtags
}

impl RepoRef {
    pub fn set_name(&mut self, name: String) -> &mut Self {
        self.name = name;
        self
    }

    pub fn set_description(&mut self, description: String) -> &mut Self {
        self.description = description;
        self
    }

    pub fn set_root_commit(&mut self, root_commit: String) -> &mut Self {
        self.root_commit = root_commit;
        self
    }

    pub fn set_relays(&mut self, relays: Vec<String>) -> &mut Self {
        self.relays = relays;
        self
    }

    pub fn to_event(&self, keys: &nostr::Keys) -> Result<nostr::Event> {
        nostr_sdk::EventBuilder::new(
            nostr::event::Kind::Custom(30017),
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

#[cfg(test)]
mod tests {
    use test_utils::*;

    use super::*;

    mod to_event {
        use super::*;
        mod tags {
            use super::*;
            fn create() -> nostr::Event {
                RepoRef::default()
                    .set_name("test name".to_string())
                    .set_description("test description".to_string())
                    .set_root_commit("23471389461".to_string())
                    .set_relays(vec![
                        "ws://relay1.io".to_string(),
                        "ws://relay2.io".to_string(),
                    ])
                    .to_event(&TEST_KEY_1_KEYS)
                    .unwrap()
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
