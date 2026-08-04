//! End-to-end coverage of `ngit sync` mainline flows, one scenario per
//! submodule.
//!
//! `ngit sync` (src/bin/ngit/sub_commands/sync.rs) reconciles the nostr
//! repository state (kind 30618) with the repository's announced git
//! servers. These tests pin its current observable behaviour ahead of
//! the planned refactor onto the shared push transaction
//! (src/bin/ngit/state_transaction.rs) — Phase 0 of push unification.
//! Assertions target refs on the servers' bare repos and events on the
//! relay surfaces, never stdout.
//!
//! ## Layout
//!
//! - [`common`] — shared arrangement: maintainer account, seed commit, manually
//!   signed kind-30617 announcement listing the harness's git servers,
//!   `nostr://` remote, initial `git push -u origin main`.
//! - [`ff_propagation`] — nostr state is ahead of one vanilla git server
//!   (simulated out-of-band rollback). Plain `ngit sync` fast-forwards the
//!   stale server to the nostr state without publishing a new state event.
//! - [`grasp_seeding`] — a grasp server added to the announcement after the
//!   repo already has nostr state. Plain `ngit sync` publishes the existing
//!   state event to the grasp's relay and pushes the git data, seeding the new
//!   server end-to-end.
//! - [`force_divergence`] — a vanilla server holds a diverged branch and a
//!   stray branch unknown to nostr state. Plain `ngit sync` leaves both
//!   untouched (fast-forward-only policy) and still exits successfully; `ngit
//!   sync --force` rewrites the diverged ref and deletes the stray one,
//!   republishing the state event.
//! - [`trust_server`] — a vanilla server is fast-forward ahead of nostr state
//!   (a push that bypassed nostr). Plain `ngit sync` neither adopts nor
//!   downgrades; `ngit sync -t` publishes an updated state event and propagates
//!   the commits to the other server. A second case covers the
//!   `nostr.trust-server-domains` git-config knob that auto-trusts servers by
//!   domain without `-t`.
//!
//! When adding a new scenario file, declare it as another `mod` below
//! and follow the same shape so failures stay pinpoint-named in
//! `cargo test` output.

mod common;
mod ff_propagation;
mod force_divergence;
mod grasp_seeding;
mod trust_server;
