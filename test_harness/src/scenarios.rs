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
use nostr::nips::{nip01::Coordinate, nip19::Nip19Coordinate};
use nostr_sdk::prelude::*;

use crate::{
    clock,
    harness::Harness,
    nostr::{KIND_PULL_REQUEST, KIND_REPO_STATE, event_branch_name_tag},
    repo::Repo,
};

/// `KIND_USER_GRASP_LIST` from `src/lib/git_events.rs:115` — kind 10317.
/// A replaceable nostr event that lists the user's preferred grasp servers
/// as `g` tags with relay URLs (`ws://`). Hand-synced rather than imported.
const KIND_USER_GRASP_LIST: Kind = Kind::Custom(10317);

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
    /// Extra `--relay <url>` arguments to pass to `ngit init`, on top of
    /// the grasp's relay URL that `apply_grasp_infrastructure`
    /// (`src/lib/repo_ref.rs:836`) prepends automatically.
    ///
    /// Each URL ends up as a relay tag on the kind-30617 announcement, so
    /// `git-remote-nostr list` (and every other ngit subcommand that
    /// resolves repo relays from the announcement) will query it for
    /// state events alongside the grasp.
    ///
    /// The intended consumer is regression tests that need a state event
    /// published to a relay the grasp does **not** gate — for example
    /// `tests/list_state.rs::uses_older_resolvable_state_event`, which
    /// publishes an unresolvable state event to a vanilla relay so it can
    /// observe `list.rs:79-90` falling back to an older-but-resolvable
    /// candidate on a different relay. Production setups also commonly
    /// list non-grasp relays in their announcements, so exercising this
    /// path is realistic.
    ///
    /// Defaults to empty: announcement carries only the grasp's relay.
    pub extra_repo_relays: Vec<String>,
    /// Additional grasp-server roles to include in the kind-30617
    /// announcement alongside the always-present `"repo"` role.
    ///
    /// Each role label is resolved via [`Harness::grasp(role)`] and its
    /// HTTP URL is appended as a further `--grasp-server <url>` argument
    /// to `ngit init`. The resulting `clone` tag on the announcement
    /// therefore lists URLs in the order `["repo", ...additional_grasp_roles]`.
    /// That iteration order is also what
    /// `push_refs_and_generate_pr_or_pr_update_event` (push.rs:735-794)
    /// uses to build its `to_try` server list, so the **first** clone URL
    /// is always the "repo" grasp's URL — both for the PR event's own
    /// `clone` tag and for the underlying `git push` target.
    ///
    /// Defaults to empty (single-grasp announcement — the `"repo"` server
    /// only). Populate when a test needs to exercise multi-grasp behaviour,
    /// e.g. verifying that every grasp in the announcement receives the PR
    /// git data even though the PR event's `clone` tag only lists the first
    /// one.
    ///
    /// Requires one `with_grasp_server(role)` call per entry on the harness
    /// builder; panics at lookup time if any role has not been registered.
    pub additional_grasp_roles: Vec<String>,
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
        // Additional grasp servers are appended in order so the kind-30617
        // `clone` tag ends up as [repo, ...additional_grasp_roles]. The
        // iteration order in push_refs_and_generate_pr_or_pr_update_event
        // (push.rs:735-794) mirrors that order, so the first clone URL
        // is always the "repo" grasp — both for the PR event's `clone` tag
        // and for which server's URL is embedded in the generated event.
        for role in &opts.additional_grasp_roles {
            let url = self.grasp(role).url().to_string();
            init_args.push("--grasp-server".into());
            init_args.push(url);
        }
        // Extra repo relays are appended *after* the grasp server is
        // already in `init_args`. `init.rs:758-770` treats `--relay` as
        // the entire announcement relay set when present, then
        // `apply_grasp_infrastructure` prepends the grasp's relay URL
        // back in — so the final announcement carries
        // `[grasp_relay, ...extras]` regardless of CLI arg order.
        for relay_url in &opts.extra_repo_relays {
            init_args.push("--relay".into());
            init_args.push(relay_url.clone());
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

    /// Fabricate and publish a kind-30618 *state* event (`KIND_REPO_STATE` in
    /// `test_harness::nostr`; mirrors `STATE_KIND` in `src/lib/client.rs`)
    /// directly to a chosen relay surface — a
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
    ///    `when_newer_relay_state_has_missing_oid_but_older_relay_state_is_resolvable`,
    ///    migrated as `tests/list_state.rs::uses_older_resolvable_state_event`
    ///    — publishes a newer-unresolvable event to a vanilla relay via
    ///    [`PublishStateEventTarget::RelayUrl`] while the grasp's
    ///    auto-generated kind-30618 from `publish_repo` plays the role of the
    ///    older-resolvable candidate).
    ///
    /// **Identifier discipline.** ngit's `RepoState::build` uses
    /// `repo_ref.identifier.clone()` as the `d` tag value — the bare
    /// identifier from the kind-30617 announcement.
    /// Pass [`PublishedRepo::identifier`] straight through.
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
    /// **Target.** Defaults to the grasp registered under role `"repo"`
    /// (i.e. the announcement's git server). Override via
    /// [`PublishStateEventOpts::target`] to publish to a different grasp
    /// or to a vanilla relay listed on the announcement via
    /// [`PublishRepoOpts::extra_repo_relays`]. Vanilla targets are the
    /// only way to land a state event whose `ref→oid` map disagrees with
    /// the bare repo, since GRASP gates kind-30618 publishes against its
    /// own git data — see the legacy
    /// `uses_older_resolvable_state_event` regression for the canonical
    /// use-case.
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
            tags.push(Tag::custom(name.clone(), vec![value.clone()]));
        }

        // Tick *before* building the event so this kind-30618's
        // `created_at` (taken from `Timestamp::now()` at sign-time) lands
        // in a strictly later unix second than any prior same-coordinate
        // replaceable event. The collision being avoided is two
        // same-coordinate replaceable events with identical
        // `(pubkey, kind, tags, content)` sharing a `created_at` second
        // — see `crate::clock` for the writeup. The tick is skipped when
        // the caller explicitly asks for a back-dated `created_at` via
        // `created_at_offset_secs`: the whole point of that knob is to
        // produce an event with a deterministically older timestamp, and
        // sleeping first would just make the test slower without
        // changing anything.
        if opts.created_at_offset_secs.is_none() {
            clock::tick_to_next_second().await;
        }

        // `created_at` defaults to "now"; `created_at_offset_secs` makes
        // the event look older by the given number of seconds, so a test
        // can publish "older resolvable" + "newer unresolvable" events
        // whose creation-time ordering is deterministic regardless of how
        // close together the two publishes run.
        let mut builder = EventBuilder::new(KIND_REPO_STATE, "").tags(tags);
        if let Some(offset) = opts.created_at_offset_secs {
            let ts = Timestamp::now() - offset;
            builder = builder.custom_created_at(ts);
        }
        let event = builder
            .finalize(&keys)
            .context("failed to sign fabricated state event")?;

        let relay_url = match opts.target {
            PublishStateEventTarget::DefaultGrasp => self.grasp("repo").relay_url(),
            PublishStateEventTarget::GraspRole(role) => self.grasp(&role).relay_url(),
            PublishStateEventTarget::RelayUrl(url) => url,
        };

        let client = Client::default();
        client
            .add_relay(&relay_url)
            .await
            .with_context(|| format!("failed to add relay {relay_url} for state-event publish"))?;
        client.connect().await;
        // `send_event_to` returns an `Output` describing which relays
        // ACKed; we only added one relay so any failure on that one means
        // the publish didn't land — bail explicitly rather than silently
        // returning a never-stored event.
        let output = client
            .send_event(&event)
            .to([relay_url.as_str()])
            .await
            .with_context(|| format!("failed to publish state event to {relay_url}"))?;
        client.disconnect().await;
        if !output.failed.is_empty() {
            bail!(
                "relay at {relay_url} rejected state event id={}: {:?}",
                event.id,
                output.failed,
            );
        }

        Ok(event)
    }

    /// Publish a `KIND_USER_GRASP_LIST` event (kind 10317) for `user_keys`,
    /// listing the ws:// relay URLs of every grasp server registered under
    /// the given `grasp_roles`.
    ///
    /// The event is published to the default relay so that subsequent ngit
    /// invocations (which fetch the user's profile from
    /// `NGIT_RELAY_DEFAULT_SET`) see the grasp list and can use those
    /// servers as personal-fork targets when all repo servers are down.
    ///
    /// ## When to call
    ///
    /// Call this after creating the contributor's account (via
    /// [`Harness::clone_published_repo`] with [`CloneLogin::AsContributor`])
    /// but before taking any repo servers offline and running `ngit send`.
    ///
    /// ## URL format
    ///
    /// ngit stores user grasp lists as `ws://` relay URLs (not `http://`
    /// clone URLs) — see `src/lib/login/user.rs:UserGraspList::to_event`.
    /// The harness uses `GraspServer::relay_url()` so the produced `g` tags
    /// match what `push.rs:617` stores when updating the grasp list
    /// interactively.
    pub async fn publish_user_grasp_list(
        &self,
        user_keys: &Keys,
        grasp_roles: &[&str],
    ) -> Result<()> {
        let tags: Vec<Tag> = grasp_roles
            .iter()
            .map(|role| {
                let ws_url = self.grasp(role).relay_url();
                Tag::custom("g", vec![ws_url])
            })
            .collect();

        clock::tick_to_next_second().await;

        let event = EventBuilder::new(KIND_USER_GRASP_LIST, "")
            .tags(tags)
            .finalize(user_keys)
            .context("failed to sign user grasp list event")?;

        let relay_url = self.relay("default").url().to_string();
        let client = Client::default();
        client.add_relay(&relay_url).await.with_context(|| {
            format!("failed to add relay {relay_url} for user grasp list publish")
        })?;
        client.connect().await;
        let output = client
            .send_event(&event)
            .to([relay_url.as_str()])
            .await
            .with_context(|| format!("failed to publish user grasp list to {relay_url}"))?;
        client.disconnect().await;
        if !output.failed.is_empty() {
            bail!(
                "relay at {relay_url} rejected user grasp list event id={}: {:?}",
                event.id,
                output.failed,
            );
        }

        Ok(())
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
    ///
    /// `BTreeMap` rather than `HashMap`: tag iteration order ends up on
    /// the signed event, so determinism matters. Tests that assert on
    /// event tag positions (or compare events by id across runs) would
    /// otherwise flake when `HashMap`'s randomised iteration changed
    /// the tag ordering between two same-coordinate publishes.
    pub state: std::collections::BTreeMap<String, String>,
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
    /// Where to publish the fabricated event. Defaults to the grasp
    /// registered under role `"repo"` — the role label `publish_repo`
    /// uses for the announcement's git server. See
    /// [`PublishStateEventTarget`] for the alternatives.
    pub target: PublishStateEventTarget,
}

