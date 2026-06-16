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
//! - [`new_pr_custom_subject_desc`] — contributor pushes `pr/feature` with `-o
//!   title=bla -o 'description=bla\n\ntest'` push options.  Asserts that the
//!   `subject` tag and `content` field of the PR event carry the override
//!   values and that `\n` (backslash + 'n') sequences in the description are
//!   decoded into real newline characters by `decode_push_option_escapes`.
//! - [`patch_kind_when_no_grasp`] — contributor pushes a `pr/feature` branch
//!   against a repo whose kind-30617 announcement has **no** GRASP server.
//!   Asserts the complementary code path: `Kind::GitPatch` events are produced
//!   (not KIND_PULL_REQUEST), confirming that `repo_has_grasp_server = false`
//!   routes back to the traditional patch-kind format.
//! - [`patch_update`] — a fresh contributor publishes a patch-series proposal
//!   via `ngit send --force-patch`; the maintainer then clones, checks out the
//!   remote-tracking `pr/<branch>(<shorthand>)` branch, commits, and `git
//!   push`es.  Asserts that the push produces another `Kind::GitPatch` event
//!   (not `KIND_PULL_REQUEST` or `KIND_PULL_REQUEST_UPDATE`) — the
//!   patch-kind-stays-patch-kind rule from `push.rs:655-658`.
//! - [`patch_update_force`] — extends `patch_update` with an amend-and-force-
//!   push step.  Asserts that the force push publishes 3 new `Kind::GitPatch`
//!   events as a revision: the first carries `["t", "root-revision"]` and an
//!   `["e", <original_root>, _, "reply"]` back-reference; the tip patch carries
//!   `["e", <revision_root>, _, "root"]` and `["e", <second_patch>, _,
//!   "reply"]`.
//! - [`patch_update_force_to_pr`] — like `patch_update_force`, but the amended
//!   commit writes a >64 KiB file so `are_commits_too_big_for_patches` returns
//!   true and the force push fires the patch→PR upgrade path.  Asserts that the
//!   result is one `KIND_PULL_REQUEST` event (not three new patches and not a
//!   `KIND_PULL_REQUEST_UPDATE`) whose `e`/`p` tags back-reference the original
//!   patch-series root and its author.
//! - [`patch_update_to_pr`] — simpler companion to `patch_update_force_to_pr`:
//!   no amend, no `-f`.  A plain fast-forward push of a single >64 KiB commit
//!   on top of an existing patch series fires the size-triggered upgrade arm in
//!   `push.rs:536-556`.  Asserts the result is one `KIND_PULL_REQUEST` event
//!   (not a `Kind::GitPatch` and not a `KIND_PULL_REQUEST_UPDATE`) and that a
//!   fresh clone advertises the single `pr/<branch>(<original_root_8>)` ref
//!   resolving to the new commit OID.
//!
//! When adding a new scenario file, declare it as another `mod` below
//! and follow the same fixture / case shape so failures stay
//! pinpoint-named in `cargo test` output.

mod ff_update;
mod force_update_stale_origin_main;
mod new_pr;
mod new_pr_custom_subject_desc;
mod patch_kind_when_no_grasp;
mod patch_update;
mod patch_update_force;
mod patch_update_force_to_pr;
mod patch_update_to_pr;
mod rebase_update;
