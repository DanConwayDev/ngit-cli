//! Nostr protocol constants and event-inspection helpers shared across
//! integration tests.
//!
//! ## Why this module exists
//!
//! Every `tests/send_pr*.rs` file independently declared the same
//! `KIND_PULL_REQUEST`, `KIND_PULL_REQUEST_UPDATE`, `tag_value`,
//! `tag_values`, and `event_branch_name_tag` symbols. This module is the
//! single source of truth; new tests import from here rather than copying.
//!
//! ## Sync contract
//!
//! The kind numbers are mirrored by hand from `src/lib/git_events.rs` /
//! `src/lib/client.rs`. If `src/` ever renumbers an event kind, both the
//! source and the corresponding constant here must be updated.

use nostr_sdk::prelude::*;

// ---------------------------------------------------------------------------
// Event-kind constants
// ---------------------------------------------------------------------------

/// `KIND_PULL_REQUEST` from `src/lib/git_events.rs:113`.
///
/// Mirrored here so test crates do not have to depend on the ngit lib crate.
pub const KIND_PULL_REQUEST: Kind = Kind::Custom(1618);

/// `KIND_PULL_REQUEST_UPDATE` from `src/lib/git_events.rs:114`.
///
/// Mirrored for the same reason as [`KIND_PULL_REQUEST`].
pub const KIND_PULL_REQUEST_UPDATE: Kind = Kind::Custom(1619);

/// `STATE_KIND` / kind 30618 from `src/lib/client.rs`.
///
/// The replaceable kind used by `git-remote-nostr push` and
/// [`crate::scenarios::Harness::publish_state_event`] to advertise the
/// current ref→oid mapping for a repository. Named `KIND_REPO_STATE` here
/// to be self-documenting at call sites.
pub const KIND_REPO_STATE: Kind = Kind::Custom(30618);

// ---------------------------------------------------------------------------
// Tag inspection helpers
// ---------------------------------------------------------------------------

/// Returns the first value (slot 1) of the first tag whose name (slot 0)
/// equals `key`, if any.
///
/// Matches the inline `tag_value` helper that was duplicated in
/// `tests/send_pr.rs`, `tests/send_pr_update.rs`,
/// `tests/send_pr_update_rebase.rs`, and the `init_state_*` tests.
pub fn tag_value(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

/// Returns all values (slots 1+) of the first tag whose name (slot 0)
/// equals `key`. Returns an empty `Vec` when no such tag is present.
///
/// Used for multi-value tags such as `clone` and `relays`. Matches the
/// inline `tag_values` helper duplicated in `tests/send_pr*.rs`.
pub fn tag_values(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(key))
        .map(|t| t.as_slice()[1..].to_vec())
        .unwrap_or_default()
}

/// Returns the value of the `branch-name` tag on a nostr event, if present.
///
/// Both `KIND_PULL_REQUEST` and `Kind::GitPatch` events carry this tag.
/// We use it to pick out the single proposal for a specific branch from a
/// relay surface that may contain events from earlier test runs.
///
/// Delegates to [`tag_value`] so the duplicated body doesn't recur.
pub fn event_branch_name_tag(event: &Event) -> Option<String> {
    tag_value(event, "branch-name")
}
