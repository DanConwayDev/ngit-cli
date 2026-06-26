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
//! - [`push_tag`] — pushes a lightweight *and* an annotated tag through the
//!   nostr remote. Asserts (negatively) that no `refs/remotes/origin/<tag>` ref
//!   is written on the publisher or a fresh nostr clone — git's remote-tracking
//!   *branch* namespace, where a stray tag entry shows up as a remote branch in
//!   `git branch -r` — and (positively) that both tags reach the publisher's
//!   `refs/tags/*`, both grasps' bare repos, a nostr clone, and the kind-30618
//!   state event (annotated tags as a tag object with the `^{}` peel;
//!   lightweight tags as a bare commit oid). Regression cover for `fix: tag
//!   pushed via nostr remote appeared as remote branch`.
//! - [`clone_interact_tag`] — a fresh user clones over `nostr://` *after* the
//!   tags were pushed, then exercises them end-to-end: `git fetch --tags
//!   --prune` keeps both tags, `git cat-file -t` confirms the annotated tag is
//!   a real tag object that peels to the seed commit, and a maintainer pushes a
//!   brand-new tag back from the fresh checkout (reaching the state event and
//!   the grasp's bare repo without a stray remote-tracking ref). The tight pin
//!   on the annotated tag's `^{}` peel lives in [`push_tag`]; this scenario
//!   covers the cloner-consumes-and-pushes side.
//! - [`auto_accept_maintainership`] — a user listed in another maintainer's
//!   announcement, but without their own kind-30617 yet, clones the repo and
//!   pushes a normal branch. Asserts the push path auto-publishes their
//!   co-maintainer announcement and records the branch in a state event signed
//!   by that co-maintainer.
//!
//! When adding a new scenario file, declare it as another `mod` below
//! and follow the same fixture / case shape so failures stay
//! pinpoint-named in `cargo test` output.

mod add_branch;
mod auto_accept_maintainership;
mod clone_interact_tag;
mod delete_branch;
mod fresh_repo;
mod push_tag;
