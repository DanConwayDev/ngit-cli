//! `ngit init` State E "NotListed" — successor to legacy
//! `tests/legacy/ngit_init.rs::state_e_not_listed::*`.
//!
//! State E is a `nostr.repo` coordinate pointing at *another* maintainer's
//! pubkey, plus an existing kind-30617 (signed by that other maintainer)
//! whose `maintainers` tag does **not** include the publisher —
//! `validate_post_fetch`'s `NotListed` arm (init.rs:564-574). Bare
//! `ngit init` errors with `"you are not listed as a maintainer"`;
//! `--defaults` does not bypass the check (regression for the `-d`
//! shortcut accidentally short-circuiting maintainer-list validation);
//! only `--force` proceeds, after which the publisher is added to the
//! new announcement's `maintainers` tag alongside the selected maintainer.
//!
//! ## Coverage
//!
//! - **Errors** (2 tests, no shared setup):
//!   - `bare_no_flags_errors_not_listed` → legacy
//!     `state_e_not_listed::errors::bare_no_flags`
//!   - `defaults_still_requires_force` → legacy
//!     `state_e_not_listed::errors::defaults_still_requires_force`
//! - **Force success** (1 captured snapshot, 4 rstest cases asserting on tags
//!   of the post-init announcement):
//!   - `force_inherits_name` → legacy
//!     `state_e_not_listed::success::name_inherited_from_other_maintainer`
//!   - `force_inherits_description` → legacy
//!     `state_e_not_listed::success::description_inherited_from_other_maintainer`
//!   - `force_inherits_web_marker` → legacy
//!     `state_e_not_listed::success::web_inherited_from_other_maintainer`
//!   - `force_maintainers_is_me_and_selected` → legacy
//!     `state_e_not_listed::success::maintainers_is_me_and_selected`
//!
//! ## Error-message brittleness
//!
//! Same caveat as `tests/init_state_fresh.rs`: the error tests
//! substring-match on `cli_error`'s output. Substring assertions on
//! stable error prefixes are tolerated as a regression-catching
//! shortcut; exact-stdout is forbidden by the harness rules.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::Harness;
use tokio::sync::OnceCell;

// ---------------------------------------------------------------------------
// Errors — independent harnesses per case, same as State B / State C
// ---------------------------------------------------------------------------

