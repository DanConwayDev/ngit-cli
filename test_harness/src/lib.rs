//! Integration test harness for ngit.
//!
//! Successor to `test_utils`. Do not import from `test_utils` here, and
//! do not re-export `test_utils` types — the boundary is hermetic by design
//! (see `docs/architecture/test-harness.md`).
//!
//! ## Scope of v1 (relay-only)
//!
//! This first cut covers the building blocks needed to drive ngit subcommands
//! against a vanilla nostr relay:
//!
//! - [`port::find_free_port`] — OS-assigned `127.0.0.1:0` bind, then drop.
//! - [`relay::VanillaRelay`] — `nostr-relay-builder` `LocalRelay` wrapped to
//!   own its port and offer a `events(filter)` query helper.
//! - [`Harness`] / [`HarnessBuilder`] — fluent role-keyed relay roster plus the
//!   four `NGIT_*` env-var rosters consumed by `Params::default()` in
//!   `src/lib/client.rs`.
//! - [`repo::Repo`] — `TempDir`-backed git repo, with [`Repo::ngit`] /
//!   [`Repo::git`] returning a `Command` pre-configured with the harness's env
//!   so children (git → git-remote-nostr) inherit it via `execve`.
//! - [`snapshot::RepoSnapshot`] — `HEAD` + refs only for now; grows as migrated
//!   tests demand.
//!
//! GRASP subprocess management is intentionally **not** scaffolded here. It
//! lands in the PR that introduces the first grasp-using test.

pub mod harness;
pub mod port;
pub mod relay;
pub mod repo;
pub mod snapshot;

pub use harness::{Harness, HarnessBuilder};
pub use relay::VanillaRelay;
pub use repo::Repo;
pub use snapshot::RepoSnapshot;