/// Where [`Harness::publish_state_event`] should send the fabricated
/// kind-30618.
///
/// `list.rs:64-69` collects state-event candidates from every relay
/// listed in the kind-30617 announcement, so the target only matters
/// inasmuch as it must be a relay the announcement actually carries. In
/// practice that means either:
///
/// - a [`GraspServer`] registered on the harness — its relay URL is added to
///   the announcement automatically by [`Harness::publish_repo`] via
///   `apply_grasp_infrastructure` (`src/lib/repo_ref.rs:836`);
/// - a [`VanillaRelay`] whose URL was passed through
///   [`PublishRepoOpts::extra_repo_relays`] — the only way to land an event on
///   a relay the grasp won't gate, useful for the "newer-unresolvable vs
///   older-resolvable" topology.
#[derive(Clone, Debug, Default)]
pub enum PublishStateEventTarget {
    /// Publish to the grasp registered under role `"repo"` — the role
    /// label [`Harness::publish_repo`] uses by default.
    #[default]
    DefaultGrasp,
    /// Publish to the grasp registered under the given role label. Only
    /// useful when the test registers multiple grasp servers.
    GraspRole(String),
    /// Publish to an arbitrary relay URL — typically a
    /// [`VanillaRelay`]'s `url()`. The URL must already be in the
    /// announcement's relay list (otherwise `list.rs` will not query it
    /// and the published event is invisible to the code under test).
    RelayUrl(String),
}

// ---------------------------------------------------------------------------
// `ngit init` state-matrix arrange helpers
//
// `ngit init`'s validate_pre_fetch / validate_post_fetch chain in
// `src/bin/ngit/sub_commands/init.rs:459-585` switches on five
// distinguishable repo states (legacy ngit_init.rs::state_{a..e}):
//
// - State A "Fresh"          — no `nostr.repo` git config, no announcement
// - State B "CoordinateOnly" — `nostr.repo` set, no announcement found
// - State C "MyAnnouncement" — announcement on relays, signed by me
// - State D "co-maintainer"  — announcement signed by someone else, lists me
// - State E "not-listed"     — announcement signed by someone else, ignores me
//
// Each state has its own arrange helper rather than a single generic
// builder because the shape of "what was already there before ngit init
// runs" diverges meaningfully between them — co-maintainer and
// not-listed publish different announcements, my-announcement requires
// a maintainer-signed event on the right relays, etc. Burying that
// inside `publish_repo` would erase the per-state intent at the call
// site.
//
// State A and State B landed in PR 6a; State C lands here in 6b;
// D + E land in 6c (see `docs/architecture/test-harness-migration.md`).
// ---------------------------------------------------------------------------

