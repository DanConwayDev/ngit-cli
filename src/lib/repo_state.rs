use std::collections::HashMap;

use anyhow::{Context, Result};
use git2::Oid;

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
        // Include a HEAD if one isn't listed to prevent errors when users git config
        // default branch isn't in the state event
        if !state.contains_key("HEAD") {
            if state.contains_key("refs/heads/master") {
                state.insert("HEAD".to_string(), "ref: refs/heads/master".to_string());
            } else if state.contains_key("refs/heads/main") {
                state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
            } else if let Some(tag) = event
                .tags
                .iter()
                .find(|t| t.len() > 1 && t.as_slice()[0].starts_with("refs/heads/"))
            {
                state.insert(
                    "HEAD".to_string(),
                    format!("ref: {}", tag.clone().to_vec()[0]),
                );
            }
        }
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
}
