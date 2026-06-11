//! Test fixtures for the `git` module's own `#[cfg(test)] mod tests`.
//! Kept co-located with the module being tested so the git unit tests stay
//! self-contained — see AGENTS.md § "Test harness boundary".

use std::{
    env::current_dir,
    fs,
    path::PathBuf,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use git2::{Branch, Oid, RepositoryInitOptions, Signature, Time};
use nostr::{
    Tag,
    event::FinalizeEvent,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};
use nostr::{Kind, RelayUrl, ToBech32};
use once_cell::sync::Lazy;

/// Monotonic counter combined with the process id and a per-process random
/// seed (taken from the address of a heap allocation at startup) gives us
/// collision-free temp-dir suffixes without pulling `rand` into ngit's
/// dependencies.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: Lazy<u64> = Lazy::new(|| {
        let b = Box::new(0u8);
        let addr = (&*b as *const u8) as u64;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        addr ^ nanos ^ (std::process::id() as u64)
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:016x}{:08x}", *SEED, n)
}

/// nsec used to sign the canned repo announcement event. The corresponding
/// pubkey is what `RepoRef::try_from` will see as the maintainer / author.
static TEST_KEY_1_NSEC: &str = "nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq";

static TEST_KEY_1_KEYS: Lazy<nostr::Keys> =
    Lazy::new(|| nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap());

pub static TEST_KEY_1_SIGNER: Lazy<std::sync::Arc<crate::NgitSigner>> =
    Lazy::new(|| std::sync::Arc::new(crate::NgitSigner::Keys(nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap())));

pub fn joe_signature() -> Signature<'static> {
    Signature::new("Joe Bloggs", "joe.bloggs@pm.me", &Time::new(0, 0)).unwrap()
}

/// Canned `GitRepoAnnouncement` event used by the patch / apply-chain tests
/// to build a `RepoRef`.
///
/// The `root_commit` value here matches the commit id produced by
/// `GitTestRepo::populate()` when the author/committer signature is
/// `joe_signature()` at unix time 0 — i.e. the deterministic setup used
/// throughout the git module's tests.
pub fn generate_repo_ref_event() -> nostr::Event {
    let root_commit = "9ee507fc4357d7ee16a5d8901bedcd103f23c17d";
    nostr::event::EventBuilder::new(nostr::Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier(format!("{root_commit}-consider-it-random")),
            Tag::parse(["r", root_commit]).unwrap(),
            Tag::parse(["name", "example name"]).unwrap(),
            Tag::parse(["description", "example description"]).unwrap(),
            Tag::parse(["clone", "git:://123.gitexample.com/test"]).unwrap(),
            Tag::parse(["web", "https://exampleproject.xyz", "https://gitworkshop.dev/123"]).unwrap(),
            Tag::parse(["relays", "ws://localhost:8055", "ws://localhost:8056"]).unwrap(),
            Tag::parse(["maintainers", &TEST_KEY_1_KEYS.public_key().to_string()]).unwrap(),
        ])
        .finalize(&*TEST_KEY_1_KEYS)
        .unwrap()
}

/// In-process git repository fixture used by the git module's unit tests.
pub struct GitTestRepo {
    pub dir: PathBuf,
    pub git_repo: git2::Repository,
    pub delete_dir_on_drop: bool,
}

impl Default for GitTestRepo {
    fn default() -> Self {
        // Create a repo on `main` and seed `nostr.repo` with a coordinate that
        // matches `generate_repo_ref_event()`.
        let repo_event = generate_repo_ref_event();
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: repo_event.pubkey,
                identifier: repo_event.tags.identifier().unwrap().to_string(),
            },
            relays: vec![
                RelayUrl::parse("ws://localhost:8055").unwrap(),
                RelayUrl::parse("ws://localhost:8056").unwrap(),
            ],
        };

        let repo = Self::new("main").unwrap();
        let _ = repo
            .git_repo
            .config()
            .unwrap()
            .set_str("nostr.repo", &coordinate.to_bech32().unwrap());
        repo
    }
}

impl GitTestRepo {
    pub fn new(main_branch_name: &str) -> Result<Self> {
        let path = current_dir()?.join(format!("tmpgit-{}", unique_suffix()));
        let git_repo = git2::Repository::init_opts(
            &path,
            RepositoryInitOptions::new()
                .initial_head(main_branch_name)
                .mkpath(true),
        )?;
        // Standardise diff prefix behaviour so user-level gitconfig can't
        // perturb tests.
        git_repo.config()?.set_bool("diff.mnemonicPrefix", false)?;
        Ok(Self {
            dir: path,
            git_repo,
            delete_dir_on_drop: true,
        })
    }

