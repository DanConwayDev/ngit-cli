//! Integration test harness for ngit.
//!
//! Successor to `test_utils`. Do not import from `test_utils` here, and
//! do not re-export `test_utils` types — the boundary is hermetic by design
//! (see `docs/architecture/test-harness.md`).
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
//! - [`relay::VanillaRelay`] — `nostr-relay-builder` `LocalRelay` wrapped to
//!   own its port and offer a `events(filter)` query helper. Used for user
//!   metadata (kind 0), relay lists (kind 10002), signer-connect events —
//!   anything that isn't a repo-specific GRASP event.
//! - [`grasp::GraspServer`] — `ngit-grasp` subprocess on a loopback port,
//!   speaking NIP-01 + git smart-http. Required for any test that publishes a
//!   kind-30617 repo announcement or pushes git data through a GRASP server.
//! - [`vanilla_git_server::VanillaGitServer`] — in-process smart-HTTP git
//!   server with **full push and fetch support**, for tests that need a
//!   non-grasp clone URL on a repo announcement. Covers the
//!   `is_grasp_server_clone_url == false` branches throughout the codebase
//!   that `GraspServer` cannot exercise.
//! - [`Harness`] / [`HarnessBuilder`] — fluent role-keyed roster of relays plus
//!   grasp servers; emits the `NGITTEST=TRUE` + four `NGIT_*_SET` env vars
//!   consumed by `Params::default()` in `src/lib/client.rs`.
//! - [`repo::Repo`] — `TempDir`-backed git repo, with [`Repo::ngit`] /
//!   [`Repo::git`] returning a `Command` pre-configured with the harness's env
//!   so children (git → git-remote-nostr) inherit it via `execve`.
//! - [`scenarios`] — multi-step setup helpers built on the primitives above
//!   ([`Harness::publish_repo`], [`Harness::clone_published_repo`]). Use these
//!   when your test starts "maintainer publishes a repo; contributor clones it;
//!   ...".
//! - [`snapshot::RepoSnapshot`] — `HEAD` + refs only for now; grows as migrated
//!   tests demand.

pub mod clock;
pub mod grasp;
pub mod harness;
pub mod port;
mod query;
pub mod relay;
pub mod repo;
pub mod scenarios;
pub mod snapshot;
pub mod vanilla_git_server;

pub use clock::tick_to_next_second;
pub use grasp::GraspServer;
pub use harness::{Harness, HarnessBuilder};
pub use relay::VanillaRelay;
pub use repo::Repo;
pub use scenarios::{
    ArrangedInitStateA, ArrangedInitStateB, ArrangedInitStateC, CloneLogin, PublishPatchSeriesOpts,
    PublishPrOpts, PublishRepoOpts, PublishStateEventOpts, PublishStateEventTarget,
    PublishedPatchSeries, PublishedPr, PublishedRepo,
};
pub use snapshot::RepoSnapshot;
pub use vanilla_git_server::VanillaGitServer;
