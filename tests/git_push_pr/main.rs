//! End-to-end coverage of `git push` to a `nostr://` remote that produces a
//! KIND_PULL_REQUEST event, organised as one scenario per submodule.
//!
//! Each scenario builds its own [`test_harness::Harness`], captures a
//! per-scenario snapshot via a [`tokio::sync::OnceCell`]-backed
//! `#[fixture]`, and then asserts on that snapshot from one
//! `#[rstest] #[tokio::test]` per property. The OnceCell is module-local
//! so scenarios never share state, and `cargo test --test git_push_pr
//! <scenario>::<case>` still exercises the full setup path for just the
//! case you care about.
//!
//! ## Layout
//!
//! - [`new_pr`] — contributor pushes a `pr/feature` branch against a
//!   single-grasp repo for the first time. Asserts that the push fires the
//!   9e06e7b "GRASP server → default to PR kind" code path: one
//!   KIND_PULL_REQUEST event lands on the grasp, no GitPatch or
//!   KIND_PULL_REQUEST_UPDATE events are produced, remote-tracking refs and
//!   `-u` upstream config are correctly written, the grasp bare repo carries
//!   `refs/nostr/<event_id>`, and a fresh clone lists the branch as
//!   `pr/feature(<shorthand>)`.
//!
//! When adding a new scenario file, declare it as another `mod` below
//! and follow the same fixture / case shape so failures stay
//! pinpoint-named in `cargo test` output.

mod new_pr;
