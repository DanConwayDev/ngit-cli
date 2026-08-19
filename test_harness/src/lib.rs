//! Integration test harness for ngit.
//!
//! ## Scope
//!
//! Building blocks for driving ngit subcommands against either a vanilla
//! nostr relay or a real `ngit-grasp` subprocess (or both):
//!
//! - [`port::reserve_port`] — OS-assigned `127.0.0.1:0` bind, held open in a
//!   [`port::PortReservation`] until the consuming fixture is about to start
//!   its real service. Eliminates same-process port races between parallel
//!   `#[tokio::test]`s.
//! - [`port::UnavailableTcpEndpoint`] — test-owned loopback endpoint that
//!   closes every accepted connection without a response. Failure-path tests
//!   get prompt protocol errors without releasing a supposedly dead port that
//!   another process could claim.
//! - [`relay::VanillaRelay`] — `nostr-relay-builder` `LocalRelay` wrapped to
//!   own its port and offer a `events(filter)` query helper. Used for user
//!   metadata (kind 0), relay lists (kind 10002), signer-connect events —
//!   anything that isn't a repo-specific GRASP event.
//! - [`grasp::GraspServer`] — `ngit-grasp` subprocess on a loopback port,
//!   speaking NIP-01 + git smart-http. Required for any test that publishes a
//!   kind-30617 repo announcement or pushes git data through a GRASP server.
//!   [`HarnessBuilder::with_grasp_server_at_base_path`] and
//!   [`HarnessBuilder::with_grasp_server_grasp06_at_base_path`] mount both
//!   protocol surfaces below a non-root public path.
//! - [`buzz::BuzzServer`] — the Nix-built Buzz relay plus isolated Postgres,
//!   Redis, and Garage subprocesses. It provisions channels and repository
//!   announcements through their Nostr protocol events, then exposes the relay
//!   and authenticated Smart HTTP surfaces to ngit.
//! - [`vanilla_git_server::VanillaGitServer`] — in-process smart-HTTP git
//!   server with **full push and fetch support**, for tests that need a
//!   non-grasp clone URL on a repo announcement. Covers the
//!   `is_grasp_server_clone_url == false` branches throughout the codebase that
//!   `GraspServer` cannot exercise. Register via
//!   [`HarnessBuilder::with_vanilla_git_server`] (empty bare repo, harness-
//!   owned lifetime) and look up with [`Harness::vanilla_git_server`]; or
//!   construct directly via [`VanillaGitServer::start`] when the test needs a
//!   pre-populated source repo.
//! - [`Harness`] / [`HarnessBuilder`] — fluent role-keyed roster of relays,
//!   grasp servers, and vanilla git servers; emits the `NGITTEST=TRUE` + four
//!   `NGIT_*_SET` env vars consumed by `Params::default()` in
//!   `src/lib/client.rs` (vanilla git servers are role-keyed lookups only — no
//!   env injection, since ngit has no process-level git-server discovery).
//! - [`repo::Repo`] — `TempDir`-backed git repo, with [`Repo::ngit`] /
//!   [`Repo::git`] returning a `Command` pre-configured with the harness's env
//!   so children (git → git-remote-nostr) inherit it via `execve`.
//! - [`scenarios`] — multi-step setup helpers built on the primitives above
//!   ([`Harness::publish_repo`], [`Harness::clone_published_repo`]). Use these
//!   when your test starts "maintainer publishes a repo; contributor clones it;
//!   ...".
//! - [`roles`] — NIP-34 role-tag announcement fixtures
//!   ([`Harness::publish_fabricated_announcement`],
//!   [`Harness::publish_repo_with_role_graph`]) for member graphs ngit's own
//!   emission cannot produce: moderator assignments, acknowledgements, role
//!   history boundaries, deprecated `maintainers`-only listings.
//! - [`snapshot::RepoSnapshot`] — `HEAD` + refs only for now; grows as migrated
//!   tests demand.

pub mod buzz;
pub mod grasp;
pub mod harness;
pub mod nostr;
pub mod port;
mod query;
pub mod relay;
pub mod repo;
pub mod roles;
pub mod scenarios;
pub mod snapshot;
pub mod vanilla_git_server;

pub use buzz::BuzzServer;
pub use grasp::GraspServer;
pub use harness::{Harness, HarnessBuilder};
pub use nostr::{
    KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, KIND_REPO_STATE, event_branch_name_tag, tag_value,
    tag_values, tag_values_multiple,
};
pub use nostr_sdk::local_relay::LocalRelayBuilderNip42;
pub use port::UnavailableTcpEndpoint;
pub use relay::VanillaRelay;
pub use repo::Repo;
pub use roles::{FabricateAnnouncementOpts, RoleEntry, RoleGraph, RoleLetter};
pub use scenarios::{
    ArrangedInitStateA, ArrangedInitStateB, ArrangedInitStateC, CloneLogin, PublishPatchSeriesOpts,
    PublishPrOpts, PublishRepoOpts, PublishStateEventOpts, PublishStateEventTarget,
    PublishedPatchSeries, PublishedPr, PublishedRepo,
};
pub use snapshot::RepoSnapshot;
pub use vanilla_git_server::VanillaGitServer;