    pub fn initial_commit(&self) -> Result<Oid> {
        let oid = self.git_repo.index()?.write_tree()?;
        let tree = self.git_repo.find_tree(oid)?;
        let commit_oid = self.git_repo.commit(
            Some("HEAD"),
            &joe_signature(),
            &joe_signature(),
            "Initial commit",
            &tree,
            &[],
        )?;
        Ok(commit_oid)
    }

    pub fn populate(&self) -> Result<Oid> {
        self.initial_commit()?;
        fs::write(self.dir.join("t1.md"), "some content")?;
        self.stage_and_commit("add t1.md")?;
        fs::write(self.dir.join("t2.md"), "some content1")?;
        self.stage_and_commit("add t2.md")
    }

    pub fn populate_with_test_branch(&self) -> Result<Oid> {
        self.populate()?;
        self.create_branch("add-example-feature")?;
        self.checkout("add-example-feature")?;
        fs::write(self.dir.join("f1.md"), "some content")?;
        self.stage_and_commit("add f1.md")?;
        fs::write(self.dir.join("f2.md"), "some content")?;
        self.stage_and_commit("add f2.md")?;
        fs::write(self.dir.join("f3.md"), "some content1")?;
        self.stage_and_commit("add f3.md")
    }

    pub fn stage_and_commit(&self, message: &str) -> Result<Oid> {
        self.stage_and_commit_custom_signature(message, None, None)
    }

    pub fn stage_and_commit_custom_signature(
        &self,
        message: &str,
        author: Option<&git2::Signature>,
        commiter: Option<&git2::Signature>,
    ) -> Result<Oid> {
        let prev_oid = self.git_repo.head().unwrap().peel_to_commit()?;

        let mut index = self.git_repo.index()?;
        index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;

        let oid = self.git_repo.commit(
            Some("HEAD"),
            author.unwrap_or(&joe_signature()),
            commiter.unwrap_or(&joe_signature()),
            message,
            &self.git_repo.find_tree(index.write_tree()?)?,
            &[&prev_oid],
        )?;
        Ok(oid)
    }

    pub fn create_branch(&'_ self, branch_name: &str) -> Result<Branch<'_>> {
        self.git_repo
            .branch(branch_name, &self.git_repo.head()?.peel_to_commit()?, false)
            .context("could not create branch")
    }

    pub fn checkout(&self, ref_name: &str) -> Result<Oid> {
        let (object, reference) = self.git_repo.revparse_ext(ref_name)?;
        self.git_repo.checkout_tree(&object, None)?;
        match reference {
            Some(gref) => self.git_repo.set_head(gref.name().unwrap()),
            None => self.git_repo.set_head_detached(object.id()),
        }?;
        Ok(self.git_repo.head()?.peel_to_commit()?.id())
    }

    pub fn get_checked_out_branch_name(&self) -> Result<String> {
        Ok(self
            .git_repo
            .head()?
            .shorthand()
            .context("an object without a shorthand is checked out")?
            .to_string())
    }

    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.git_repo.remote(name, url)?;
        Ok(())
    }

    /// Creates a git worktree linked to this repository.
    pub fn create_worktree(&self, branch_name: &str) -> Result<GitTestRepo> {
        let worktree_path = self
            .dir
            .parent()
            .unwrap()
            .join(format!("tmpgit-worktree-{}", unique_suffix()));

        let head_commit = self.git_repo.head()?.peel_to_commit()?;
        self.git_repo
            .branch(branch_name, &head_commit, false)
            .context("failed to create branch for worktree")?;

        let worktree = self
            .git_repo
            .worktree(
                branch_name,
                &worktree_path,
                Some(
                    git2::WorktreeAddOptions::new().reference(Some(
                        &self
                            .git_repo
                            .find_branch(branch_name, git2::BranchType::Local)?
                            .into_reference(),
                    )),
                ),
            )
            .context("failed to create worktree")?;

        let worktree_repo = git2::Repository::open_from_worktree(&worktree)
            .context("failed to open repo from worktree")?;

        Ok(GitTestRepo {
            dir: worktree_path,
            git_repo: worktree_repo,
            delete_dir_on_drop: true,
        })
    }
}

impl Drop for GitTestRepo {
    fn drop(&mut self) {
        if self.delete_dir_on_drop {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