/// Equivalent of legacy
/// `state_e_not_listed::errors::bare_no_flags`. With no flags,
/// `validate_post_fetch`'s NotListed arm (init.rs:564-574) emits the
/// `"you are not listed as a maintainer"` error and ngit exits non-zero.
#[tokio::test]
async fn bare_no_flags_errors_not_listed() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_e_not_listed().await?;
    let out = repo
        .ngit(["init"])
        .output()
        .await
        .context("failed to spawn ngit init")?;

    assert!(
        !out.status.success(),
        "expected `ngit init` to fail in State E; exited successfully\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("you are not listed as a maintainer"),
        "expected not-listed error, got: {combined}",
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_e_not_listed::errors::defaults_still_requires_force`.
/// `--defaults` does **not** bypass the NotListed check — that gate
/// only honours `--force`. Pins the regression where `-d` accidentally
/// short-circuited maintainer-list validation.
#[tokio::test]
async fn defaults_still_requires_force() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, _state) = harness.arrange_init_state_e_not_listed().await?;
    let out = repo
        .ngit(["init", "--defaults"])
        .output()
        .await
        .context("failed to spawn ngit init --defaults")?;

    assert!(
        !out.status.success(),
        "expected `ngit init --defaults` to fail in State E; exited successfully\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("you are not listed as a maintainer"),
        "expected not-listed error even with --defaults, got: {combined}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Force success — `ngit init --force --grasp-server <url>`
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --force --grasp-server <url>`
/// invocation against a State E repo. Four assertion cases share this
/// snapshot via `OnceCell`.
struct Snapshot {
    /// Post-init kind-30617 signed by the publisher. Same query strategy
    /// as the State-D snapshot — filter the default relay by `(author =
    /// publisher, kind = GitRepoAnnouncement, d = identifier)` so the
    /// existing event signed by the selected maintainer (different
    /// `pubkey`) is excluded.
    announcement: Event,
    /// `name` from the existing announcement; should be inherited.
    existing_name: String,
    /// `description` from the existing announcement; should be inherited.
    existing_description: String,
    /// Marker substring (`"exampleproject.xyz"`) the existing
    /// announcement's `web` tag carries. The post-init `web` should
    /// also contain a value carrying this substring.
    existing_web_marker: String,
    /// Publisher's pubkey (hex).
    me_pubkey_hex: String,
    /// Selected maintainer's pubkey (hex).
    selected_pubkey_hex: String,
}

static SNAPSHOT: OnceCell<Arc<Snapshot>> = OnceCell::const_new();

#[fixture]
async fn snapshot() -> Arc<Snapshot> {
    SNAPSHOT
        .get_or_init(|| async {
            Arc::new(
                capture_snapshot()
                    .await
                    .expect("init_state_not_listed fixture: capture_snapshot failed"),
            )
        })
        .await
        .clone()
}

async fn capture_snapshot() -> Result<Snapshot> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_e_not_listed().await?;
    let grasp = harness.grasp("repo");
    let grasp_http_url = grasp.url().to_string();

    // `--force` bypasses the NotListed gate; `--grasp-server` provides
    // the publisher's clone-URL infrastructure (init.rs's republish
    // path needs at least one git server). No `--name` /
    // `--description` so name/description inheritance from the existing
    // announcement is exercised.
    let init_out = repo
        .ngit(["init", "--force", "--grasp-server", &grasp_http_url])
        .output()
        .await
        .context("failed to spawn ngit init --force --grasp-server")?;
    if !init_out.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init_out.status,
            String::from_utf8_lossy(&init_out.stdout),
            String::from_utf8_lossy(&init_out.stderr),
        );
    }

    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(state.keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .into_iter()
        .find(|e| tag_value(e, "d").as_deref() == Some(state.coordinate_identifier.as_str()))
        .with_context(|| {
            format!(
                "no kind-30617 with `d` = {:?} authored by the publisher on the default \
                 relay after `ngit init --force --grasp-server` — did State E's NotListed \
                 force-republish path fail?",
                state.coordinate_identifier,
            )
        })?;

    let existing_web_marker = state
        .existing_web
        .iter()
        .find(|w| w.contains("exampleproject.xyz"))
        .cloned()
        .map(|s| {
            s.split('/')
                .find(|seg| seg.contains("exampleproject.xyz"))
                .unwrap_or("exampleproject.xyz")
                .to_string()
        })
        .unwrap_or_else(|| "exampleproject.xyz".to_string());

    Ok(Snapshot {
        announcement,
        existing_name: state.existing_name,
        existing_description: state.existing_description,
        existing_web_marker,
        me_pubkey_hex: state.keys.public_key().to_string(),
        selected_pubkey_hex: state.selected_maintainer_keys.public_key().to_string(),
    })
}

/// Equivalent of legacy
/// `state_e_not_listed::success::name_inherited_from_other_maintainer`.
#[rstest]
#[tokio::test]
async fn force_inherits_name(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "name").as_deref(),
        Some(s.existing_name.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_e_not_listed::success::description_inherited_from_other_maintainer`.
#[rstest]
#[tokio::test]
async fn force_inherits_description(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "description").as_deref(),
        Some(s.existing_description.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_e_not_listed::success::web_inherited_from_other_maintainer`.
/// `--force` does not change the `web`-inheritance behaviour: the
/// existing announcement's web list survives into the post-init event.
#[rstest]
#[tokio::test]
async fn force_inherits_web_marker(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let web = tag_values(&s.announcement, "web");
    assert!(
        web.iter().any(|w| w.contains(&s.existing_web_marker)),
        "post-init `web` tag should inherit a value containing {:?} from the \
         existing announcement; got {web:?}",
        s.existing_web_marker,
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_e_not_listed::success::maintainers_is_me_and_selected`. Even
/// though the existing announcement excluded the publisher, the
/// post-`--force` announcement carries `[publisher, selected]` —
/// init.rs:869-878's coordinate-fallback branch when `my_ref` is None
/// and `selected != my_pubkey`.
#[rstest]
#[tokio::test]
async fn force_maintainers_is_me_and_selected(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let maintainers = tag_values(&s.announcement, "maintainers");
    assert_eq!(
        maintainers.len(),
        2,
        "post-init `maintainers` tag should have exactly 2 entries; got \
         {maintainers:?}",
    );
    assert!(
        maintainers.contains(&s.me_pubkey_hex),
        "post-init `maintainers` tag should include the publisher ({}); got \
         {maintainers:?}",
        s.me_pubkey_hex,
    );
    assert!(
        maintainers.contains(&s.selected_pubkey_hex),
        "post-init `maintainers` tag should include the selected maintainer ({}); \
         got {maintainers:?}",
        s.selected_pubkey_hex,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — duplicated from sibling init_state_*.rs because each
// integration test compiles as its own binary.
// ---------------------------------------------------------------------------

fn tag_value(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

fn tag_values(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(key))
        .map(|t| t.as_slice()[1..].to_vec())
        .unwrap_or_default()
}
