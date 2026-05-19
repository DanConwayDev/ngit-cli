//! High-level scenario builders used by integration tests.
//!
//! These helpers wrap multi-step setups that recur across the test suite so
//! a test reads as "publish a repo; clone it as a contributor; assert ..."
//! rather than 50 lines of per-step `ngit account create` / `git commit` /
//! `ngit init` / `git push` choreography.
//!
//! Today there are two entry points, both as methods on [`Harness`]:
//!
//! - [`Harness::publish_repo`] — spin up a maintainer identity, commit a seed
//!   file, run `ngit init` against the first registered grasp server, then `git
//!   push` so the announcement graduates out of the grasp's purgatory and the
//!   bare repo gets refs. Returns the maintainer's local working tree plus a
//!   [`PublishedRepo`] carrying everything subsequent helpers need.
//! - [`Harness::clone_published_repo`] — `git clone` the announced `nostr://`
//!   URL into a fresh repo and, optionally, log in locally — either as the
//!   maintainer (reusing the published nsec) or as a brand new account (a
//!   "contributor"). Returns a [`Repo`] pointing at the cloned working tree,
//!   ready to drive further ngit commands.
//!
//! Together they cover the "maintainer publishes / contributor clones"
//! shape that most send / pr / fetch tests want. Helpers that need
//! something more bespoke can compose the lower-level [`Repo`] /
//! [`Harness`] primitives directly.
//!
//! Layered on top:
//!
//! - [`Harness::publish_pr`] / [`Harness::publish_patch_series`] — mint a fresh
//!   contributor clone of a [`PublishedRepo`], commit some files on a branch,
//!   and run `ngit send` with **explicit `--force-pr` / `--force-patch`** to
//!   pin the produced event kind. The forced kind is the entire point: every
//!   test consuming "an open proposal" as a precondition must remain green when
//!   the default-kind heuristic in `src/bin/ngit/sub_commands/send.rs:236-243`
//!   evolves underneath it (see `docs/architecture/test-harness-migration.md` §
//!   "Scenario builders").
//! - [`Harness::publish_three_open_proposals`] — `cli_tester_create_proposals`
//!   replacement. Mints one contributor identity and publishes three PRs from
//!   it on `feature-1`/`feature-2`/`feature-3`, with the same `{prefix}3.md` /
//!   `{prefix}4.md` commit shape as legacy so anyone diffing legacy-vs-new can
//!   match commits one-to-one.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;

use crate::{clock, harness::Harness, repo::Repo};

/// `KIND_PULL_REQUEST` from `src/lib/git_events.rs`. Mirrored locally so the
/// harness doesn't pull in the ngit lib crate just for one number. Kept in
/// sync by hand — if `src/` ever renumbers PR events both sides must move.
const KIND_PULL_REQUEST: Kind = Kind::Custom(1618);

/// `STATE_KIND` from `src/lib/client.rs:2381` — kind 30618. Same mirroring
/// rule as `KIND_PULL_REQUEST`: hand-synced rather than imported so the
/// harness crate keeps a small dep tree.
const STATE_KIND: Kind = Kind::Custom(30618);

/// Knobs for [`Harness::publish_repo`]. All fields are optional and
/// defaults match what unit-style tests usually want.
#[derive(Clone, Debug, Default)]
pub struct PublishRepoOpts {
    /// `--name` passed to both `ngit account create` and `ngit init`.
    /// Defaults to `"ngit test maintainer"`.
    pub display_name: Option<String>,
    /// `--identifier` passed to `ngit init`. Defaults to
    /// `"ngit-test-repo"`.
    pub identifier: Option<String>,
    /// Filename + content for the seed commit. Defaults to
    /// `README.md` with the body `"hello, ngit!\n"`. The file is created
    /// before `ngit init` so HEAD has an oid for libgit2 to read.
    pub initial_file: Option<(String, String)>,
    /// Number of *additional* co-maintainers (beyond the publisher) to
    /// list in the kind-30617 announcement. The harness mints that many
    /// fresh [`Keys`] and passes their npubs to `ngit init` via
    /// `--other-maintainers`. The resulting maintainer list on the
    /// announcement is `[publisher, extra-1, extra-2, ...]`, matching
    /// the order `init.rs` assembles it (see
    /// `src/bin/ngit/sub_commands/init.rs:880-892`).
    ///
    /// Defaults to `0` (single-maintainer announcement — the publisher).
    /// The minted keys are surfaced on
    /// [`PublishedRepo::additional_maintainer_keys`] so tests can assert
    /// on per-co-maintainer `p` / `a` tags downstream. The co-maintainers
    /// themselves do **not** need to sign anything: an npub in the
    /// `maintainers` tag is enough to make ngit treat that pubkey as a
    /// maintainer for tag-generation purposes.
    pub additional_maintainer_count: usize,
}

/// Metadata about a repository that has been published to the grasp via
/// [`Harness::publish_repo`], sufficient to clone it again or to reason
/// about the events it produced.
#[derive(Clone, Debug)]
pub struct PublishedRepo {
    /// Maintainer's full keypair. Useful when a test wants to sign
    /// arbitrary events as the maintainer.
    pub maintainer_keys: Keys,
    /// Maintainer's nsec in bech32 form — exactly the value found in the
    /// publisher's `.git/config` `nostr.nsec` key, ready to pass to
    /// `ngit account login --local --nsec ...`.
    pub maintainer_nsec: String,
    /// Maintainer's npub in bech32 form.
    pub maintainer_npub: String,
    /// `--identifier` that the announcement was published with.
    pub identifier: String,
    /// `--name` that the announcement was published with.
    pub display_name: String,
    /// `nostr://<npub>/<grasp>/<identifier>` URL exactly as printed by
    /// `ngit init` — pass straight to `git clone`.
    pub clone_url: String,
    /// Commit oid of `refs/heads/main` after the initial seed commit.
    /// Use to assert that a later clone resolves to the same tree.
    pub initial_oid: String,
    /// Keypairs for the *additional* co-maintainers minted by
    /// [`PublishRepoOpts::additional_maintainer_count`], in the order they
    /// were minted — same order they appear on the announcement's
    /// `maintainers` tag after the publisher. Empty when
    /// `additional_maintainer_count == 0`.
    ///
    /// Surfaces full [`Keys`] (not just pubkeys) so tests that want to
    /// sign events *as* a co-maintainer (e.g. status-event regressions,
    /// permission tests) can do so without re-deriving. The simpler
    /// `pubkey` accessor is `keys.public_key()`.
    pub additional_maintainer_keys: Vec<Keys>,
    /// Monotonic source for default `feature-{n}` branch names handed out by
    /// [`Harness::publish_pr`] and [`Harness::publish_patch_series`]. Shared
    /// across [`Clone`]s so two helpers operating on copies of the same
    /// `PublishedRepo` still hand out distinct branch names. Private — tests
    /// pass `branch: Some(...)` explicitly when they care about the name.
    pub(crate) feature_counter: Arc<AtomicU32>,
}

