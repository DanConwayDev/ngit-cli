//! End-to-end coverage of `git push` to a `nostr://` remote, organised
//! as one scenario per submodule.
//!
//! Each scenario builds its own [`test_harness::Harness`], captures a
//! per-scenario snapshot via a [`tokio::sync::OnceCell`]-backed
//! `#[fixture]`, and then asserts on that snapshot from one
//! `#[rstest] #[tokio::test]` per property. The OnceCell is module-local
//! so scenarios never share state, and `cargo test --test git_push_state
//! <scenario>::<case>` still exercises the full setup path for just the
//! case you care about.
//!
//! ## Layout
//!
//! - [`fresh_repo`] — pushing a brand-new repo for the first time (manual
//!   kind-30617 announcement, no `ngit init`). Asserts that the single pushed
//!   `main` branch lands on both grasps + the vanilla relay, the state event
//!   names it as HEAD, and both clone surfaces reproduce it.
//! - [`add_branch`] — builds on [`fresh_repo`]'s end state, then pushes an
//!   additional `vnext` branch. Asserts `main` is preserved, HEAD stays on
//!   `main`, the new branch shows up everywhere, and the state event now lists
//!   both refs.
//! - [`delete_branch`] — builds on [`add_branch`]'s end state, then issues `git
//!   push origin --delete vnext`. Asserts `main` is preserved, HEAD stays on
//!   `main`, the deleted branch disappears from every observable surface
//!   (publisher remote-tracking, both clones, both grasps, state event), and
//!   the local branch + its upstream config are not collateral damage.
//!
//! When adding a new scenario file, declare it as another `mod` below
//! and follow the same fixture / case shape so failures stay
//! pinpoint-named in `cargo test` output.

mod add_branch;
mod delete_branch;
mod fresh_repo;
