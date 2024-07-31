use anyhow::{Context, Result};
use git2::Oid;

pub struct RepoState {
    pub identifier: String,
    pub state: Vec<(String, String)>,
    pub event: nostr::Event,
}

impl RepoState {
    pub fn try_from(mut state_events: Vec<nostr::Event>) -> Result<Self> {
        state_events.sort_by_key(|e| e.created_at);
        let event = state_events.first().context("no state events")?;
        let mut state = vec![];
        for tag in &event.tags {
            if let Some(name) = tag.as_vec().first() {
                if ["refs/heads/", "refs/tags", "HEAD"]
                    .iter()
                    .any(|s| name.starts_with(*s))
                {
                    if let Some(value) = tag.as_vec().get(1) {
                        if Oid::from_str(value).is_ok() {
                            state.push((name.to_owned(), value.to_owned()));
                        }
                    }
                }
            }
        }
        Ok(RepoState {
            identifier: event
                .identifier()
                .context("existing event must have an identifier")?
                .to_string(),
            state,
            event: event.clone(),
        })
    }
}