/// What [`Harness::clone_published_repo`] should do *after* the clone
/// finishes.
#[derive(Clone, Debug, Default)]
pub enum CloneLogin {
    /// Leave the cloned repo logged out. Useful when the test drives every
    /// subsequent ngit invocation with explicit `--nsec` flags or doesn't
    /// need to sign anything.
    #[default]
    None,
    /// Run `ngit account login --local --nsec <published.maintainer_nsec>`
    /// so the cloned repo signs as the original maintainer.
    AsMaintainer,
    /// Run `ngit account create --local --name <display_name>` so the
    /// cloned repo signs as a fresh contributor identity. Models the most
    /// common send-a-proposal flow: someone other than the maintainer
    /// clones, makes a branch, and runs `ngit send`.
    AsContributor {
        /// `--name` for the new contributor account. Has no effect on the
        /// underlying nostr operations; only the metadata event content
        /// reflects it.
        display_name: String,
    },
}

impl Harness {
    /// Maintainer-side setup: mint an account, commit a seed file, announce
    /// the repo via `ngit init`, then push so the grasp graduates the
    /// announcement out of purgatory. Returns the maintainer's local
    /// working tree (kept alive for any follow-up the test wants) and the
    /// metadata needed to clone the repo from a fresh tempdir.
    ///
    /// Requires at least one `with_grasp_server(...)` registered on the
    /// harness — the first grasp's `http://...` URL is passed as
    /// `--grasp-server`. A user-relay (`with_relay("default")`) is also
    /// required because `ngit account create` publishes kind 0 / 10002 to
    /// the user's default-set; without one ngit-grasp would reject those
    /// events and `account create` would fail.
    pub async fn publish_repo(&self, opts: PublishRepoOpts) -> Result<(Repo, PublishedRepo)> {
        let publisher = self.fresh_repo()?;

        let display_name = opts
            .display_name
            .unwrap_or_else(|| "ngit test maintainer".to_string());
        let identifier = opts
            .identifier
            .unwrap_or_else(|| "ngit-test-repo".to_string());
        let (seed_filename, seed_content) = opts
            .initial_file
            .unwrap_or_else(|| ("README.md".to_string(), "hello, ngit!\n".to_string()));

        // --- 1. account create ------------------------------------------------
        //
        // `--local` writes the new nsec into the publisher's `.git/config`
        // rather than the global config, so subsequent ngit commands run
        // from this repo authenticate as this account automatically.
        check_ok(
            "ngit account create",
            publisher
                .ngit(["account", "create", "--local", "--name", &display_name])
                .output()
                .await
                .context("failed to spawn ngit account create")?,
        )?;

        let nsec = publisher
            .config("nostr.nsec")
            .await?
            .context("nostr.nsec missing from local git config after `ngit account create`")?;
        let keys = Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")?;
        let pubkey = keys.public_key();
        let npub = pubkey
            .to_bech32()
            .context("failed to bech32-encode the new account's public key")?;

        // --- 2. seed commit ---------------------------------------------------
        //
        // libgit2's `get_head_commit` (called inside `ngit init`) needs a
        // resolved HEAD. The simplest way to produce one is a single
        // non-empty commit so the on-disk tree is also testable.
        std::fs::write(publisher.dir().join(&seed_filename), &seed_content).with_context(|| {
            format!("failed to write seed file {seed_filename} in publisher repo")
        })?;
        check_ok(
            "git add",
            publisher
                .git(["add", &seed_filename])
                .output()
                .await
                .context("failed to spawn git add")?,
        )?;
        check_ok(
            "git commit",
            publisher
                .git(["commit", "-m", "initial", "--no-gpg-sign"])
                .output()
                .await
                .context("failed to spawn git commit")?,
        )?;

        let snapshot = publisher.snapshot()?;
        let initial_oid = snapshot
            .refs
            .get("refs/heads/main")
            .context("refs/heads/main missing after initial commit")?
            .clone();

        // --- 3. ngit init -----------------------------------------------------
        //
        // `--grasp-server` adds the grasp as both a git server and a
        // repo-relay tag inside the kind 30617 announcement. `-d` opts the
        // form into defaults — there are no interactive prompts to drive
        // in the new harness.
        //
        // `--other-maintainers <npub>...` injects the freshly-minted
        // co-maintainer pubkeys into the announcement's `maintainers` tag.
        // The init code path that consumes this lives at
        // `src/bin/ngit/sub_commands/init.rs:880-892` — when the flag is
        // present (or running non-interactively, as here), `base_maintainers`
        // is taken straight from it without any prompt. The co-maintainer
        // keypairs need not sign anything; an npub in the maintainers tag
        // is enough for ngit to emit per-maintainer `p` / `a` tags on
        // subsequent patches.
        let additional_maintainer_keys: Vec<Keys> = (0..opts.additional_maintainer_count)
            .map(|_| Keys::generate())
            .collect();
        let additional_maintainer_npubs: Vec<String> = additional_maintainer_keys
            .iter()
            .map(|k| {
                k.public_key()
                    .to_bech32()
                    .expect("nostr pubkeys always bech32-encode")
            })
            .collect();

        let grasp_url = self.grasp("repo").url().to_string();
        let mut init_args: Vec<String> = vec![
            "init".into(),
            "--name".into(),
            display_name.clone(),
            "--identifier".into(),
            identifier.clone(),
            "--grasp-server".into(),
            grasp_url,
            "-d".into(),
        ];
        if !additional_maintainer_npubs.is_empty() {
            init_args.push("--other-maintainers".into());
            for npub in &additional_maintainer_npubs {
                init_args.push(npub.clone());
            }
        }
        let init = publisher
            .ngit(init_args)
            .output()
            .await
            .context("failed to spawn ngit init")?;
        if !init.status.success() {
            bail!(
                "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
                init.status,
                String::from_utf8_lossy(&init.stdout),
                String::from_utf8_lossy(&init.stderr),
            );
        }

        let init_stdout = String::from_utf8_lossy(&init.stdout);
        let clone_url = extract_clone_url(&init_stdout).with_context(|| {
            format!(
                "no `clone url:` line in ngit init stdout — has the print format \
                 changed?\nfull stdout was:\n{init_stdout}"
            )
        })?;

        // --- 4. push to graduate the announcement -----------------------------
        //
        // Without this push the kind 30617 stays in ngit-grasp's purgatory
        // (see init.rs:1195 short-circuit under NGITTEST=TRUE). Pushing
        // streams pack data over smart-http and publishes the state event,
        // both of which graduate the announcement into the relay DB. After
        // this point a fresh `git clone` of the nostr:// URL works.
        //
        // `Repo::nostr_push` (rather than a raw `git push`) is mandatory
        // here because the push emits a kind-30618 state event — see its
        // doc-comment for the timing rule, and `crate::clock` for the
        // root-cause writeup.
        publisher
            .nostr_push(["-u", "origin", "main"])
            .await
            .context("git push -u origin main (publish_repo graduation)")?;

        Ok((
            publisher,
            PublishedRepo {
                maintainer_keys: keys,
                maintainer_nsec: nsec,
                maintainer_npub: npub,
                identifier,
                display_name,
                clone_url,
                initial_oid,
                additional_maintainer_keys,
                feature_counter: Arc::new(AtomicU32::new(1)),
            },
        ))
    }