/// Default seed-commit fan-out used by every `ngit init` arrange helper.
///
/// Three commits — one empty root + two file commits — so:
///
/// - The root commit (empty tree, no parents) is **distinct** from `HEAD`,
///   which makes the `r euc` tag assertion non-degenerate (legacy
///   `state_a_fresh::earliest_unique_commit_is_root` would pass even with a
///   single-commit repo, but only because EUC == HEAD; capturing the root oid
///   separately catches a regression where ngit emits HEAD's oid as the EUC).
/// - The shape mirrors `GitTestRepo::populate` (initial empty commit + `t1.md`
///   + `t2.md`), so the migrated tests assert the *same* semantic property the
///     legacy tests did, only against dynamically-captured oids instead of
///     hardcoded values.
async fn populate_init_seed_commits(repo: &Repo) -> Result<(String, String)> {
    // Initial empty commit — `--allow-empty` because no files have been
    // staged yet. Mirrors legacy `initial_commit()`.
    check_ok(
        "git commit --allow-empty (initial)",
        repo.git([
            "commit",
            "--allow-empty",
            "-m",
            "Initial commit",
            "--no-gpg-sign",
        ])
        .output()
        .await
        .context("failed to spawn git commit --allow-empty")?,
    )?;
    let root_oid = head_oid(repo)
        .await
        .context("failed to read root oid after initial commit")?;

    for (file_name, content) in [("t1.md", "some content"), ("t2.md", "some content1")] {
        std::fs::write(repo.dir().join(file_name), content)
            .with_context(|| format!("failed to write {file_name} in init-arrange repo"))?;
        check_ok(
            "git add",
            repo.git(["add", file_name])
                .output()
                .await
                .context("failed to spawn git add")?,
        )?;
        check_ok(
            "git commit",
            repo.git(["commit", "-m", &format!("add {file_name}"), "--no-gpg-sign"])
                .output()
                .await
                .context("failed to spawn git commit")?,
        )?;
    }
    let head_oid_str = head_oid(repo)
        .await
        .context("failed to read HEAD oid after seed commits")?;
    Ok((root_oid, head_oid_str))
}

/// Run `ngit account create --local --name <display>` and return the
/// resulting `(keys, nsec, npub)` triple. Shared between the State A and
/// State B arrange helpers; both want a logged-in publisher identity
/// before any state-specific setup.
async fn create_publisher_account(
    repo: &Repo,
    display_name: &str,
) -> Result<(Keys, String, String)> {
    check_ok(
        "ngit account create",
        repo.ngit(["account", "create", "--local", "--name", display_name])
            .output()
            .await
            .context("failed to spawn ngit account create")?,
    )?;
    let nsec = repo
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after `ngit account create`")?;
    let keys = Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")?;
    let npub = keys
        .public_key()
        .to_bech32()
        .context("failed to bech32-encode publisher pubkey")?;
    Ok((keys, nsec, npub))
}

/// Captured side-state for a State A "Fresh" arrange.
///
/// `ngit init` running against this repo with no flags hits
/// `validate_fresh` (init.rs:342); with `--name` + `--grasp-server` it
/// publishes a fresh announcement.
#[derive(Clone, Debug)]
pub struct ArrangedInitStateA {
    /// Publisher's keypair — useful for tests that filter the produced
    /// announcement by pubkey.
    pub keys: Keys,
    /// Bech32 nsec, exactly what `nostr.nsec` in the repo's local
    /// git-config holds. Surfaced so a test can drive `ngit` through a
    /// second clone signed by the same identity if it wants to.
    pub nsec: String,
    /// Bech32 npub.
    pub npub: String,
    /// Oid of the **root commit** (the empty initial commit) — what
    /// `r euc` should resolve to on a freshly-emitted kind-30617.
    pub root_oid: String,
    /// Oid of `HEAD` after the seed commits — distinct from `root_oid`
    /// so EUC-vs-HEAD regressions are detectable.
    pub head_oid: String,
}

/// Captured side-state for a State B "CoordinateOnly" arrange.
///
/// `ngit init` running against this repo with no flags hits the
/// CoordinateOnly branch in `validate_post_fetch` (init.rs:515-530)
/// because `nostr.repo` resolves to a coordinate but no announcement
/// exists on the relays the coordinate names. With `--force` + a grasp
/// server, ngit emits a fresh announcement whose `d` tag inherits the
/// coordinate's identifier (legacy
/// `state_b_coordinate_only::success::identifier_from_coordinate`).
#[derive(Clone, Debug)]
pub struct ArrangedInitStateB {
    /// Publisher's keypair — same shape as
    /// [`ArrangedInitStateA::keys`].
    pub keys: Keys,
    /// Bech32 nsec.
    pub nsec: String,
    /// Bech32 npub.
    pub npub: String,
    /// Root commit oid (empty initial). Same shape as
    /// [`ArrangedInitStateA::root_oid`].
    pub root_oid: String,
    /// HEAD oid after seed commits.
    pub head_oid: String,
    /// `identifier` value that the State B arrange wrote into
    /// `nostr.repo`'s coordinate. The post-init kind-30617 announcement
    /// must carry this same value as its `d` tag — the State B
    /// "inherits identifier from coordinate" contract.
    pub coordinate_identifier: String,
    /// Bech32 form of the `nostr.repo` coordinate ngit will read on
    /// the next subcommand. Captured so tests that want to assert the
    /// coordinate is *unchanged* by validate_post_fetch can do so
    /// without re-deriving the bech32.
    pub coordinate_bech32: String,
}

