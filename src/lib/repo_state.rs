use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use git2::Oid;
use nostr::{
    event::{EventBuilder, Tag},
    signer::NostrSigner,
};

use crate::client::{STATE_KIND, sign_event};

pub struct RepoState {
    pub identifier: String,
    pub state: HashMap<String, String>,
    pub event: nostr::Event,
}

impl RepoState {
    pub fn try_from(mut state_events: Vec<nostr::Event>) -> Result<Self> {
        state_events.sort_by_key(|e| e.created_at);
        let event = state_events.last().context("no state events")?;
        let mut state = HashMap::new();
        for tag in event.tags.iter() {
            if let Some(name) = tag.as_slice().first() {
                if ["refs/heads/", "refs/tags", "HEAD"]
                    .iter()
                    .any(|s| name.starts_with(*s))
                    // dont include dereferenced tags
                    && !name.ends_with("^{}")
                {
                    if let Some(value) = tag.as_slice().get(1) {
                        if Oid::from_str(value).is_ok() || value.contains("ref: refs/") {
                            state.insert(name.to_owned(), value.to_owned());
                        }
                    }
                }
            }
        }
        add_head(&mut state);
        Ok(RepoState {
            identifier: event
                .tags
                .identifier()
                .context("existing event must have an identifier")?
                .to_string(),
            state,
            event: event.clone(),
        })
    }

    pub async fn build(
        identifier: String,
        mut state: HashMap<String, String>,
        signer: &Arc<dyn NostrSigner>,
    ) -> Result<Self> {
        add_head(&mut state);
        let mut tags = vec![Tag::identifier(identifier.clone())];
        for (name, value) in &state {
            tags.push(Tag::custom(
                nostr_sdk::TagKind::Custom(name.into()),
                vec![value.clone()],
            ));
        }
        let event = sign_event(
            EventBuilder::new(STATE_KIND, "").tags(tags),
            signer,
            "git state".to_string(),
        )
        .await?;
        Ok(RepoState {
            identifier,
            state,
            event,
        })
    }
}

// Include a HEAD if one isn't listed to prevent errors when users git config
// default branch isn't in the state event
fn add_head(state: &mut HashMap<String, String>) {
    if !state.contains_key("HEAD") {
        if state.contains_key("refs/heads/master") {
            state.insert("HEAD".to_string(), "ref: refs/heads/master".to_string());
        } else if state.contains_key("refs/heads/main") {
            state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
        } else if let Some(k) = state.keys().find(|k| k.starts_with("refs/heads/")) {
            state.insert("HEAD".to_string(), format!("ref: {k}"));
        }
    }
}