    /// `git clone` an already-[`publish_repo`]ed repository into a fresh
    /// repo and, optionally, log in locally.
    ///
    /// The returned [`Repo`] points at the cloned working tree (not the
    /// publisher's), with the harness env, augmented `PATH`, and a
    /// per-repo identity ready for further commits. All `ngit` /
    /// `git` commands spawned through it run inside the clone.
    ///
    /// [`publish_repo`]: Harness::publish_repo
    pub async fn clone_published_repo(
        &self,
        published: &PublishedRepo,
        login: CloneLogin,
    ) -> Result<Repo> {
        let clone = Repo::clone(self, &published.clone_url).await?;

        match login {
            CloneLogin::None => {}
            CloneLogin::AsMaintainer => {
                check_ok(
                    "ngit account login --local --nsec ... (as maintainer)",
                    clone
                        .ngit([
                            "account",
                            "login",
                            "--local",
                            "--nsec",
                            &published.maintainer_nsec,
                        ])
                        .output()
                        .await
                        .context("failed to spawn ngit account login")?,
                )?;
            }
            CloneLogin::AsContributor { display_name } => {
                check_ok(
                    "ngit account create --local (as contributor)",
                    clone
                        .ngit(["account", "create", "--local", "--name", &display_name])
                        .output()
                        .await
                        .context("failed to spawn ngit account create")?,
                )?;
            }
        }

        Ok(clone)
    }

    /// Publish a single **PR-kind** proposal against `repo` from a fresh
    /// contributor identity.
    ///
    /// Drives:
    ///
    /// 1. `clone_published_repo` with `CloneLogin::AsContributor` so the
    ///    proposal is signed by a brand-new account — the canonical "someone
    ///    other than the maintainer submits a proposal" shape.
    /// 2. `git checkout -b <branch>` from `main`.
    /// 3. One commit per `(filename, content)` in `opts.commits`. Default is
    ///    two commits adding `t3.md` and `t4.md`, matching the legacy
    ///    `cli_tester_create_proposals` shape (with the `a`/`b`/`c` prefix
    ///    substituted by `t`) so anyone diffing legacy vs new can pair commits
    ///    one-to-one.
    /// 4. `ngit send HEAD~N --force-pr --title <t> --description <d>
    ///    [--in-reply-to ...]`.
    /// 5. Re-query the grasp's repo-relay surface to confirm the
    ///    `KIND_PULL_REQUEST` event for the new branch actually landed —
    ///    catches "we changed `--force-pr` to do nothing" at
    ///    scenario-construction time rather than at first test assertion.
    ///
    /// **`--force-pr` is mandatory and load-bearing.** Without it ngit's
    /// commit-size heuristic in `src/bin/ngit/sub_commands/send.rs:236-243`
    /// decides the kind from the patch payload size and submodule
    /// presence; a future refactor of that heuristic would silently re-kind
    /// every test consuming "an open proposal" as a precondition. Forcing
    /// the kind here is precisely the coupling the migration is meant to
    /// break — see `docs/architecture/test-harness-migration.md` §
    /// "Force-flag discipline". The corresponding `git push -u
    /// pr/<branch>` PR-creation path lives in a tiny named group of
    /// hand-rolled push tests; it is never reached through this builder.
    ///
    /// The contributor identity is fresh per call; tests that need a
    /// shared author across multiple proposals should reach for
    /// [`Harness::publish_three_open_proposals`] (or its patch sibling) or
    /// compose the lower-level [`Harness::clone_published_repo`] +
    /// `ngit send` themselves.
    pub async fn publish_pr(
        &self,
        repo: &PublishedRepo,
        opts: PublishPrOpts,
    ) -> Result<PublishedPr> {
        let clone = self
            .clone_published_repo(
                repo,
                CloneLogin::AsContributor {
                    display_name: "ngit test contributor".to_string(),
                },
            )
            .await?;
        self.publish_pr_in_clone(&clone, repo, opts).await
    }

    /// Publish a single **patch-series** proposal against `repo` from a
    /// fresh contributor identity.
    ///
    /// Same flow as [`Harness::publish_pr`] except step 4 runs
    /// `ngit send HEAD~N --force-patch ...` (with `--no-cover-letter` when
    /// `opts.cover_letter` is `None`).
    ///
    /// **`--force-patch` is mandatory and load-bearing** for the same
    /// reason `--force-pr` is mandatory in
    /// [`Harness::publish_pr`]: pinning the kind here decouples every
    /// test asserting on patch behaviour from the default-kind heuristic.
    /// See `docs/architecture/test-harness-migration.md` § "Force-flag
    /// discipline".
    pub async fn publish_patch_series(
        &self,
        repo: &PublishedRepo,
        opts: PublishPatchSeriesOpts,
    ) -> Result<PublishedPatchSeries> {
        let clone = self
            .clone_published_repo(
                repo,
                CloneLogin::AsContributor {
                    display_name: "ngit test contributor".to_string(),
                },
            )
            .await?;
        self.publish_patch_series_in_clone(&clone, repo, opts).await
    }