/// Captured side-state for a State C "MyAnnouncement" arrange.
///
/// `ngit init` running against this repo finds an existing kind-30617
/// signed by the publisher (`coordinate.pubkey == user.pubkey`) and
/// trips the `MyAnnouncement` arm in `validate_post_fetch`
/// (init.rs:531-549) — bare `ngit init` is rejected with
/// "no arguments specified" unless `--force` (or a substantive flag
/// such as `--name`) is set, and `--identifier <new>` is rejected with
/// "changing identifier creates a new repository" unless `--force` is
/// also passed.
///
/// The arrange publishes the existing announcement to the harness's
/// `"default"` relay — the same relay [`ArrangedInitStateB`] lists in
/// `nostr.repo`'s coordinate — so ngit's lookup finds it.
#[derive(Clone, Debug)]
pub struct ArrangedInitStateC {
    /// Publisher's keypair. Same shape as [`ArrangedInitStateA::keys`].
    pub keys: Keys,
    /// Bech32 nsec.
    pub nsec: String,
    /// Bech32 npub.
    pub npub: String,
    /// Root commit oid (empty initial). Same shape as
    /// [`ArrangedInitStateA::root_oid`].
    pub root_oid: String,
    /// HEAD oid after seed commits.
    pub head_oid: String,
    /// `d` tag on the existing announcement — also the identifier in
    /// the `nostr.repo` coordinate. The re-published announcement
    /// keeps this value unless `--identifier <new> --force` is passed
    /// (legacy `identifier_unchanged`).
    pub coordinate_identifier: String,
    /// Bech32 form of the `nostr.repo` coordinate ngit reads on the
    /// next subcommand.
    pub coordinate_bech32: String,
    /// The kind-30617 event the arrange published before `ngit init`
    /// runs. Tests can inspect tags on this directly without re-querying
    /// the relay.
    pub existing_announcement: Event,
    /// `name` tag value carried by [`existing_announcement`]. The
    /// re-published announcement should preserve this on `--force`
    /// (legacy `name_preserved`) or replace it when `--name <new>` is
    /// passed (legacy `name_overridden`).
    pub existing_name: String,
    /// `description` tag value carried by [`existing_announcement`].
    /// Preserved on `--force` (legacy `description_preserved`).
    pub existing_description: String,
    /// URLs in [`existing_announcement`]'s `relays` tag. Includes the
    /// harness `"default"` relay (so ngit's lookup finds the event)
    /// plus a distinctive marker URL so the
    /// `relays-survive-into-re-publish` assertion is not tautological
    /// against ngit's own default relay-set.
    pub existing_relays: Vec<String>,
    /// Marker URL distinguishable from the harness's own default relay
    /// — its presence in the re-published announcement's `relays` tag
    /// proves ngit took the relay list from the existing announcement
    /// rather than from elsewhere (legacy `relays_from_my_event`).
    pub marker_relay_url: String,
    /// Co-maintainers minted by the arrange and listed alongside the
    /// publisher in [`existing_announcement`]'s `maintainers` tag.
    /// Tests assert these pubkeys survive into the re-published
    /// announcement (legacy `maintainers_preserved`). The keypairs
    /// themselves need not sign anything — an npub in the
    /// `maintainers` tag is enough for ngit to treat the pubkey as a
    /// maintainer on republish.
    pub additional_maintainer_keys: Vec<Keys>,
}

/// Captured side-state for a State D "CoMaintainer" arrange.
///
/// `ngit init` running against this repo finds an existing kind-30617
/// signed by a *different* maintainer (`coordinate.pubkey !=
/// user_pubkey`) whose `maintainers` tag includes the publisher's
/// pubkey — the `CoMaintainer` arm in `validate_post_fetch`
/// (init.rs:551-562). No `--force` is required: bare
/// `ngit init --grasp-server <url>` is enough to publish a new
/// announcement signed by the publisher that inherits the existing
/// announcement's `name` / `description` / `web` while replacing
/// `clone` / `relays` with the publisher's own grasp infrastructure
/// and listing both publisher + selected maintainer in the
/// `maintainers` tag.
///
/// The arrange publishes the existing announcement to the harness's
/// `"default"` relay — the same relay listed in `nostr.repo`'s
/// coordinate — so ngit's lookup finds it on the first round-trip.
#[derive(Clone, Debug)]
pub struct ArrangedInitStateD {
    /// Publisher's keypair (the "me" identity that runs `ngit init`).
    /// Same shape as [`ArrangedInitStateA::keys`].
    pub keys: Keys,
    /// Bech32 nsec of the publisher.
    pub nsec: String,
    /// Bech32 npub of the publisher.
    pub npub: String,
    /// Root commit oid (empty initial). Same shape as
    /// [`ArrangedInitStateA::root_oid`].
    pub root_oid: String,
    /// HEAD oid after seed commits.
    pub head_oid: String,
    /// `d` tag on the existing announcement — also the identifier in
    /// the `nostr.repo` coordinate. The post-init announcement keeps
    /// this value as its own `d` tag (legacy state-D doesn't change
    /// identifier). The publisher signs the new announcement, so the
    /// `(pubkey, kind, d)` tuple differs from the existing one's and
    /// both events coexist on the relay.
    pub coordinate_identifier: String,
    /// Bech32 form of the `nostr.repo` coordinate ngit reads on the
    /// next subcommand. Coordinate's `public_key` comes from
    /// [`selected_maintainer_keys`](Self::selected_maintainer_keys);
    /// publisher's own pubkey is **not** in the coordinate.
    pub coordinate_bech32: String,
    /// The kind-30617 event the arrange published before `ngit init`
    /// runs. Signed by
    /// [`selected_maintainer_keys`](Self::selected_maintainer_keys),
    /// lists both the selected maintainer and the publisher in its
    /// `maintainers` tag — that is the State-D discriminator.
    pub existing_announcement: Event,
    /// `name` tag on [`existing_announcement`](Self::existing_announcement).
    /// The post-init announcement should inherit this value (legacy
    /// `name_inherited_from_other_maintainer`).
    pub existing_name: String,
    /// `description` tag on
    /// [`existing_announcement`](Self::existing_announcement).
    /// Inherited (legacy `description_inherited_from_other_maintainer`).
    pub existing_description: String,
    /// `web` tag values on
    /// [`existing_announcement`](Self::existing_announcement).
    /// Inherited (legacy `web_inherited_from_other_maintainer`); the
    /// content includes a marker substring (`"exampleproject.xyz"`) so
    /// the assertion is non-tautological against ngit's own
    /// gitworkshop default URL.
    pub existing_web: Vec<String>,
    /// Selected maintainer's git server URL on
    /// [`existing_announcement`](Self::existing_announcement). Captured
    /// so tests can assert this URL is **not** carried over into the
    /// publisher's new announcement (legacy
    /// `clone_url_from_my_grasp_server_not_theirs`). Deliberately
    /// non-grasp-format so it survives `is_my_grasp_clone_url` filtering
    /// in init.rs:718 unchanged.
    pub existing_clone_url: String,
    /// Selected maintainer's keypair — signs
    /// [`existing_announcement`](Self::existing_announcement) and
    /// appears alongside the publisher in the post-init
    /// announcement's `maintainers` tag (legacy
    /// `maintainers_is_me_and_selected`).
    pub selected_maintainer_keys: Keys,
}

