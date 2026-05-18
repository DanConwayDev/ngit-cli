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

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;

use crate::{harness::Harness, repo::Repo};

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
        let grasp_url = self.grasp("repo").url().to_string();
        let init = publisher
            .ngit([
                "init",
                "--name",
                &display_name,
                "--identifier",
                &identifier,
                "--grasp-server",
                &grasp_url,
                "-d",
            ])
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
        check_ok(
            "git push",
            publisher
                .git(["push", "-u", "origin", "main"])
                .output()
                .await
                .context("failed to spawn git push")?,
        )?;

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