    /// `cli_tester_create_proposals` replacement: publish three PR-kind
    /// proposals on top of `repo`, all authored by the **same** fresh
    /// contributor identity.
    ///
    /// Three PRs because the legacy fetch / list / pr_checkout tests
    /// assumed a non-trivial open-proposal set (to exercise filtering,
    /// ordering, ref-naming) without caring which kind each is. Each PR
    /// has two commits matching the legacy shape: file names are
    /// `{prefix}3.md` / `{prefix}4.md` where `prefix` is `"a"`, `"b"`,
    /// `"c"` for the first, second, third proposal respectively. Branches
    /// are `feature-1`, `feature-2`, `feature-3` (no `pr/` prefix — we
    /// drive `ngit send`, not `git push pr/<branch>`).
    ///
    /// Inherits the [`Harness::publish_pr`] `--force-pr` discipline: each
    /// proposal is locked to `KIND_PULL_REQUEST` regardless of how the
    /// default-kind heuristic evolves. A patch-kind sibling
    /// (`publish_three_open_patch_proposals`) will exist for the narrow
    /// case where tests pin patch behaviour — it lands in the migration PR
    /// that first needs it.
    pub async fn publish_three_open_proposals(
        &self,
        repo: &PublishedRepo,
    ) -> Result<[PublishedPr; 3]> {
        // One shared contributor across all three proposals — the legacy
        // helper minted one repo and submitted from it, and downstream
        // tests assume a single author for filter / list / merge flows.
        let clone = self
            .clone_published_repo(
                repo,
                CloneLogin::AsContributor {
                    display_name: "ngit test contributor".to_string(),
                },
            )
            .await?;

        let mut prs: Vec<PublishedPr> = Vec::with_capacity(3);
        for (idx, prefix) in ["a", "b", "c"].iter().enumerate() {
            let n = idx + 1;
            let pr = self
                .publish_pr_in_clone(
                    &clone,
                    repo,
                    PublishPrOpts {
                        branch: Some(format!("feature-{n}")),
                        commits: vec![
                            (format!("{prefix}3.md"), "some content\n".to_string()),
                            (format!("{prefix}4.md"), "some content\n".to_string()),
                        ],
                        title: format!("proposal {prefix}"),
                        description: format!("proposal {prefix} description"),
                        in_reply_to: vec![],
                    },
                )
                .await
                .with_context(|| format!("publishing proposal #{n} (prefix {prefix:?})"))?;
            prs.push(pr);
        }

        // `[T; 3]::try_from(Vec<T>)` consumes the vec, fails only on
        // length mismatch — we just pushed three so it's infallible.
        prs.try_into().map_err(|v: Vec<PublishedPr>| {
            anyhow::anyhow!(
                "expected exactly 3 published PRs; got {} — programmer error",
                v.len()
            )
        })
    }

    /// Patch-kind sibling of [`Harness::publish_three_open_proposals`].
    /// Publishes three **patch-series** proposals (with cover letters)
    /// against `repo`, all authored by the same fresh contributor
    /// identity.
    ///
    /// Exists for tests that specifically pin patch-kind behaviour —
    /// chiefly the regression coverage in `tests/pr_checkout_patch.rs`
    /// where the legacy `ngit_pr_checkout.rs` assertions depend on
    /// `checkout_patch`'s case-3/4/5 semantics (fast-forward / diverged
    /// bails / force-overwrites). The PR-kind equivalent
    /// (`checkout_pr`) takes an upstream-deferral path at
    /// `checkout.rs:247` once an upstream is set on the local branch,
    /// which a real cloned test_repo always has after the first
    /// checkout. See `tests/pr_checkout.rs`'s module-level doc-comment
    /// for the full divergence write-up.
    ///
    /// Inherits the [`Harness::publish_patch_series`] `--force-patch`
    /// discipline: each proposal is locked to `Kind::GitPatch` with a
    /// `cover-letter` patch sibling, regardless of how the default-kind
    /// heuristic in `src/bin/ngit/sub_commands/send.rs:236-243`
    /// evolves. Cover letters are included by default because the
    /// legacy `cli_tester_create_proposals` produced cover-lettered
    /// patch series.
    ///
    /// Branch names are `feature-1` / `feature-2` / `feature-3`, and
    /// each proposal has two commits named `{prefix}3.md` /
    /// `{prefix}4.md` for `prefix` in `"a"` / `"b"` / `"c"` — the same
    /// shape as the PR-kind variant so diffing the two suites is
    /// trivial.
    pub async fn publish_three_open_patch_proposals(
        &self,
        repo: &PublishedRepo,
    ) -> Result<[PublishedPatchSeries; 3]> {
        // One shared contributor across all three proposals, same as
        // the PR-kind sibling — preserves the single-author shape that
        // legacy list / filter / merge tests assumed.
        let clone = self
            .clone_published_repo(
                repo,
                CloneLogin::AsContributor {
                    display_name: "ngit test contributor".to_string(),
                },
            )
            .await?;

        let mut series: Vec<PublishedPatchSeries> = Vec::with_capacity(3);
        for (idx, prefix) in ["a", "b", "c"].iter().enumerate() {
            let n = idx + 1;
            let s = self
                .publish_patch_series_in_clone(
                    &clone,
                    repo,
                    PublishPatchSeriesOpts {
                        branch: Some(format!("feature-{n}")),
                        commits: vec![
                            (format!("{prefix}3.md"), "some content\n".to_string()),
                            (format!("{prefix}4.md"), "some content\n".to_string()),
                        ],
                        cover_letter: Some((
                            format!("proposal {prefix}"),
                            format!("proposal {prefix} description"),
                        )),
                        in_reply_to: vec![],
                    },
                )
                .await
                .with_context(|| {
                    format!("publishing patch-series proposal #{n} (prefix {prefix:?})")
                })?;
            series.push(s);
        }

        series.try_into().map_err(|v: Vec<PublishedPatchSeries>| {
            anyhow::anyhow!(
                "expected exactly 3 published patch series; got {} — programmer error",
                v.len()
            )
        })
    }

