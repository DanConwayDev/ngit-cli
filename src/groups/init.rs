
use std::{str::FromStr, fmt::Debug};
use nostr_sdk::{EventBuilder, Tag, secp256k1::XOnlyPublicKey, Keys, Event};
use serde::{Deserialize, Serialize};

use crate::{kind::Kind, ngit_tag::{tag_extract_relays, tag_admin_group_with_relays, tag_extract_value, tag_hashtag, tag_into_event}};

/// [`InitializeGroup`] error
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error serializing or deserializing JSON data
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    // /// Error adding wrong tag kind to member_groups
    // #[error("expecting event tag for member_groups but got: {0}")]
    // WongMemberGroupsTagKind(String),
    // /// Error InvalidGroupIdInTag
    // #[error("invalid group id in member_groups tag: {0}")]
    // InvalidGroupIdInTag(String),
}

impl InitializeGroup {

    pub fn initialize(&self,keys:&Keys) -> Event {
        // let keys = Keys::generate();
        EventBuilder::new(
            Kind::InitializeGroup.into_sdk_custom_kind(),
            self.as_json(),
            &self.generate_tags(),
        )
        .to_unsigned_event(keys.public_key())
        .sign(&keys)
        .unwrap()
    }

    fn generate_tags(&self) -> Vec<Tag> {
        let mut tags:Vec<Tag> = vec![
            tag_hashtag("ngit-event"),
            tag_hashtag("ngit-format-0.0.1"),
        ];
        for m in &self.direct_members {
            let key = XOnlyPublicKey::from_str(m);
            match key {
                Ok(k) => tags.push(Tag::PubKey(k, None)),
                Err(error) => print!("could not add this pubkey to tag: {m} error: {error}"),
            }
        }
        for m in &self.member_groups {
            tags.push(m.clone());
            tags.push(tag_into_event(m.clone()));

        }
        match &self.admin {
            None => (),
            Some(admin) => { 
                tags.push(tag_admin_group_with_relays(
                    &tag_extract_value(admin),
                    &tag_extract_relays(admin),
                ));
                tags.push(tag_into_event(admin.clone()));
            },
        };
        tags
    }
}

/// InitializeGroup
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct InitializeGroup {
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
    /// Direct Members
    pub direct_members: Vec<String>,  
    /// Member Groups as group tag vector
    pub member_groups: Vec<Tag>, 
    /// Admin
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin: Option<Tag>,

}

impl Default for InitializeGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl InitializeGroup {
    /// New empty [`InitializeGroup`]
    pub fn new() -> Self {
        Self {
            name: None,
            about: None,
            picture: None,
            relays: vec![],
            direct_members: vec![],
            member_groups: vec![],
            admin: None,
        }
    }

    /// Deserialize [`InitializeGroup`] from `JSON` string
    pub fn from_json<S>(json: S) -> Result<Self, Error>
    where
        S: Into<String>,
    {
        Ok(serde_json::from_str(&json.into())?)
    }

    /// Serialize [`InitializeGroup`] to `JSON` string
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

    /// Set members
    pub fn members(mut self, pubkeys: Vec<String>, group_refs:Vec<Tag>) -> Self /* Result<Self,Error>*/ {
        for m in pubkeys {
            let key = XOnlyPublicKey::from_str(m.as_str());
            match key {
                Ok(_k) => self.direct_members.push(m),
                Err(error) => print!("could not add this pubkey to members: {m} error: {error}"),
            }
        }
        for group_ref in group_refs {
            self.member_groups.push(group_ref);
        }
        self
    }

    /// Set admin
    pub fn admin(self, group_ref: Tag) -> Self {
        Self {
            admin: Some(group_ref),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {

    use nostr::prelude::UncheckedUrl;

    use crate::ngit_tag::{tag_group_with_relays, tag_group};

    use super::*;

    #[test]
    fn test_deserialize_content() {
        let content = r#"{
            "name":"myname",
            "picture":"https://www.example.com/profile.jpg",
            "direct_members":[
                "88a14a0df1aa0223e9f3a44cd4964fb82a19590440bb8cf1610d8c7367798314",
                "14c27d59268ae2554d03b89c5c01dac17a604b17ac258ad345bd0648d3f5c011"
            ],
            "member_groups":[
                ["group","109ca9850488d301147ac92c6ea3e1d3dd3ebe3a59dcd1151e99c7e16ef48897","ws://localhost"],
                ["group","06bd7667a7c115fd8faf7f300302f39c019e16e6461845930686b84fbeae8c87"]
            ],
            "relays":["wss://relay.damus.io","ws://localhost"],
            "admin":["admin-group","109ca9850488d301147ac92c6ea3e1d3dd3ebe3a59dcd1151e99c7e16ef48897","ws://localhost"]
        }"#;
        assert_eq!(
            InitializeGroup::from_json(content).unwrap(),
            InitializeGroup::new()
                .name("myname")
                // 'about' intentionally ommitted
                .picture("https://www.example.com/profile.jpg")
                .members(
                    vec![
                        "88a14a0df1aa0223e9f3a44cd4964fb82a19590440bb8cf1610d8c7367798314".to_string(),
                        "14c27d59268ae2554d03b89c5c01dac17a604b17ac258ad345bd0648d3f5c011".to_string(),
                    ],
                    vec![
                        tag_group_with_relays(
                            &"109ca9850488d301147ac92c6ea3e1d3dd3ebe3a59dcd1151e99c7e16ef48897".to_string(),
                            &vec!["ws://localhost".to_string()],
                        ),
                        tag_group(
                            &"06bd7667a7c115fd8faf7f300302f39c019e16e6461845930686b84fbeae8c87".to_string(),
                        ),
                    ],
                )
                .relays(&vec!["wss://relay.damus.io".to_string(),"ws://localhost".to_string()])
                .admin(
                    tag_admin_group_with_relays(
                        &"109ca9850488d301147ac92c6ea3e1d3dd3ebe3a59dcd1151e99c7e16ef48897".to_string(),
                        &vec![UncheckedUrl::from_str("ws://localhost").unwrap()],
                    ),
                )
        );
    }
}
