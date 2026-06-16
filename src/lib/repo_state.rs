use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use git2::Oid;
use nostr::event::{EventBuilder, Tag};

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
                // include ^{} peeled refs for annotated tags: git requires
                // both "<tag-oid> refs/tags/v1.0.0" and
                // "<commit-oid> refs/tags/v1.0.0^{}" in the list output so
                // it can resolve the tag to a commit. without the ^{} line
                // git fetch --prune deletes the tag as unresolvable.
                if ["refs/heads/", "refs/tags", "HEAD"]
                    .iter()
                    .any(|s| name.starts_with(*s))
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
        signer: &Arc<crate::NgitSigner>,
    ) -> Result<Self> {
        add_head(&mut state);
        let mut tags = vec![Tag::identifier(identifier.clone())];
        for (name, value) in &state {
            tags.push(Tag::parse([name.as_str(), value.as_str()]).unwrap());
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

/// Extract the repository's default branch short name (e.g. `main`) from a
/// nostr repo-state map's `HEAD` tag (`"ref: refs/heads/<branch>"`). This is
/// the maintainer-declared default branch and the most authoritative source
/// for default-branch identification. Returns `None` when no resolvable HEAD
/// symref is present.
#[must_use]
pub fn default_branch_from_state(state: &HashMap<String, String>) -> Option<String> {
    state
        .get("HEAD")
        .and_then(|v| v.strip_prefix("ref: refs/heads/"))
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod default_branch_from_state {
        use super::*;

        #[test]
        fn extracts_non_main_branch_name() {
            let mut state = HashMap::new();
            state.insert("HEAD".to_string(), "ref: refs/heads/develop".to_string());
            assert_eq!(
                default_branch_from_state(&state),
                Some("develop".to_string())
            );
        }

        #[test]
        fn none_when_no_head_tag() {
            let state = HashMap::new();
            assert_eq!(default_branch_from_state(&state), None);
        }

        #[test]
        fn none_when_head_is_not_a_heads_symref() {
            let mut state = HashMap::new();
            // a detached/oid HEAD value is not a branch symref.
            state.insert(
                "HEAD".to_string(),
                "0000000000000000000000000000000000000000".to_string(),
            );
            assert_eq!(default_branch_from_state(&state), None);
        }
    }
}
