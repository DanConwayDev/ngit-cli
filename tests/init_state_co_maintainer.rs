//! `ngit init` State D "CoMaintainer" — successor to legacy
//! `tests/legacy/ngit_init.rs::state_d_co_maintainer::*`.
//!
//! State D is a `nostr.repo` coordinate pointing at *another* maintainer's
//! pubkey, plus an existing kind-30617 (signed by that other maintainer)
//! that lists the publisher in its `maintainers` tag —
//! `validate_post_fetch`'s `CoMaintainer` arm (init.rs:551-562). No
//! `--force` is required; bare `ngit init --grasp-server <url>`
//! republishes the announcement signed by the publisher, inheriting
//! `name` / `description` / `web` from the existing announcement and
//! replacing `clone` / `relays` with the publisher's own grasp
//! infrastructure.
//!
//! ## Coverage
//!
//! Single shared snapshot covering 6 read-only assertions on tags of
//! the post-init announcement (legacy `state_d_co_maintainer::success::*`,
//! one `#[rstest]` per legacy test):
//!
//! - `name_inherited_from_other_maintainer`
//! - `description_inherited_from_other_maintainer`
//! - `web_inherited_from_other_maintainer`
//! - `clone_url_from_my_grasp_server_not_theirs`
//! - `relays_from_my_grasp_server`
//! - `maintainers_is_me_and_selected`
//!
//! No error path is tested — State D never errors on bare init; that is
//! the entire CoMaintainer-vs-NotListed discriminator.
//!
//! Setup uses [`tokio::sync::OnceCell`] so the shared `arrange + ngit init`
//! cost is paid once per binary.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use rstest::*;
use test_harness::Harness;
use tokio::sync::OnceCell;

// ---------------------------------------------------------------------------
// Snapshot — `ngit init --grasp-server <url>` against State D
// ---------------------------------------------------------------------------

/// Captured side-effects of one `ngit init --grasp-server <url>`
/// invocation against a State D repo. Six assertion cases share this
/// snapshot via `OnceCell`.
struct Snapshot {
    /// Post-init kind-30617 signed by the publisher. Located by
    /// querying the default relay for `(author = publisher, kind =
    /// GitRepoAnnouncement, d = coordinate_identifier)` — that filter
    /// excludes the existing State-D event signed by the selected
    /// maintainer, which has a different `pubkey`.
    announcement: Event,
    /// `name` tag carried by the State-D arrange's existing
    /// announcement. Asserted-equal to the post-init announcement's
    /// `name` (legacy `name_inherited_from_other_maintainer`).
    existing_name: String,
    /// `description` from the existing announcement. Asserted-equal to
    /// the post-init `description`.
    existing_description: String,
    /// Marker substring that the existing announcement's `web` tag
    /// contains (`"exampleproject.xyz"`). The post-init announcement's
    /// `web` should also contain a value carrying this substring —
    /// catches "ngit dropped the inherited web tag" without pinning the
    /// exact list, which init.rs may reorder.
    existing_web_marker: String,
    /// Selected maintainer's git server URL on the existing
    /// announcement. Should **not** appear in the post-init `clone`
    /// tag (legacy `clone_url_from_my_grasp_server_not_theirs`).
    existing_clone_url: String,
    /// Grasp HTTP base URL — the post-init `clone` tag should contain
    /// at least one entry starting with `<grasp_http_url>/`.
    grasp_http_url: String,
    /// Grasp WebSocket relay URL — should appear verbatim in the
    /// post-init `relays` tag (legacy `relays_from_my_grasp_server`).
    grasp_relay_url: String,
    /// Publisher's pubkey (hex) — asserted to appear in the post-init
    /// `maintainers` tag.
    me_pubkey_hex: String,
    /// Selected maintainer's pubkey (hex) — asserted to appear
    /// alongside the publisher in the post-init `maintainers` tag.
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
                    .expect("init_state_co_maintainer fixture: capture_snapshot failed"),
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

    let (repo, state) = harness.arrange_init_state_d_co_maintainer().await?;

    let grasp = harness.grasp("repo");
    let grasp_http_url = grasp.url().to_string();
    let grasp_relay_url = grasp.relay_url();

