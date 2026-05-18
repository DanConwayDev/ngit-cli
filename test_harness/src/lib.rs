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
//! - [`port::find_free_port`] — OS-assigned `127.0.0.1:0` bind, then drop.
//! - [`relay::VanillaRelay`] — `nostr-relay-builder` `LocalRelay` wrapped to
//!   own its port and offer a `events(filter)` query helper. Used for user
//!   metadata (kind 0), relay lists (kind 10002), signer-connect events —
//!   anything that isn't a repo-specific GRASP event.
//! - [`grasp::GraspServer`] — `ngit-grasp` subprocess on a loopback port,
//!   speaking NIP-01 + git smart-http. Required for any test that publishes a
//!   kind-30617 repo announcement or pushes git data through a GRASP server.
//! - [`Harness`] / [`HarnessBuilder`] — fluent role-keyed roster of relays plus
//!   grasp servers; emits the `NGITTEST=TRUE` + four `NGIT_*_SET` env vars
//!   consumed by `Params::default()` in `src/lib/client.rs`.
//! - [`repo::Repo`] — `TempDir`-backed git repo, with [`Repo::ngit`] /
//!   [`Repo::git`] returning a `Command` pre-configured with the harness's env
//!   so children (git → git-remote-nostr) inherit it via `execve`.
//! - [`snapshot::RepoSnapshot`] — `HEAD` + refs only for now; grows as migrated
//!   tests demand.

pub mod grasp;
pub mod harness;
pub mod port;
mod query;
pub mod relay;
pub mod repo;
pub mod snapshot;

pub use grasp::GraspServer;
pub use harness::{Harness, HarnessBuilder};
pub use relay::VanillaRelay;
pub use repo::Repo;
pub use snapshot::RepoSnapshot;