    /// Fabricate and publish a kind-30618 *state* event (`STATE_KIND` in
    /// `src/lib/client.rs:2381`) directly to a chosen relay surface — a
    /// thinner abstraction than [`Harness::publish_repo`], for tests that
    /// need to drive `git-remote-nostr list`'s state-event selection logic
    /// (`src/bin/git_remote_nostr/list.rs:55-90`) without going through
    /// `ngit push`.
    ///
    /// Three use cases this exists for, all in the migrated
    /// `tests/legacy/git_remote_nostr/list.rs` regression set:
    ///
    /// 1. **Override** the state event generated by `publish_repo`'s final `git
    ///    push`, replacing it with one that points at a different commit (e.g.
    ///    claim `refs/heads/main` is at the seed commit when the bare repo has
    ///    since advanced — legacy `when_announcement_doesnt_match_git_server`).
    /// 2. Publish a state event whose ref→oid mapping references an OID that
    ///    does **not** exist on any git server, exercising the "fall back to
    ///    git-server state" branch (legacy
    ///    `when_state_event_references_oids_not_on_git_server`).
    /// 3. Force an older `created_at` so two competing state events on
    ///    different relays can be compared (legacy
    ///    `when_newer_relay_state_has_missing_oid_but_older_relay_state_is_resolvable`
    ///    — deferred to PR 5a along with the multi-grasp helper, but the
    ///    `created_at_offset_secs` knob is built in now so the implementation
    ///    doesn't have to change shape later).
    ///
    /// **Identifier discipline.** ngit's `RepoState::build` uses
    /// `repo_ref.identifier.clone()` as the `d` tag value — the bare
    /// identifier from the kind-30617 announcement, not the
    /// `{root_commit}-{identifier}` variant the legacy `test_utils`
    /// hardcoded. Pass [`PublishedRepo::identifier`] straight through.
    ///
    /// **Signing.** Defaults to [`PublishedRepo::maintainer_keys`] because
    /// `list.rs:64-69` filters candidate state events by
    /// `repo_ref.maintainers.contains(&event.pubkey)` — events signed by a
    /// non-maintainer are silently ignored and the test wouldn't fail in
    /// any way that points at the cause. The
    /// [`PublishStateEventOpts::signer_keys`] knob is there only for tests
    /// that specifically want a non-maintainer event present (so they can
    /// assert it's discarded).
    ///
    /// **Target.** Currently always publishes to a [`GraspServer`]'s relay
    /// surface. The harness's vanilla relays could be added as targets
    /// later, but every list-test the migration plan calls out wants the
    /// event on a grasp because that's where the kind-30617 announcement
    /// already lives (and `list.rs` reads state events from the relays
    /// listed in the announcement). A `PublishStateEventTarget::Relay(...)`
    /// variant can be added when the first consumer materialises.
    pub async fn publish_state_event(
        &self,
        repo: &PublishedRepo,
        opts: PublishStateEventOpts,
    ) -> Result<Event> {
        let keys = opts
            .signer_keys
            .unwrap_or_else(|| repo.maintainer_keys.clone());
        let identifier = opts.identifier.unwrap_or_else(|| repo.identifier.clone());

        let mut tags: Vec<Tag> = Vec::with_capacity(opts.state.len() + 1);
        tags.push(Tag::identifier(identifier.clone()));
        // HEAD must be a symref ("ref: refs/heads/<name>"); per-ref entries
        // are oid hex. The caller decides which is which — `list.rs` only
        // round-trips the values verbatim.
        for (name, value) in &opts.state {
            tags.push(Tag::custom(
                TagKind::Custom(name.clone().into()),
                vec![value.clone()],
            ));
        }

        // `created_at` defaults to "now"; `created_at_offset_secs` makes
        // the event look older by the given number of seconds, so a test
        // can publish "older resolvable" + "newer unresolvable" events
        // whose creation-time ordering is deterministic regardless of how
        // close together the two publishes run.
        let mut builder = EventBuilder::new(STATE_KIND, "").tags(tags);
        if let Some(offset) = opts.created_at_offset_secs {
            let ts = Timestamp::now() - offset;
            builder = builder.custom_created_at(ts);
        }
        let event = builder
            .sign_with_keys(&keys)
            .context("failed to sign fabricated state event")?;

        let grasp_role = opts.grasp_role.as_deref().unwrap_or("repo");
        let relay_url = self.grasp(grasp_role).relay_url();

        let client = Client::default();
        client.add_relay(&relay_url).await.with_context(|| {
            format!("failed to add grasp relay {relay_url} for state-event publish")
        })?;
        client.connect().await;
        // `send_event_to` returns an `Output` describing which relays
        // ACKed; we only added one relay so any failure on that one means
        // the publish didn't land — bail explicitly rather than silently
        // returning a never-stored event.
        let output = client
            .send_event_to([relay_url.as_str()], &event)
            .await
            .with_context(|| format!("failed to publish state event to {relay_url}"))?;
        client.disconnect().await;
        if !output.failed.is_empty() {
            bail!(
                "grasp at {relay_url} rejected state event id={}: {:?}",
                event.id,
                output.failed,
            );
        }

        // Guarantee the next event published from this harness lands in a
        // strictly later unix second. See `crate::clock::tick_to_next_second`
        // for the relay-builder quirk that makes this necessary.
        clock::tick_to_next_second().await;

        Ok(event)
    }

    /// Internal shared driver for [`Harness::publish_pr`] and
    /// [`Harness::publish_three_open_proposals`]. `clone` is assumed to be
    /// a `clone_published_repo(_, AsContributor { .. })` result — i.e. a
    /// fresh clone with `nostr.nsec` already populated for the publishing
    /// identity.
    async fn publish_pr_in_clone(
        &self,
        clone: &Repo,
        repo: &PublishedRepo,
        opts: PublishPrOpts,
    ) -> Result<PublishedPr> {
        let branch = opts.branch.unwrap_or_else(|| {
            // Monotonic across the lifetime of a `PublishedRepo` so two
            // back-to-back `publish_pr` calls with default branch don't
            // collide. Tests that care about the exact name pass
            // `Some(...)` explicitly.
            let n = repo.feature_counter.fetch_add(1, Ordering::SeqCst);
            format!("feature-{n}")
        });
        let commits = if opts.commits.is_empty() {
            vec![
                ("t3.md".to_string(), "some content\n".to_string()),
                ("t4.md".to_string(), "some content\n".to_string()),
            ]
        } else {
            opts.commits
        };
        let commit_oids = create_branch_and_commit(clone, &branch, &commits).await?;
        let tip = commit_oids
            .last()
            .cloned()
            .context("commits vec was empty after defaulting — should be unreachable")?;

        // --- ngit send --force-pr ------------------------------------------
        //
        // `--force-pr` is mandatory: see the doc-comment on `publish_pr`.
        let n = commits.len();
        let mut send_args: Vec<String> = vec![
            "send".into(),
            format!("HEAD~{n}"),
            "--force-pr".into(),
            "--title".into(),
            opts.title.clone(),
            "--description".into(),
            opts.description.clone(),
        ];
        if !opts.in_reply_to.is_empty() {
            send_args.push("--in-reply-to".into());
            for r in &opts.in_reply_to {
                send_args.push(r.clone());
            }
        }
        let send_out = clone
            .ngit(send_args)
            .output()
            .await
            .context("failed to spawn ngit send --force-pr")?;
        check_ok("ngit send --force-pr", send_out)?;

        // --- restore main so subsequent calls branch from a clean state ----
        check_ok(
            "git checkout main (after publish_pr)",
            clone
                .git(["checkout", "main"])
                .output()
                .await
                .context("failed to spawn git checkout main")?,
        )?;

        // --- verify the produced event has the expected kind ---------------
        //
        // Re-query the grasp's repo-relay over a real REQ. This is what
        // catches "we changed `--force-pr` to do nothing" at
        // scenario-construction time rather than at first test assertion.
        let author_pubkey = read_clone_pubkey(clone).await?;
        let events = self
            .grasp("repo")
            .events(Filter::new().author(author_pubkey).kind(KIND_PULL_REQUEST))
            .await?;
        let root_event = events
            .into_iter()
            .find(|e| event_branch_name_tag(e).as_deref() == Some(branch.as_str()))
            .with_context(|| {
                format!(
                    "no KIND_PULL_REQUEST event with branch-name={branch:?} authored by {author_pubkey} found \
                     on grasp `repo` after `ngit send --force-pr` — did --force-pr stop forcing?"
                )
            })?;

        Ok(PublishedPr {
            event_id: root_event.id,
            author_pubkey,
            branch_name: branch,
            commits: commit_oids,
            tip,
            root_event,
        })
    }