/// Captured side-state for a State E "NotListed" arrange.
///
/// Same shape as [`ArrangedInitStateD`] except the existing
/// announcement's `maintainers` tag does **not** include the publisher
/// — the `NotListed` arm in `validate_post_fetch` (init.rs:564-574).
/// Bare `ngit init` (and `ngit init --defaults`) errors with
/// `"you are not listed as a maintainer"`; only `ngit init --force`
/// proceeds, after which the publisher is added to the new
/// announcement's `maintainers` tag alongside the selected maintainer.
#[derive(Clone, Debug)]
pub struct ArrangedInitStateE {
    /// Publisher's keypair (the "me" identity that runs `ngit init`).
    pub keys: Keys,
    /// Bech32 nsec of the publisher.
    pub nsec: String,
    /// Bech32 npub of the publisher.
    pub npub: String,
    /// Root commit oid (empty initial).
    pub root_oid: String,
    /// HEAD oid after seed commits.
    pub head_oid: String,
    /// `d` tag on the existing announcement — also the identifier in
    /// `nostr.repo`'s coordinate.
    pub coordinate_identifier: String,
    /// Bech32 form of `nostr.repo`. Coordinate's `public_key` comes
    /// from [`selected_maintainer_keys`](Self::selected_maintainer_keys).
    pub coordinate_bech32: String,
    /// The kind-30617 event the arrange published. Signed by
    /// [`selected_maintainer_keys`](Self::selected_maintainer_keys);
    /// lists **only** the selected maintainer in `maintainers` — that
    /// is the State-E discriminator.
    pub existing_announcement: Event,
    /// `name` tag on [`existing_announcement`](Self::existing_announcement).
    /// Inherited on `--force` (legacy
    /// `name_inherited_from_other_maintainer`).
    pub existing_name: String,
    /// `description` tag on
    /// [`existing_announcement`](Self::existing_announcement). Inherited on
    /// `--force`.
    pub existing_description: String,
    /// `web` tag values on
    /// [`existing_announcement`](Self::existing_announcement). Inherited on
    /// `--force`; carries the same marker substring as State D so the
    /// `web_inherited_from_other_maintainer` assertion is non-tautological.
    pub existing_web: Vec<String>,
    /// Selected maintainer's keypair — signs
    /// [`existing_announcement`](Self::existing_announcement) and is
    /// listed alongside the publisher in the post-`--force`
    /// announcement's `maintainers` tag (legacy
    /// `maintainers_is_me_and_selected`).
    pub selected_maintainer_keys: Keys,
}

impl Harness {
    /// State A "Fresh" arrange: a fresh repo with a logged-in publisher
    /// account and three seed commits, but **no** `nostr.repo` git
    /// config. Running `ngit init` against the returned [`Repo`]
    /// without flags reproduces legacy
    /// `state_a_fresh::errors::bare_no_flags`'s "missing required
    /// fields" error; with `--name` + `--grasp-server` it reproduces
    /// legacy `state_a_fresh::success`'s announcement publish.
    ///
    /// Requires `with_relay("default")` so `ngit account create` can
    /// publish kind 0 / 10002 to the user's default-set; no grasp is
    /// required here (the caller passes `--grasp-server` to the init
    /// invocation it drives).
    pub async fn arrange_init_state_a_fresh(&self) -> Result<(Repo, ArrangedInitStateA)> {
        let repo = self.fresh_repo()?;
        // Display name is fixed (rather than knob'd) because no migrated
        // assertion in PR 6a inspects the publisher's display-name —
        // they all assert on tags of the *announcement* event, which is
        // the next subcommand's output, not this account's metadata.
        let (keys, nsec, npub) = create_publisher_account(&repo, "ngit test maintainer").await?;
        let (root_oid, head_oid) = populate_init_seed_commits(&repo).await?;

        Ok((
            repo,
            ArrangedInitStateA {
                keys,
                nsec,
                npub,
                root_oid,
                head_oid,
            },
        ))
    }

    /// State B "CoordinateOnly" arrange: same shape as
    /// [`Harness::arrange_init_state_a_fresh`] but with `nostr.repo`
    /// pre-populated with a Nip19Coordinate that **no relay carries an
    /// announcement for**. The coordinate's pubkey is the publisher's
    /// own (so the would-be announcement is not someone else's repo —
    /// State C/D/E discriminator), and its identifier is unique enough
    /// that no fan-out from a parallel test can plausibly land an event
    /// matching it (the `root_oid` hex prefix is deterministic per-test
    /// because each harness mints fresh keys + seed commits).
    ///
    /// Running `ngit init` against the returned [`Repo`] with no flags
    /// reproduces legacy
    /// `state_b_coordinate_only::errors::bare_no_flags`'s "no
    /// announcement found for coordinate" error; with
    /// `--force --grasp-server <url>` it publishes a new announcement
    /// whose `d` tag inherits [`ArrangedInitStateB::coordinate_identifier`].
    ///
    /// The coordinate's `relays` list is set to the harness's `default`
    /// vanilla relay URL — reachable so the lookup actually completes
    /// (rather than timing out, which would mask the CoordinateOnly
    /// path), but devoid of the kind-30617 the lookup is searching for.
    pub async fn arrange_init_state_b_coordinate_only(&self) -> Result<(Repo, ArrangedInitStateB)> {
        let (repo, state_a) = self.arrange_init_state_a_fresh().await?;

        // Identifier baked from the root oid so a parallel test running
        // an arrange against the *same* default relay can't accidentally
        // collide on `(pubkey, kind, d-tag)` and surface as a stray
        // announcement. The "-coord-only" suffix mirrors legacy's
        // `-consider-it-random` shape so anyone reading the d tag in a
        // test failure immediately sees this came from a State B fixture.
        let coordinate_identifier = format!("{}-coord-only", &state_a.root_oid);

        // Default relay is reachable (so ngit's lookup actually
        // completes its REQ rather than hanging on a dead connection)
        // but won't carry a matching kind-30617 — the State B
        // discriminator. We deliberately don't list the grasp's relay
        // here: a grasp queried for the coordinate's namespace would
        // also return nothing (no announcement was published to it
        // either), but listing only the vanilla relay keeps the
        // arrange's intent unambiguous — "look here, find nothing".
        let relay_url = RelayUrl::parse(self.relay("default").url())
            .context("default relay's url is not a valid RelayUrl")?;
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate::new(Kind::GitRepoAnnouncement, state_a.keys.public_key())
                .identifier(coordinate_identifier.clone()),
            relays: vec![relay_url],
        };
        let coordinate_bech32 = coordinate
            .to_bech32()
            .context("failed to bech32-encode fabricated state-B coordinate")?;

        check_ok(
            "git config --local nostr.repo (state-B coordinate)",
            repo.git(["config", "--local", "nostr.repo", &coordinate_bech32])
                .output()
                .await
                .context("failed to spawn git config --local nostr.repo")?,
        )?;

