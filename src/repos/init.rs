
use std::fmt::Debug;
use nostr_sdk::{EventBuilder, Tag, Keys, Event};
use serde::{Deserialize, Serialize};

use crate::{kind::Kind, ngit_tag::{tag_repo, tag_relays, tag_hashtag, tag_group_with_relays, tag_into_event}};

/// [`InitializeRepo`] error
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error serializing or deserializing JSON data
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl InitializeRepo {

    pub fn initialize(&self,keys:&Keys) -> Event {
        // let keys = Keys::generate();
        EventBuilder::new(
            nostr_sdk::Kind::Custom(
                match self.root_repo {
                    None => u64::from(Kind::InitializeRepo),
                    _ => u64::from(Kind::InitializeBranch),
                }
            ),
            self.as_json(),
            &self.generate_tags(),
        )
        .to_unsigned_event(keys.public_key())
        .sign(&keys)
        .unwrap()
    }

    fn generate_tags(&self) -> Vec<Tag> {
        let mut tags = 
        vec![
            self.maintainers_group.as_ref()
                .expect("there always to be a maintainers group when initialising")
                .clone(),
            tag_hashtag("ngit-event"),
            tag_hashtag("ngit-format-0.0.1"),
        ];
        if !self.relays.is_empty() {
            tags.push(
                tag_relays(&self.relays)
            );
        }

        match &self.root_repo {
            None =>(),
            Some(id) => {
                tags.push(
                    tag_repo(id)
                );
                tags.push(
                    tag_into_event(
                        // its a bit silly / lazy reusing this function just to get the tags formatted with relays when it is not a group
                        tag_group_with_relays(id, &self.relays)
                    )
                );

            }
        }
        tags
    }
}

/// InitializeRepo
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct InitializeRepo {
    /// Name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Picture
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// relays
    pub relays: Vec<String>,
    /// Maintainers Group
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintainers_group: Option<Tag>,
    /// Maintainers Group
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_repo: Option<String>,
}

impl Default for InitializeRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl InitializeRepo {
    /// New empty [`InitializeRepo`]
    pub fn new() -> Self {
        Self {
            name: None,
            about: None,
            picture: None,
            relays: vec![],
            maintainers_group: None,
            root_repo:None,
        }
    }

    /// Deserialize [`InitializeRepo`] from `JSON` string
    pub fn from_json<S>(json: S) -> Result<Self, Error>
    where
        S: Into<String>,
    {
        Ok(serde_json::from_str(&json.into())?)
    }

    /// Serialize [`InitializeRepo`] to `JSON` string
    pub fn as_json(&self) -> String {
        serde_json::json!(self).to_string()
    }

    /// Set name
    pub fn name<S>(self, name: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            name: Some(name.into()),
            ..self
        }
    }

    /// Set about
    pub fn about<S>(self, about: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            about: Some(about.into()),
            ..self
        }
    }

    /// Set picture
    pub fn picture<S>(self, picture: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            picture: Some(picture.into()),
            ..self
        }
    }

    /// Set relays
    pub fn relays(mut self, relays: &Vec<String>) -> Self {
        for m in relays {
            self.relays.push(m.clone());
        }
        self
    }

    /// Set maintainers_group
    pub fn maintainers_group(self, group_ref: Tag) -> Self {
        Self {
            maintainers_group: Some(group_ref),
            ..self
        }
    }

    /// Set root_repo
    pub fn root_repo<S>(self, root_repo: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            root_repo: Some(root_repo.into()),
            ..self
        }
    }
    
}