    /// Internal shared driver for [`Harness::publish_patch_series`].
    /// `clone` must be an `AsContributor`-logged-in clone (same precondition
    /// as [`Harness::publish_pr_in_clone`]).
    async fn publish_patch_series_in_clone(
        &self,
        clone: &Repo,
        repo: &PublishedRepo,
        opts: PublishPatchSeriesOpts,
    ) -> Result<PublishedPatchSeries> {
        let branch = opts.branch.unwrap_or_else(|| {
            let n = repo.feature_counter.fetch_add(1, Ordering::SeqCst);
            format!("feature-{n}")
        });
        let commits = if opts.commits.is_empty() {
            vec![
                ("t3.md".to_string(), "some content\n".to_string()),
                ("t4.md".to_string(), "some content\n".to_string()),
            ]
        } else {
            opts.commits
        };
        let commit_oids = create_branch_and_commit(clone, &branch, &commits).await?;
        let tip = commit_oids
            .last()
            .cloned()
            .context("commits vec was empty after defaulting — should be unreachable")?;

        // --- ngit send --force-patch ---------------------------------------
        //
        // `--force-patch` is mandatory: without it the kind ngit picks is
        // determined by `are_commits_too_big_for_patches` /
        // `do_commits_contain_submodules` / the heuristic in
        // `src/bin/ngit/sub_commands/send.rs:236-243`.
        let n = commits.len();
        let mut send_args: Vec<String> =
            vec!["send".into(), format!("HEAD~{n}"), "--force-patch".into()];
        match &opts.cover_letter {
            Some((title, description)) => {
                send_args.push("--title".into());
                send_args.push(title.clone());
                send_args.push("--description".into());
                send_args.push(description.clone());
            }
            None => send_args.push("--no-cover-letter".into()),
        }
        if !opts.in_reply_to.is_empty() {
            send_args.push("--in-reply-to".into());
            for r in &opts.in_reply_to {
                send_args.push(r.clone());
            }
        }
        let send_out = clone
            .ngit(send_args)
            .output()
            .await
            .context("failed to spawn ngit send --force-patch")?;
        check_ok("ngit send --force-patch", send_out)?;

        check_ok(
            "git checkout main (after publish_patch_series)",
            clone
                .git(["checkout", "main"])
                .output()
                .await
                .context("failed to spawn git checkout main")?,
        )?;

        // --- verify the produced events have the expected kind -------------
        //
        // Patch events fan out one-per-commit, optionally preceded by a
        // cover-letter patch carrying the `["t", "cover-letter"]` tag.
        // Querying the grasp post-send catches "we changed --force-patch
        // to do nothing" at scenario-construction time.
        //
        // `branch-name` tag discipline (per `src/lib/git_events.rs:260-271`):
        // it's only emitted on the **root** of the series — the cover
        // letter when one is requested, otherwise the first patch
        // (`thread_event_id.is_none()` branch). Per-commit patches in a
        // cover-lettered series have no `branch-name` tag; they're tied
        // to the cover letter via an `e` tag carrying the `Reply`
        // marker. Filtering by branch-name tag alone therefore picks up
        // the cover letter only and zero descendants, which is exactly
        // the bug the previous version of this verification fell into.
        // We walk the chain via the root's id instead.
        let author_pubkey = read_clone_pubkey(clone).await?;
        let all_patches: Vec<Event> = self
            .grasp("repo")
            .events(Filter::new().author(author_pubkey).kind(Kind::GitPatch))
            .await?;

        let cover_letter_event = if opts.cover_letter.is_some() {
            let cl = all_patches
                .iter()
                .find(|e| {
                    is_cover_letter(e)
                        && event_branch_name_tag(e).as_deref() == Some(branch.as_str())
                })
                .cloned();
            if cl.is_none() {
                bail!(
                    "publish_patch_series called with cover_letter=Some(...) but no \
                     `t cover-letter` patch event landed on grasp for branch-name={branch:?}"
                );
            }
            cl
        } else {
            if let Some(stray) = all_patches.iter().find(|e| {
                is_cover_letter(e) && event_branch_name_tag(e).as_deref() == Some(branch.as_str())
            }) {
                bail!(
                    "publish_patch_series called with cover_letter=None (and --no-cover-letter \
                     passed) yet a `t cover-letter` patch event arrived for \
                     branch-name={branch:?} (id={}) — did --no-cover-letter stop suppressing it?",
                    stray.id,
                );
            }
            None
        };

        // Root of the series for this branch: cover letter when there
        // is one, else the lone non-cover-letter patch that carries
        // the `branch-name` tag (the first commit in the series).
        let root_event = match &cover_letter_event {
            Some(cl) => Some(cl.clone()),
            None => all_patches
                .iter()
                .find(|e| {
                    !is_cover_letter(e)
                        && event_branch_name_tag(e).as_deref() == Some(branch.as_str())
                })
                .cloned(),
        };
        let root_id = root_event.as_ref().map(|e| e.id).with_context(|| {
            format!(
                "no root patch event for branch-name={branch:?} authored by \
                     {author_pubkey} found on grasp `repo` after `ngit send --force-patch` — \
                     did --force-patch stop forcing?"
            )
        })?;

        // Per-commit patches: every non-cover-letter GitPatch that
        // either *is* the root (no-cover-letter mode — the root is the
        // first commit) or references the root via an `e` tag.
        let patch_events: Vec<Event> = all_patches
            .iter()
            .filter(|e| !is_cover_letter(e))
            .filter(|e| e.id == root_id || event_references_via_e_tag(e, root_id))
            .cloned()
            .collect();

        if patch_events.len() != commit_oids.len() {
            bail!(
                "expected {} patch event(s) for branch-name={branch:?} after \
                 `ngit send --force-patch`; got {} (root_id={root_id})",
                commit_oids.len(),
                patch_events.len(),
            );
        }

        Ok(PublishedPatchSeries {
            author_pubkey,
            branch_name: branch,
            commits: commit_oids,
            tip,
            cover_letter_event,
            patch_events,
        })
    }
}