        Ok((
            repo,
            ArrangedInitStateB {
                keys: state_a.keys,
                nsec: state_a.nsec,
                npub: state_a.npub,
                root_oid: state_a.root_oid,
                head_oid: state_a.head_oid,
                coordinate_identifier,
                coordinate_bech32,
            },
        ))
    }

    /// State C "MyAnnouncement" arrange: a State B repo plus an
    /// **already-published** kind-30617 signed by the publisher that
    /// matches `nostr.repo`'s coordinate.
    ///
    /// Running `ngit init` against the returned [`Repo`] hits the
    /// `MyAnnouncement` arm of `validate_pre_fetch` / `validate_post_fetch`
    /// (init.rs:478-499, 531-549) because `coord.coordinate.public_key ==
    /// my_pubkey`:
    ///
    /// - Bare `ngit init` errors with `"no arguments specified, use --force to
    ///   publish with new timestamp"` (legacy
    ///   `state_c_my_announcement::errors::bare_no_flags_requires_force`).
    /// - `ngit init --identifier <new>` (without `--force`) errors with
    ///   `"changing identifier creates a new repository"` (legacy
    ///   `state_c_my_announcement::errors::identifier_change_requires_force`).
    /// - `ngit init --force` republishes, preserving the existing
    ///   announcement's `name` / `description` / `relays` / `maintainers`
    ///   (legacy `state_c_my_announcement::success::force_refresh::*`).
    /// - `ngit init --name <new>` overrides the name but leaves the identifier
    ///   (legacy `state_c_my_announcement::success::name_override::*`).
    ///
    /// **Discovery.** The existing announcement is published to the
    /// harness's `"default"` relay — the same relay
    /// [`Self::arrange_init_state_b_coordinate_only`] writes into
    /// `nostr.repo`'s coordinate — so `fetching_with_report` finds it on
    /// the first round-trip. No grasp servers are required (the existing
    /// announcement's `clone` tag carries a deliberately unreachable URL;
    /// under `NGITTEST=TRUE`, init.rs:1195 short- circuits the post-init
    /// `git push` so the unreachability is invisible).
    ///
    /// **Marker relay.** `relays` on the existing announcement is
    /// `[default_relay_url, marker_relay_url]`. The marker is a
    /// non-routable `ws://ngit-test-marker.invalid:65535` — ngit's
    /// publish to it will fail silently (one-of-many relays), but the
    /// URL string survives into the re-published announcement's `relays`
    /// tag via init.rs:739-746. Asserting on the marker (rather than
    /// `default_relay_url`, which ngit might add for unrelated reasons)
    /// keeps the `relays_from_my_event` regression non-tautological.
    ///
    /// **Timestamp.** The fabricated announcement is back-dated 30s so
    /// ngit's re-publish carries a strictly greater `created_at` and
    /// the relay's replaceable-event semantics keep the newer copy.
    /// Without this the two events can collide in the same unix second
    /// — see `crate::clock` for the writeup, same root cause as the
    /// `nostr_push` timing rule.
    ///
    /// **Co-maintainer.** A single fresh [`Keys`] is minted and listed
    /// after the publisher in `maintainers`. The keypair need not
    /// sign anything; an npub in the tag is enough for ngit to keep
    /// it on republish. The full [`Keys`] is surfaced (not just the
    /// pubkey) so tests that want to drive ngit *as* the co-maintainer
    /// in a follow-up can do so without re-deriving.
    pub async fn arrange_init_state_c_my_announcement(&self) -> Result<(Repo, ArrangedInitStateC)> {
        self.arrange_init_state_c_my_announcement_with_extra_tags(vec![])
            .await
    }

    /// Same as [`Self::arrange_init_state_c_my_announcement`] but appends
    /// `extra_tags` verbatim to the fabricated kind-30617 before signing.
    ///
    /// Used by tests that need to verify ngit's round-trip behaviour for
    /// tags it doesn't itself emit — e.g. unknown tags added by a future
    /// ngit version or third-party tools. The extras land in the
    /// fabricated event's tag list *after* the ngit-known tags
    /// (`d`/`name`/`description`/`clone`/`relays`/`maintainers`), which
    /// matters for any parser whose dedup rule is "last wins" on
    /// repeated known names.
    pub async fn arrange_init_state_c_my_announcement_with_extra_tags(
        &self,
        extra_tags: Vec<Tag>,
    ) -> Result<(Repo, ArrangedInitStateC)> {
        let (repo, state_b) = self.arrange_init_state_b_coordinate_only().await?;

        let additional_maintainer_keys: Vec<Keys> = vec![Keys::generate()];

        let default_relay_url = self.relay("default").url().to_string();
        // RFC-2606 `.invalid` TLD so this never resolves, no matter what
        // happens to the test host's DNS — ngit's publish to it fails at
        // the connect stage and the test path doesn't depend on any
        // particular failure mode beyond "this URL is not the default
        // relay". Port 65535 is a no-such-service placeholder; the URL
        // is opaque to ngit's republish logic, which only round-trips
        // the string through the `relays` tag.
        let marker_relay_url = "ws://ngit-test-marker.invalid:65535".to_string();
        let existing_name = "example name".to_string();
        let existing_description = "example description".to_string();
        let existing_relays = vec![default_relay_url.clone(), marker_relay_url.clone()];

        let mut maintainers: Vec<String> = vec![state_b.keys.public_key().to_string()];
        for k in &additional_maintainer_keys {
            maintainers.push(k.public_key().to_string());
        }

        let mut tags: Vec<Tag> = vec![
            Tag::identifier(state_b.coordinate_identifier.clone()),
            // `["r", "<oid>", "euc"]` — the earliest-unique-commit
            // marker, 3-element so we can't use a standard reference tag
            // (which only allows one value).
            Tag::custom("r", vec![state_b.root_oid.clone(), "euc".to_string()]),
            Tag::custom("name", vec![existing_name.clone()]),
            Tag::custom("description", vec![existing_description.clone()]),
            // `clone` carries a deliberately unreachable URL — same
            // shape as the legacy fixture's `git:://123.gitexample.com/test`
            // — because the State C arrange doesn't drive any git
            // operation, and the `NGITTEST=TRUE` short-circuit in
            // init.rs:1195 suppresses the post-republish push that
            // would have hit it.
            Tag::custom(
                "clone",
                vec!["https://ngit-test-clone.invalid/repo.git".to_string()],
            ),
            Tag::custom("relays", existing_relays.clone()),
            Tag::custom("maintainers", maintainers),
        ];
        // Extras land *after* the ngit-known tags so that any "last wins"
        // dedup on repeated known names (e.g. an extras-provided `name`)
        // sees the extras and the existing announcement's own name in a
        // realistic order.
        tags.extend(extra_tags);

        // Back-date by 30s. Two events with `(pubkey, kind, d)`
        // matching are replaceable: the relay keeps whichever has the
        // greater `created_at`. We want the *new* announcement
        // produced by `ngit init --force` to win, so the fabricated
        // existing event needs an older timestamp. 30s is generous
        // enough that even a slow CI box's clock-skew can't flip the
        // order; the consuming tests don't care about the exact
        // delta, only that "existing predates republish".
        let created_at = Timestamp::now() - 30u64;
        let event = EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .custom_created_at(created_at)
            .finalize(&state_b.keys)
            .context("failed to sign fabricated state-C announcement")?;

        let client = Client::default();
        client
            .add_relay(&default_relay_url)
            .await
            .with_context(|| {
                format!("failed to add default relay {default_relay_url} for state-C publish")
            })?;
        client.connect().await;
        let output = client
            .send_event(&event)
            .to([default_relay_url.as_str()])
            .await
            .with_context(|| {
                format!(
                    "failed to publish state-C announcement to default relay {default_relay_url}"
                )
            })?;
        client.disconnect().await;
        if !output.failed.is_empty() {
            bail!(
                "default relay at {default_relay_url} rejected fabricated state-C \
                 announcement (id={}): {:?}",
                event.id,
                output.failed,
            );
        }

        Ok((
            repo,
            ArrangedInitStateC {
                keys: state_b.keys,
                nsec: state_b.nsec,
                npub: state_b.npub,
                root_oid: state_b.root_oid,
                head_oid: state_b.head_oid,
                coordinate_identifier: state_b.coordinate_identifier,
                coordinate_bech32: state_b.coordinate_bech32,
                existing_announcement: event,
                existing_name,
                existing_description,
                existing_relays,
                marker_relay_url,
                additional_maintainer_keys,
            },
        ))
    }

    /// State D "CoMaintainer" arrange: a State A repo plus a `nostr.repo`
    /// coordinate pointing at a **different** maintainer's pubkey **and**
    /// an already-published kind-30617 (signed by that other maintainer)
    /// whose `maintainers` tag includes the publisher.
    ///
    /// Running `ngit init` against the returned [`Repo`] hits the
    /// `CoMaintainer` arm of `validate_post_fetch` (init.rs:551-562)
    /// because `coord.coordinate.public_key != my_pubkey` *and* the
    /// existing announcement's `maintainers` tag contains `my_pubkey`:
    ///
    /// - Bare `ngit init --grasp-server <url>` succeeds (no `--force` needed —
    ///   that is the State-D vs State-E discriminator) and publishes a new
    ///   announcement signed by the publisher whose `d` tag equals the
    ///   coordinate identifier, whose `name` / `description` / `web` are
    ///   inherited from the existing announcement, whose `clone` / `relays`
    ///   come from the publisher's own grasp infrastructure (the selected
    ///   maintainer's git-server URL is **not** carried over), and whose
    ///   `maintainers` tag carries `[publisher, selected_maintainer]` — the
    ///   maintainers-default fallback in init.rs:869-878 when `my_ref.is_none()
    ///   && selected != my_pubkey`.
    /// - `ngit init` does **not** error on a missing `--name` /
    ///   `--description`: `validate_post_fetch`'s CoMaintainer arm doesn't gate
    ///   on either — `validate_fresh`'s required-fields check (init.rs:354-371)
    ///   only runs in State A.
    ///
    /// **Discovery.** The existing announcement is published to the
    /// harness's `"default"` relay — the same relay [`nostr.repo`'s
    /// coordinate carries — so `fetching_with_report` finds it on the
    /// first round-trip. No grasp servers are required for the arrange
    /// itself; the test driving `ngit init` adds `--grasp-server <url>`
    /// to provide the publisher's clone-URL infrastructure.
    ///
    /// **EUC.** The existing announcement carries the publisher's
    /// `root_oid` as its `r euc` value, mirroring the realistic case
    /// where two maintainers work on the same repo. ngit's EUC
    /// resolution in init.rs:979-990 falls through to
    /// `repo_ref.root_commit` when `my_ref` is `None`, so the post-init
    /// announcement carries the same EUC.
    ///
    /// **Timestamp.** The fabricated announcement is back-dated 30s so
    /// the publisher's new event lands in a strictly later unix second.
    /// Two events with different `(pubkey, kind, d)` tuples can coexist
    /// on a relay regardless of `created_at`, but the same-second
    /// chain-of-events flake mode in `crate::clock` still applies if
    /// the publisher republishes — back-dating keeps the arrange's
    /// timing predictable.
    ///
    /// **Selected maintainer.** A fresh [`Keys`] is minted to play the
    /// role of the selected maintainer; its public key is what
    /// `nostr.repo`'s coordinate points at, and it signs
    /// [`existing_announcement`](ArrangedInitStateD::existing_announcement).
    /// Surfacing the full keypair (not just the pubkey) means tests
    /// that want to publish further events as the selected maintainer
    /// (e.g. status events on PRs the publisher subsequently sends) can
    /// do so without re-deriving.
    pub async fn arrange_init_state_d_co_maintainer(&self) -> Result<(Repo, ArrangedInitStateD)> {
        let (repo, state_a) = self.arrange_init_state_a_fresh().await?;
        let selected = Keys::generate();
        let me_pubkey_hex = state_a.keys.public_key().to_string();
        let selected_pubkey_hex = selected.public_key().to_string();
        // Maintainers list: selected (the signer of the existing event,
        // listed first) + publisher. State-D discriminator is
        // "publisher's pubkey appears in this list".
        let maintainers_hex = vec![selected_pubkey_hex, me_pubkey_hex];
        let (
            event,
            coordinate_identifier,
            coordinate_bech32,
            existing_name,
            existing_description,
            existing_web,
            existing_clone_url,
        ) = self
            .publish_other_maintainer_announcement(
                &repo,
                &state_a,
                &selected,
                &maintainers_hex,
                "co-maintainer",
            )
            .await?;

        Ok((
            repo,
            ArrangedInitStateD {
                keys: state_a.keys,
                nsec: state_a.nsec,
                npub: state_a.npub,
                root_oid: state_a.root_oid,
                head_oid: state_a.head_oid,
                coordinate_identifier,
                coordinate_bech32,
                existing_announcement: event,
                existing_name,
                existing_description,
                existing_web,
                existing_clone_url,
                selected_maintainer_keys: selected,
            },
        ))
    }

    /// State E "NotListed" arrange: same shape as
    /// [`Self::arrange_init_state_d_co_maintainer`] except the existing
    /// announcement's `maintainers` tag carries **only** the selected
    /// maintainer — the publisher's pubkey is absent.
    ///
    /// Running `ngit init` against the returned [`Repo`] hits the
    /// `NotListed` arm of `validate_post_fetch` (init.rs:564-574):
    ///
    /// - Bare `ngit init` errors with `"you are not listed as a maintainer"`
    ///   (legacy `state_e_not_listed::errors::bare_no_flags`).
    /// - `ngit init --defaults` errors with the same message — `--defaults`
    ///   does **not** bypass the NotListed check, only `--force` does (legacy
    ///   `state_e_not_listed::errors::defaults_still_requires_force`,
    ///   regression for the `-d` shortcut accidentally short-circuiting
    ///   maintainer-list validation).
    /// - `ngit init --force --grasp-server <url>` succeeds and publishes a new
    ///   announcement signed by the publisher whose `name` / `description` /
    ///   `web` are inherited from the existing announcement and whose
    ///   `maintainers` tag carries `[publisher, selected_maintainer]` — the
    ///   same maintainers-default fallback the CoMaintainer arm uses when
    ///   `my_ref` is None.
    pub async fn arrange_init_state_e_not_listed(&self) -> Result<(Repo, ArrangedInitStateE)> {
        let (repo, state_a) = self.arrange_init_state_a_fresh().await?;
        let selected = Keys::generate();
        let selected_pubkey_hex = selected.public_key().to_string();
        // Maintainers list: selected only — publisher's pubkey is
        // deliberately absent. That is the State-E discriminator.
        let maintainers_hex = vec![selected_pubkey_hex];
        let (
            event,
            coordinate_identifier,
            coordinate_bech32,
            existing_name,
            existing_description,
            existing_web,
            _existing_clone_url,
        ) = self
            .publish_other_maintainer_announcement(
                &repo,
                &state_a,
                &selected,
                &maintainers_hex,
                "not-listed",
            )
            .await?;

        Ok((
            repo,
            ArrangedInitStateE {
                keys: state_a.keys,
                nsec: state_a.nsec,
                npub: state_a.npub,
                root_oid: state_a.root_oid,
                head_oid: state_a.head_oid,
                coordinate_identifier,
                coordinate_bech32,
                existing_announcement: event,
                existing_name,
                existing_description,
                existing_web,
                selected_maintainer_keys: selected,
            },
        ))
    }

    /// Shared implementation for the State-D / State-E arranges. Mints
    /// a kind-30617 signed by `selected` whose `maintainers` tag is
    /// exactly `maintainers_hex`, publishes it to the harness's
    /// `"default"` relay, and writes a `nostr.repo` coordinate
    /// pointing at `(selected.public_key(), <identifier>)` into the
    /// repo's local git config.
    ///
    /// `identifier_suffix` is the human-readable tail used to mint a
    /// per-fixture-unique `d` tag — `"co-maintainer"` for State D,
    /// `"not-listed"` for State E. Mirrors State B's `"-coord-only"`
    /// suffix shape so anyone reading a `d` tag in a test failure
    /// immediately sees which fixture produced it.
    ///
    /// Returns a tuple of:
    /// `(announcement_event, identifier, coordinate_bech32, name,
    ///   description, web, clone_url)` — the captured-state shape both
    /// arranges plug into their respective `Arranged*` structs.
    async fn publish_other_maintainer_announcement(
        &self,
        repo: &Repo,
        state_a: &ArrangedInitStateA,
        selected: &Keys,
        maintainers_hex: &[String],
        identifier_suffix: &str,
    ) -> Result<(Event, String, String, String, String, Vec<String>, String)> {
        let coordinate_identifier = format!("{}-{}", &state_a.root_oid, identifier_suffix);

        // Coordinate points at the *selected maintainer*, not the
        // publisher — that is what makes the next `ngit init` route
        // through validate_post_fetch's CoMaintainer / NotListed arms
        // rather than MyAnnouncement.
        let default_relay_url_str = self.relay("default").url().to_string();
        let default_relay_url = RelayUrl::parse(&default_relay_url_str)
            .context("default relay's url is not a valid RelayUrl")?;
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate::new(Kind::GitRepoAnnouncement, selected.public_key())
                .identifier(coordinate_identifier.clone()),
            relays: vec![default_relay_url],
        };
        let coordinate_bech32 = coordinate.to_bech32().context(
            "failed to bech32-encode fabricated other-maintainer-coordinate for state-D/E",
        )?;

        check_ok(
            "git config --local nostr.repo (state-D/E coordinate)",
            repo.git(["config", "--local", "nostr.repo", &coordinate_bech32])
                .output()
                .await
                .context("failed to spawn git config --local nostr.repo")?,
        )?;

        let existing_name = "example name".to_string();
        let existing_description = "example description".to_string();
        // `web` carries a marker substring (`exampleproject.xyz`) so the
        // `web_inherited_from_other_maintainer` assertion is
        // non-tautological against ngit's own gitworkshop default URL.
        let existing_web = vec![
            "https://exampleproject.xyz".to_string(),
            "https://gitworkshop.dev/123".to_string(),
        ];
        // Deliberately non-grasp-format so it survives the
        // `is_my_grasp_clone_url` filter in init.rs:718 unchanged when
        // (and if) it ever reaches `git_servers_default`. Under State D
        // `my_ref` is None, so the filter is bypassed entirely and
        // `git_servers_default = vec![]` — but capturing the URL on the
        // arrange lets the test assert "the selected maintainer's git
        // server URL did NOT leak into the new announcement" without
        // re-deriving the value (legacy
        // `clone_url_from_my_grasp_server_not_theirs`).
        //
        // RFC-2606 `.invalid` TLD so this never resolves; `clone` tags
        // are opaque to `ngit init`'s republish logic, which only
        // round-trips them through the filter.
        let existing_clone_url = "https://ngit-test-selected.invalid/repo.git".to_string();

        let mut tags: Vec<Tag> = vec![
            Tag::identifier(coordinate_identifier.clone()),
            // `["r", "<oid>", "euc"]` — earliest-unique-commit marker.
            // Use the publisher's actual root oid so EUC resolution in
            // init.rs:979-990 produces a coherent value (matches the
            // realistic "two maintainers, one repo" case).
            Tag::custom("r", vec![state_a.root_oid.clone(), "euc".to_string()]),
            Tag::custom("name", vec![existing_name.clone()]),
            Tag::custom("description", vec![existing_description.clone()]),
            Tag::custom("clone", vec![existing_clone_url.clone()]),
            Tag::custom("web", existing_web.clone()),
            Tag::custom("relays", vec![default_relay_url_str.clone()]),
            Tag::custom("maintainers", maintainers_hex.to_vec()),
        ];
        // Stable order to keep event ids deterministic per-fixture; not
        // strictly required but useful when chasing replay failures in a
        // test log.
        let _ = &mut tags; // silence "unused mut" if future edits drop the .push()

        // Back-date by 30s — same reasoning as State C: the publisher's
        // post-init announcement should carry a strictly greater
        // `created_at` (different `(pubkey, kind, d)` tuple, but
        // chronological ordering matters for the test's relay-query
        // sort). 30s is generous enough that even a slow CI box's
        // clock-skew can't flip the order.
        let created_at = Timestamp::now() - 30u64;
        let event = EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .custom_created_at(created_at)
            .finalize(selected)
            .context("failed to sign fabricated other-maintainer announcement")?;

        let client = Client::default();
        client
            .add_relay(&default_relay_url_str)
            .await
            .with_context(|| {
                format!("failed to add default relay {default_relay_url_str} for state-D/E publish")
            })?;
        client.connect().await;
        let output = client
            .send_event(&event)
            .to([default_relay_url_str.as_str()])
            .await
            .with_context(|| {
                format!(
                    "failed to publish other-maintainer announcement to default relay \
                     {default_relay_url_str}"
                )
            })?;
        client.disconnect().await;
        if !output.failed.is_empty() {
            bail!(
                "default relay at {default_relay_url_str} rejected fabricated \
                 other-maintainer announcement (id={}): {:?}",
                event.id,
                output.failed,
            );
        }

        Ok((
            event,
            coordinate_identifier,
            coordinate_bech32,
            existing_name,
            existing_description,
            existing_web,
            existing_clone_url,
        ))
    }
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