    // No `--force` (CoMaintainer arm doesn't require it), no `--name` /
    // `--description` (those should be inherited from the existing
    // announcement). Mirrors legacy `state_d_co_maintainer::success`'s
    // CLI shape so the inheritance assertions exercise exactly the
    // same code path as legacy did.
    let init_out = repo
        .ngit(["init", "--grasp-server", &grasp_http_url])
        .output()
        .await
        .context("failed to spawn ngit init --grasp-server")?;
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
                 relay after `ngit init --grasp-server` — did State D's CoMaintainer arm fail \
                 to publish?",
                state.coordinate_identifier,
            )
        })?;

    // Pull the marker substring out of the arrange's existing web list.
    // The legacy assertion was `web.iter().any(|w|
    // w.contains("exampleproject.xyz"))`; we replicate that exactly rather than
    // asserting equality with the full inherited list, which init.rs is free to
    // canonicalise.
    let existing_web_marker = state
        .existing_web
        .iter()
        .find(|w| w.contains("exampleproject.xyz"))
        .cloned()
        .map(|s| {
            // Use the substring "exampleproject.xyz" rather than the
            // full URL so a future change to ngit's URL canonicaliser
            // (e.g. trailing-slash normalisation) doesn't break this
            // test for cosmetic reasons.
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
        existing_clone_url: state.existing_clone_url,
        grasp_http_url,
        grasp_relay_url,
        me_pubkey_hex: state.keys.public_key().to_string(),
        selected_pubkey_hex: state.selected_maintainer_keys.public_key().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Assertions — one #[rstest] per legacy test
// ---------------------------------------------------------------------------

/// Equivalent of legacy
/// `state_d_co_maintainer::success::name_inherited_from_other_maintainer`.
/// `ngit init` with no `--name` flag falls through `name_default = lr.name`
/// (init.rs:611) and re-publishes the existing announcement's name.
#[rstest]
#[tokio::test]
async fn name_inherited_from_other_maintainer(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "name").as_deref(),
        Some(s.existing_name.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_d_co_maintainer::success::description_inherited_from_other_maintainer`.
#[rstest]
#[tokio::test]
async fn description_inherited_from_other_maintainer(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    assert_eq!(
        tag_value(&s.announcement, "description").as_deref(),
        Some(s.existing_description.as_str()),
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_d_co_maintainer::success::web_inherited_from_other_maintainer`.
/// At least one value of the post-init `web` tag carries the marker
/// substring from the existing announcement's `web` list, proving the
/// list was inherited rather than ngit's gitworkshop default.
#[rstest]
#[tokio::test]
async fn web_inherited_from_other_maintainer(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
/// `state_d_co_maintainer::success::clone_url_from_my_grasp_server_not_theirs`.
/// Two assertions:
/// 1. The publisher's grasp HTTP URL appears in the `clone` tag.
/// 2. The selected maintainer's git-server URL is **absent** from the `clone`
///    tag — `git_servers_default = vec![]` when `my_ref` is None (init.rs:736),
///    so the only thing surviving into `clone` is what
///    `apply_grasp_infrastructure` adds.
#[rstest]
#[tokio::test]
async fn clone_url_from_my_grasp_server_not_theirs(
    #[future] snapshot: Arc<Snapshot>,
) -> Result<()> {
    let s = snapshot.await;
    let clone_urls = tag_values(&s.announcement, "clone");
    let prefix = format!("{}/", s.grasp_http_url);
    assert!(
        clone_urls.iter().any(|u| u.starts_with(&prefix)),
        "expected at least one clone url starting with {prefix} (the publisher's \
         grasp); got {clone_urls:?}",
    );
    assert!(
        !clone_urls.iter().any(|u| u == &s.existing_clone_url),
        "selected maintainer's git-server URL ({}) should NOT survive into the \
         post-init `clone` tag; got {clone_urls:?}",
        s.existing_clone_url,
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_d_co_maintainer::success::relays_from_my_grasp_server`. The
/// publisher's grasp-derived relay URL is present in the post-init
/// `relays` tag — `relays_default = vec![]` when `my_ref` is None
/// (init.rs:751), so the only relays surviving are what
/// `apply_grasp_infrastructure` prepends.
#[rstest]
#[tokio::test]
async fn relays_from_my_grasp_server(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
    let s = snapshot.await;
    let relays = tag_values(&s.announcement, "relays");
    assert!(
        relays.iter().any(|r| r == &s.grasp_relay_url),
        "post-init `relays` tag should include the publisher's grasp-derived \
         relay url ({}); got {relays:?}",
        s.grasp_relay_url,
    );
    Ok(())
}

/// Equivalent of legacy
/// `state_d_co_maintainer::success::maintainers_is_me_and_selected`. The
/// post-init `maintainers` tag carries exactly the publisher and the
/// selected maintainer — the maintainers-default fallback in
/// init.rs:869-878 when `my_ref` is None and `selected != my_pubkey`.
#[rstest]
#[tokio::test]
async fn maintainers_is_me_and_selected(#[future] snapshot: Arc<Snapshot>) -> Result<()> {
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