/// Knobs for [`Harness::publish_pr`]. `title` / `description` are mandatory
/// (no `Default` impl) — `ngit send --force-pr` will not synthesise either
/// without `--defaults`, and we deliberately don't leak the `--defaults`
/// behaviour into scenarios.
#[derive(Clone, Debug)]
pub struct PublishPrOpts {
    /// Branch name to create from `main`. Defaults to `"feature-{n}"`
    /// where `n` is monotonic per-[`PublishedRepo`]. No `pr/` prefix — the
    /// builder uses `ngit send`, not `git push pr/<branch>`, and we want
    /// the branch name to be observable in events independently of any
    /// `pr/` convention the remote helper may apply.
    pub branch: Option<String>,
    /// `(filename, content)` pairs committed one-per-commit on top of the
    /// new branch. Defaults to two commits adding `t3.md` then `t4.md`,
    /// matching the legacy `cli_tester_create_proposals` shape. Files are
    /// written to disk before `git add` so the commits' trees reflect the
    /// content.
    pub commits: Vec<(String, String)>,
    /// `--title` (clap alias for `--subject`). Mandatory: `ngit send
    /// --force-pr` requires both title and description in non-interactive
    /// mode unless `--defaults` is set, and we don't want the default to
    /// leak into scenarios.
    pub title: String,
    /// `--description`. Mandatory for the same reason as
    /// [`PublishPrOpts::title`].
    pub description: String,
    /// Optional `--in-reply-to` references (event ids / nevents / npubs /
    /// nprofiles). When non-empty, passed as
    /// `--in-reply-to <ref1> <ref2> ...` to `ngit send`. The first
    /// reference is interpreted by `ngit send` as the proposal root for
    /// revisions; subsequent references are mentions. An empty vec means
    /// no `--in-reply-to` flag is appended. Defaults to empty.
    pub in_reply_to: Vec<String>,
}

/// Outcome of [`Harness::publish_pr`].
#[derive(Clone, Debug)]
pub struct PublishedPr {
    /// Event id of the `KIND_PULL_REQUEST` root event captured back from
    /// the grasp's repo-relay.
    pub event_id: EventId,
    /// Pubkey of the contributor identity that signed the PR. Read from
    /// the cloned repo's local `nostr.nsec` after `account create`.
    pub author_pubkey: PublicKey,
    /// e.g. `"feature-1"`. Matches the `branch-name` tag on
    /// [`root_event`](PublishedPr::root_event).
    pub branch_name: String,
    /// Commit OIDs in chronological order — one entry per element of
    /// [`PublishPrOpts::commits`].
    pub commits: Vec<String>,
    /// Last commit OID; equal to `commits.last().unwrap()` but exposed
    /// directly because most assertions compare against the tip.
    pub tip: String,
    /// The `KIND_PULL_REQUEST` event itself, re-fetched from the grasp.
    /// Useful for tests that want to assert on tag shape, content body,
    /// or `created_at`.
    pub root_event: Event,
}

/// Knobs for [`Harness::publish_patch_series`]. Unlike
/// [`PublishPrOpts`], all fields are optional — `ngit send --force-patch
/// --no-cover-letter` is a valid non-interactive invocation, so the
/// scenario can default to "no cover letter, two commits, fresh branch
/// name".
#[derive(Clone, Debug, Default)]
pub struct PublishPatchSeriesOpts {
    /// See [`PublishPrOpts::branch`]. Same monotonic default.
    pub branch: Option<String>,
    /// See [`PublishPrOpts::commits`]. Same default.
    pub commits: Vec<(String, String)>,
    /// When `Some((title, description))`, the builder passes
    /// `--title <title> --description <description>` and a `t cover-letter`
    /// patch is published in addition to the per-commit patches. When
    /// `None`, the builder passes `--no-cover-letter`. The scenario asserts
    /// the chosen branch on the grasp post-send, so a regression in either
    /// direction is caught here rather than in downstream tests.
    pub cover_letter: Option<(String, String)>,
    /// Optional `--in-reply-to` references; same shape as
    /// [`PublishPrOpts::in_reply_to`]. Defaults to empty (no `--in-reply-to`
    /// flag passed).
    pub in_reply_to: Vec<String>,
}

/// Outcome of [`Harness::publish_patch_series`].
///
/// No top-level `event_id` field because a patch series has a vec of
/// `Kind::GitPatch` events plus an optional cover-letter patch — tests
/// pick whichever they want to assert on.
#[derive(Clone, Debug)]
pub struct PublishedPatchSeries {
    /// Pubkey of the signing contributor identity. Read from the cloned
    /// repo's local `nostr.nsec` after `account create`.
    pub author_pubkey: PublicKey,
    /// e.g. `"feature-1"`.
    pub branch_name: String,
    /// Commit OIDs in chronological order — one entry per
    /// `Kind::GitPatch` non-cover-letter event in
    /// [`PublishedPatchSeries::patch_events`].
    pub commits: Vec<String>,
    /// Last commit OID; equal to `commits.last().unwrap()`.
    pub tip: String,
    /// `Kind::GitPatch` event carrying the `["t", "cover-letter"]` tag, if
    /// one was requested. `None` when [`PublishPatchSeriesOpts::cover_letter`]
    /// was `None`.
    pub cover_letter_event: Option<Event>,
    /// Per-commit `Kind::GitPatch` events; ordering is whatever the grasp
    /// returns over its REQ surface (typically newest-first) and tests
    /// should not depend on a specific order.
    pub patch_events: Vec<Event>,
}

/// Knobs for [`Harness::publish_state_event`]. Every field is optional —
/// the defaults publish a maintainer-signed event with `repo.identifier` and
/// the caller-supplied state map, timestamped now.
#[derive(Clone, Debug, Default)]
pub struct PublishStateEventOpts {
    /// `ref-name → oid-hex` (for refs/heads/* and refs/tags/*) or
    /// `"HEAD" → "ref: refs/heads/<name>"` for the HEAD symref. The map
    /// is round-tripped verbatim onto the event tags — see
    /// `src/lib/repo_state.rs:53-78` for the canonical builder.
    ///
    /// Mandatory: an empty state is not a meaningful regression target,
    /// and `list.rs:79-90` would treat it as "no refs advertised", which
    /// is more easily expressed by skipping the helper entirely.
    pub state: std::collections::HashMap<String, String>,
    /// `d` tag value. Defaults to [`PublishedRepo::identifier`] — the
    /// identifier carried by the kind-30617 announcement, which is what
    /// `list.rs:48-53` matches state events against. Override only for
    /// tests that specifically need a *non-matching* identifier (none do
    /// today).
    pub identifier: Option<String>,
    /// Sign with these keys instead of [`PublishedRepo::maintainer_keys`].
    /// `list.rs:67-69` filters candidates by
    /// `repo_ref.maintainers.contains(&event.pubkey)`, so a non-maintainer
    /// signer is only useful for tests asserting "this event is
    /// discarded".
    pub signer_keys: Option<Keys>,
    /// Subtract this many seconds from `Timestamp::now()` for the
    /// event's `created_at`. Used to deterministically order two state
    /// events published back-to-back when one needs to be the "older"
    /// candidate. Defaults to `None` (= now).
    pub created_at_offset_secs: Option<u64>,
    /// Role label of the [`GraspServer`] to publish to. Defaults to
    /// `"repo"` — the role label `publish_repo` uses for the
    /// announcement's git server. Override only when the test registers
    /// multiple grasp servers under distinct labels.
    pub grasp_role: Option<String>,
}

/// `git checkout -b <branch>` from `main`, write each `(filename, content)`,
/// `git add` + `git commit -m "add <filename>" --no-gpg-sign` per pair, then
/// return the resulting chronological list of commit OIDs.
async fn create_branch_and_commit(
    clone: &Repo,
    branch: &str,
    commits: &[(String, String)],
) -> Result<Vec<String>> {
    check_ok(
        "git checkout -b <branch>",
        clone
            .git(["checkout", "-b", branch])
            .output()
            .await
            .context("failed to spawn git checkout -b")?,
    )?;

    let mut oids: Vec<String> = Vec::with_capacity(commits.len());
    for (file_name, content) in commits {
        std::fs::write(clone.dir().join(file_name), content)
            .with_context(|| format!("failed to write {file_name} in contributor clone"))?;
        check_ok(
            "git add",
            clone
                .git(["add", file_name.as_str()])
                .output()
                .await
                .context("failed to spawn git add")?,
        )?;
        check_ok(
            "git commit",
            clone
                .git(["commit", "-m", &format!("add {file_name}"), "--no-gpg-sign"])
                .output()
                .await
                .context("failed to spawn git commit")?,
        )?;

        let oid = head_oid(clone)
            .await
            .with_context(|| format!("failed to read HEAD oid after committing {file_name}"))?;
        oids.push(oid);
    }
    Ok(oids)
}

/// Resolve `HEAD` to a commit oid via `git rev-parse`. Cheaper than
/// `Repo::snapshot()` when we only care about one ref.
async fn head_oid(clone: &Repo) -> Result<String> {
    let out = clone
        .git(["rev-parse", "HEAD"])
        .output()
        .await
        .context("failed to spawn git rev-parse HEAD")?;
    if !out.status.success() {
        bail!(
            "git rev-parse HEAD exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(String::from_utf8(out.stdout)
        .context("git rev-parse HEAD returned non-utf8")?
        .trim()
        .to_string())
}

/// Pubkey of the identity logged in to `clone` — i.e. the value
/// `clone_published_repo(_, AsContributor { .. })` wrote to local
/// `nostr.nsec`.
async fn read_clone_pubkey(clone: &Repo) -> Result<PublicKey> {
    let nsec = clone
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from clone — was clone_published_repo called with a login?")?;
    let keys = Keys::parse(&nsec)
        .context("nostr.nsec in clone's local config is not a valid bech32 nsec")?;
    Ok(keys.public_key())
}

/// First value of the `["branch-name", <name>]` tag on a nostr event, if
/// present. PR and patch events both carry this; we use it to disambiguate
/// the event for one proposal from any others published in the same test.
fn event_branch_name_tag(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some("branch-name") {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

/// `true` when `event` carries `["t", "cover-letter"]` — the marker that
/// distinguishes a cover-letter patch from a regular per-commit patch in a
/// series.
fn is_cover_letter(event: &Event) -> bool {
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("t")
            && s.get(1).map(String::as_str) == Some("cover-letter")
    })
}

/// `true` when `event` has an `e` tag whose value is the hex of `target`.
///
/// Patch-series descendants reference their root (cover letter, or the
/// first patch in a no-cover-letter series) via such an `e` tag carrying
/// a `Reply` marker — see `src/lib/git_events.rs:248-258`. We only
/// inspect the value; the marker is downstream's concern.
fn event_references_via_e_tag(event: &Event, target: EventId) -> bool {
    let target_hex = target.to_hex();
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("e")
            && s.get(1).map(String::as_str) == Some(target_hex.as_str())
    })
}

/// Bail with a captured-output error message when a child process exits
/// non-zero. Keeps the assertion noise out of the body of each helper.
fn check_ok(label: &str, out: std::process::Output) -> Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }
}

/// Pull the first `nostr://...` URL printed after a `clone url:` /
/// `your clone URL:` label in `ngit init` stdout. Matches both casings
/// init.rs prints (depending on the co-maintainer code path).
///
/// Lifted verbatim from `tests/clone_grasp.rs` so the lighthouse test and
/// the scenario builder stay in lockstep.
fn extract_clone_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("clone url:") {
            let rest = line[idx + "clone url:".len()..].trim();
            if rest.starts_with("nostr://") {
                return Some(rest.to_string());
            }
        }
    }
    None
}
