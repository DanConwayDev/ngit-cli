//! Tests that pushing certain commits to `refs/heads/main` causes the
//! `git-remote-nostr` push pipeline to publish a `Kind::GitStatusApplied`
//! (kind 1631) event tying the merged proposal to the new commit(s).
//!
//! The producing code lives in
//! `src/bin/git_remote_nostr/push.rs::get_merged_status_events` (entry at
//! `push.rs:361`); per-proposal classification happens in
//! `get_merged_proposals_info` (`push.rs:1172`), the event shape itself is
//! built by `create_merge_status` (`push.rs:1402-1501`).
//!
//! There are **three** push-time merge paths the pipeline distinguishes, all
//! triggered by `to.eq("refs/heads/main")` in the push refspec:
//!
//! - **Merge commit** (`MergedPRCommitType::MergeCommit`): maintainer runs `git
//!   merge --no-ff` so `main`'s new tip is a true merge commit. The merge
//!   commit's parents are walked, and the parent matching a proposal's
//!   per-commit `commit` / `c` tag pins the proposal. The kind-1631 tag emitted
//!   is `["merge-commit-id", <merge-oid>]` — a single oid — plus
//!   per-merged-patch `q` tags. `push.rs:1474-1484`.
//!
//! - **Fast-forward** (`merge_pr_with_fast_forward`): maintainer runs `git
//!   merge --ff-only` so `main` advances to the proposal's tip with no merge
//!   commit. Every commit between the prior main and the new main ends up in
//!   `ahead` as a `PatchCommit { event_id }`. `applied` stays `false` and the
//!   tag name is still `merge-commit-id`, but the values are **every PR-tip
//!   commit** (parents-first via `keys().reverse()` — see `push.rs:1369-1373`).
//!   `q` tags hold one entry per merged-patch event id; for PR-kind that's the
//!   same `pr.event_id` once per matching commit, for Patch-kind it's
//!   per-commit patch event ids.
//!
//! - **Apply-as-commits** (`merge_pr_by_applying_patches`): maintainer replays
//!   the proposal's diffs with fresh commit ids (typically via `ngit apply`,
//!   which calls `git am`). The new commits don't match any patch's `commit`
//!   tag, so `get_merged_proposals_info` falls through to the per-commit
//!   author-match branch at `push.rs:1245-1278`, producing `PatchApplied {
//!   event_id }` entries. With no `MergeCommit` and at least one
//!   `PatchApplied`, the `applied` flag at `push.rs:1382-1387` flips on and the
//!   tag name switches from `merge-commit-id` to `applied-as-commits`. This
//!   path is **patch-kind only** because PR-kind events don't carry an `author`
//!   tag, so the author-match fallback can never fire for them — see
//!   `push.rs:1284-1299` (`get_patch_author`).
//!
//! Both proposal kinds are exercised where applicable. PR-kind proposals
//! push the topic branch to the git server as `refs/heads/pr/<branch>`,
//! so the maintainer reaches the proposal tip with a plain `git fetch
//! origin`. Patch-kind proposals carry their commits exclusively as
//! `Kind::GitPatch` events; `git-remote-nostr list` reconstructs the
//! commits locally and advertises them under the same long-form
//! `pr/<branch>(<8hex>)` ref name so `git fetch origin` works identically
//! from the test's point of view. See `src/bin/git_remote_nostr/list.rs:233`
//! (`make_commits_for_proposal`) for the reconstruction path.
//!
//! ## "As the maintainer" — repo choice
//!
//! `publish_repo` already returns the maintainer's local working tree with
//! `origin` pointing at the nostr:// URL, the nsec persisted in local git
//! config (so subsequent ngit invocations sign as the maintainer), and an
//! upstream wired by the post-init `nostr_push -u origin main`. That is
//! sufficient to drive a merge + push end-to-end, so each test uses it
//! directly rather than spinning up a fresh `clone_published_repo(...,
//! AsMaintainer)`. Either repo would hit the same `get_merged_status_events`
//! code path on push; the choice is purely setup-cost.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST_UPDATE, PublishPatchSeriesOpts, PublishPrOpts,
    PublishRepoOpts, PublishedPatchSeries, PublishedPr, PublishedRepo, Repo, clock,
};

// ---------------------------------------------------------------------------
// Unified proposal handle
//
// The kind-1631 status event's shape only depends on a small slice of what
// `publish_pr` / `publish_patch_series` return: the root event id (used in
// the `e/root` tag), the topic-branch name (used to derive the long-form
// `pr/<branch>(<8hex>)` ref name `list.rs` advertises), the tip oid
// (against which `merge-commit-id` for the merge-commit path is compared),
// and the set of event ids that should land in `q` tags (one per merged
// patch). Wrapping them up in `MergedProposal` lets the strategy / assertion
// helpers stay kind-agnostic.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProposalKind {
    /// `Kind::Custom(1618)` — branch pushed to git server as
    /// `refs/heads/pr/<branch>`; `git fetch origin` retrieves the commits
    /// directly from the server.
    Pr,
    /// `Kind::GitPatch` series (one patch event per commit, optionally with
    /// a cover-letter patch). Commits are reconstructed locally by
    /// `git-remote-nostr list` from the patch events.
    Patch,
}

struct MergedProposal {
    kind: ProposalKind,
    /// Event id used in the kind-1631 status event's `e/root` tag. For
    /// PR-kind that's `PublishedPr::event_id`; for Patch-kind it's the
    /// cover letter's event id (or the first patch's id when there's no
    /// cover letter — we always request a cover letter in these tests,
    /// so this is always the cover letter).
    root_event_id: EventId,
    /// Topic branch name as the contributor set it (no `pr/` prefix, no
    /// `(<8hex>)` suffix). Both kinds share the same `branch-name` tag
    /// convention.
    branch_name: String,
    /// Last commit oid of the proposal. For PR-kind: the tip pushed to
    /// `refs/heads/pr/<branch>`. For Patch-kind: the last patch's commit
    /// oid as reconstructed locally — same hex either way thanks to the
    /// deterministic reconstruction in `make_commits_for_proposal`.
    tip: String,
    /// Event ids the kind-1631 event should carry as `q` tags. For PR-kind
    /// this is just `[pr.event_id]` (it shows up `q`-tagged once per
    /// matching commit, but the test only asserts existence not multiplicity).
    /// For Patch-kind it's the per-commit patch event ids; the cover letter
    /// is **not** included because cover letters don't have a `commit` tag
    /// and so never become `merged_patches` entries in
    /// `get_merged_proposals_info`.
    expected_q_event_ids: Vec<EventId>,
}

impl MergedProposal {
    fn from_pr(pr: &PublishedPr) -> Self {
        MergedProposal {
            kind: ProposalKind::Pr,
            root_event_id: pr.event_id,
            branch_name: pr.branch_name.clone(),
            tip: pr.tip.clone(),
            expected_q_event_ids: vec![pr.event_id],
        }
    }

    fn from_patch_series(series: &PublishedPatchSeries) -> Self {
        // We always publish patch-kind series with a cover letter in these
        // tests (see `setup_patch_series`), so the root event id is the
        // cover letter's.
        // `get_proposal_and_revision_root_from_patch_or_pr_or_pr_update`
        // walks any patch's `e/root` tag back to this same id, so it's
        // also what `create_merge_status` puts in the kind-1631 event's
        // `e/root` tag.
        let root = series
            .cover_letter_event
            .as_ref()
            .expect("patch-kind setup uses cover_letter=Some so the cover letter event exists");

        // `expected_q_event_ids` is the **per-commit patch** event ids — one
        // per non-cover-letter patch. `PublishedPatchSeries::patch_events`
        // already filters cover letters out at construction (see
        // `scenarios.rs::publish_patch_series_in_clone`), so we can pass
        // it through verbatim.
        let expected_q_event_ids = series.patch_events.iter().map(|e| e.id).collect();

        MergedProposal {
            kind: ProposalKind::Patch,
            root_event_id: root.id,
            branch_name: series.branch_name.clone(),
            tip: series.tip.clone(),
            expected_q_event_ids,
        }
    }
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

struct Setup {
    /// Held only to keep the relay + grasp subprocess alive for the duration
    /// of the test. Used in assertions via `harness.grasp("repo").events(...)`.
    harness: Harness,
    /// Maintainer-published repo metadata — `published.maintainer_keys`
    /// is what signs the kind-1631 status event we assert on.
    published: PublishedRepo,
    /// Maintainer's local working tree (the one `publish_repo` returns).
    /// Has `origin` configured, the maintainer nsec in local config, and
    /// `main` checked out with upstream tracking already set.
    maintainer_repo: Repo,
    /// The proposal under test, normalised across PR / Patch kinds.
    proposal: MergedProposal,
}

/// Two-commit `feature` branch published as a `KIND_PULL_REQUEST`.
async fn setup_pr() -> Result<Setup> {
    let harness = build_harness().await?;
    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("merge-test maintainer".into()),
            identifier: Some("merge-test-repo".into()),
            ..Default::default()
        })
        .await?;

    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: Some("feature".into()),
                commits: vec![
                    ("a.md".to_string(), "alpha\n".to_string()),
                    ("b.md".to_string(), "beta\n".to_string()),
                ],
                title: "merge me".into(),
                description: "please merge".into(),
                in_reply_to: vec![],
            },
        )
        .await?;

    Ok(Setup {
        harness,
        published,
        maintainer_repo,
        proposal: MergedProposal::from_pr(&pr),
    })
}

/// Two-commit `feature` branch published as a `Kind::GitPatch` series with
/// a cover letter. The cover letter is mandatory because
/// [`MergedProposal::root_event_id`] for Patch-kind is the cover letter id;
/// tests that wanted to exercise the "first-patch-is-root" variant would need a
/// separate setup helper.
async fn setup_patch_series() -> Result<Setup> {
    let harness = build_harness().await?;
    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("merge-test maintainer".into()),
            identifier: Some("merge-test-repo".into()),
            ..Default::default()
        })
        .await?;

    let series = harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                branch: Some("feature".into()),
                commits: vec![
                    ("a.md".to_string(), "alpha\n".to_string()),
                    ("b.md".to_string(), "beta\n".to_string()),
                ],
                cover_letter: Some(("merge me".into(), "please merge".into())),
                in_reply_to: vec![],
            },
        )
        .await?;

    Ok(Setup {
        harness,
        published,
        maintainer_repo,
        proposal: MergedProposal::from_patch_series(&series),
    })
}

async fn build_harness() -> Result<Harness> {
    Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await
}

// ---------------------------------------------------------------------------
// Refs / subprocess helpers
// ---------------------------------------------------------------------------

/// `pr/<branch>(<8-hex>)` — the form `git-remote-nostr/list.rs:235-244`
/// emits for a proposal whose author differs from the current user. The
/// maintainer always falls into this branch because the proposal was
/// authored by the contributor identity `publish_pr` /
/// `publish_patch_series` minted.
fn long_branch(proposal: &MergedProposal) -> String {
    let hex = proposal.root_event_id.to_hex();
    format!("pr/{}({})", proposal.branch_name, &hex[..8])
}

async fn git_ok<I, S>(repo: &Repo, args: I, label: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn {label}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

async fn rev_parse(repo: &Repo, rev: &str) -> Result<String> {
    let out = repo
        .git(["rev-parse", rev])
        .output()
        .await
        .with_context(|| format!("failed to spawn git rev-parse {rev}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git rev-parse {rev} exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git rev-parse stdout not utf-8")?
        .trim()
        .to_string())
}

// ---------------------------------------------------------------------------
// Merge strategies
//
// Each strategy mutates the maintainer's local `main` so that a subsequent
// `nostr_push origin main` exercises a specific branch of
// `get_merged_proposals_info`. The strategy's return value is the local
// oid(s) the test should later assert against — what shape that takes
// depends on the strategy, so the return types intentionally differ.
// ---------------------------------------------------------------------------

/// `git fetch origin` then `git merge --no-ff origin/<long-pr-branch>` from
/// `main`. Produces a true merge commit (two parents: prior main tip and
/// proposal tip). Returns the merge commit's oid.
///
/// Works for **both** proposal kinds: PR-kind reaches the proposal tip via
/// the `refs/heads/pr/<branch>` ref the contributor's `ngit send --force-pr`
/// pushed to the git server; Patch-kind reaches it via
/// `make_commits_for_proposal` reconstructing the commits locally during
/// `git-remote-nostr list`.
async fn merge_pr_with_merge_commit(repo: &Repo, proposal: &MergedProposal) -> Result<String> {
    git_ok(repo, ["fetch", "origin"], "git fetch origin").await?;

    // Sanity: the remote helper advertises the proposal tip under the
    // long-form branch — if this fails the rest of the test is meaningless
    // and the error here will be much clearer than a downstream
    // "merge-commit-id != merge_oid" mismatch.
    let remote_ref = format!("origin/{}", long_branch(proposal));
    let remote_tip = rev_parse(repo, &remote_ref).await?;
    anyhow::ensure!(
        remote_tip == proposal.tip,
        "after `git fetch origin`, {remote_ref} resolved to {remote_tip}; \
         expected proposal tip {} — did list.rs stop advertising the long-form ref, \
         or did patch reconstruction produce different commit oids?",
        proposal.tip,
    );

    // `--no-ff` is load-bearing: a fast-forward from a 0-commit main onto a
    // 2-commit proposal would advance main to the proposal tip with no
    // merge commit, which is the *fast-forward* path covered by a sibling
    // test — not what this one is exercising.
    git_ok(
        repo,
        [
            "merge",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            &format!("Merge {}", long_branch(proposal)),
            &remote_ref,
        ],
        "git merge --no-ff",
    )
    .await?;

    let merge_oid = rev_parse(repo, "HEAD").await?;
    anyhow::ensure!(
        merge_oid != proposal.tip,
        "expected --no-ff to produce a merge commit distinct from the proposal \
         tip; HEAD is at proposal.tip ({merge_oid}) — git silently fast-forwarded?",
    );
    Ok(merge_oid)
}

/// `git fetch origin` then `git merge --ff-only origin/<long-pr-branch>`
/// from `main`. Advances `main` to the proposal tip with no merge commit.
/// Returns the resulting `main` tip oid (= the proposal tip) so the caller
/// can spot a silent regression to the `--no-ff` shape.
async fn merge_pr_with_fast_forward(repo: &Repo, proposal: &MergedProposal) -> Result<String> {
    git_ok(repo, ["fetch", "origin"], "git fetch origin").await?;

    let remote_ref = format!("origin/{}", long_branch(proposal));
    let remote_tip = rev_parse(repo, &remote_ref).await?;
    anyhow::ensure!(
        remote_tip == proposal.tip,
        "after `git fetch origin`, {remote_ref} resolved to {remote_tip}; \
         expected proposal tip {} — did list.rs stop advertising the long-form ref, \
         or did patch reconstruction produce different commit oids?",
        proposal.tip,
    );

    // `--ff-only` rejects anything that isn't a strict fast-forward, so a
    // future change that put a divergent commit on the maintainer's main
    // would surface here rather than as a "wrong tag name" assertion miles
    // downstream. The maintainer's main is at the seed commit (publish_repo's
    // single seed commit) and the proposal branches from that seed, so the
    // FF is always available at this point.
    git_ok(
        repo,
        ["merge", "--ff-only", "--no-gpg-sign", &remote_ref],
        "git merge --ff-only",
    )
    .await?;

    let new_main = rev_parse(repo, "HEAD").await?;
    anyhow::ensure!(
        new_main == proposal.tip,
        "expected --ff-only to advance HEAD to the proposal tip {}; got {new_main} \
         — did the merge accidentally produce a merge commit?",
        proposal.tip,
    );
    Ok(new_main)
}

/// Patch-kind apply-as-commits path: maintainer first adds an unrelated
/// commit on `main`, then runs `ngit apply <root_event_id>` which invokes
/// `git am` under the hood to replay the patches against the new `main`.
///
/// Two distinct effects keep the new commits from being misclassified as
/// `PatchCommit`:
///
/// 1. **Different parent.** The extra commit on main moves the apply-base away
///    from the seed commit, so even before `git am` rewrites the committer, the
///    parent oid of the first applied commit already differs from what the
///    patch event's `parent-commit` tag records. Cuts the dependency on
///    libgit2's exact handling of committer rewrites — even if `git am`
///    happened to preserve the committer (it doesn't, but a future ngit
///    refactor might), the parent change alone would still produce fresh oids.
///
/// 2. **Different committer.** `git am` keeps the patch author identity (from
///    the `From:` header) but uses the current user's identity as the
///    committer. The committer participates in commit-object hashing, so the
///    resulting oids never coincide with any patch event's `commit` tag.
///
/// Together those two effects guarantee
/// `get_merged_proposals_info`'s "match by `commit` tag" branch at
/// `push.rs:1213-1244` finds no candidates for the new oids, and the
/// pipeline falls through to the per-commit author-match branch at
/// `push.rs:1245-1278` — which **does** match because `git am` preserved
/// the author tuple (`name`, `email`, `unixtime`, `offset`) and that's
/// exactly what `get_patch_author` reads off the patch event.
///
/// Returns the list of applied commit oids (parent-first order = order of
/// the patches). The leading "extra" commit on main is **not** included in
/// the returned vec because it's not a proposal commit; it's only there to
/// move the apply-base.
async fn apply_pr_with_ngit_apply(repo: &Repo, proposal: &MergedProposal) -> Result<Vec<String>> {
    anyhow::ensure!(
        proposal.kind == ProposalKind::Patch,
        "apply-as-commits is only valid for patch-kind proposals; PR-kind events \
         carry no `author` tag and so the author-match fallback at push.rs:1245 \
         can never fire for them — refusing to run a test that would silently \
         pass because no status event is published at all",
    );

    // --- 1. extra commit on main ------------------------------------------
    //
    // Bumps the apply-base so the applied commits' parent differs from
    // what the patches' `parent-commit` tag records — see the strategy
    // doc-comment for why that matters.
    //
    // The `--author` override is **load-bearing**: every `Repo` in the
    // harness configures `user.name = "ngit test"` and
    // `user.email = "ngit-test@example.invalid"`, so by default the extra
    // commit's author tuple matches the patches' author tuple on name and
    // email. If the extra commit also happens to share a unix-second with
    // any patch's author timestamp (commits made back-to-back in setup
    // routinely do), the author-match fallback in
    // `get_merged_proposals_info` would misclassify the extra commit as
    // `PatchApplied { ... }` too — landing a third oid in the kind-1631
    // event's `applied-as-commits` tag and a third `r` tag, neither of
    // which the test expects. Forcing a distinct author on the extra
    // commit decouples the harness's shared-user setup from the apply
    // path's author-match logic.
    std::fs::write(repo.dir().join("extra.md"), "extra\n")
        .context("failed to write extra.md in maintainer repo")?;
    git_ok(repo, ["add", "extra.md"], "git add extra.md").await?;
    git_ok(
        repo,
        [
            "commit",
            "--author=Maintainer <maint@example.invalid>",
            "-m",
            "extra commit on main",
            "--no-gpg-sign",
        ],
        "git commit (extra)",
    )
    .await?;

    // --- 2. ngit apply ----------------------------------------------------
    //
    // `parse_event_id` accepts hex strings (`apply.rs:153-164`), so the
    // root event id's hex form is sufficient. Apply does its own nostr
    // fetch via the configured nostr remote (`apply.rs:101-107`), which
    // populates the local cache with the patch series the contributor
    // published — `publish_repo`'s post-init push and the contributor's
    // `ngit send` ran against the same grasp, but the maintainer's cache
    // hasn't synced since publish_repo finished, so the fetch is required.
    let id_hex = proposal.root_event_id.to_hex();
    let apply_out = repo
        .ngit(["pr", "apply", &id_hex])
        .output()
        .await
        .context("failed to spawn ngit pr apply")?;
    anyhow::ensure!(
        apply_out.status.success(),
        "ngit pr apply {id_hex} exited {:?}\nstdout: {}\nstderr: {}",
        apply_out.status,
        String::from_utf8_lossy(&apply_out.stdout),
        String::from_utf8_lossy(&apply_out.stderr),
    );

    // --- 3. collect the resulting oids ------------------------------------
    //
    // The applied commits are HEAD~(n-1) .. HEAD, where n = number of
    // patches. They sit on top of the "extra" commit, which sits on top
    // of the seed; we walk down from HEAD by the patch count to capture
    // the applied commits parent-first.
    let n = proposal.expected_q_event_ids.len();
    anyhow::ensure!(
        n > 0,
        "expected at least one patch event in proposal.expected_q_event_ids; \
         apply-as-commits would have nothing to apply",
    );
    let mut applied = Vec::with_capacity(n);
    for i in (0..n).rev() {
        applied.push(rev_parse(repo, &format!("HEAD~{i}")).await?);
    }
    Ok(applied)
}

/// Run `ngit issue create --subject <title> --body <body>` from a logged-in
/// repo and parse the created issue event id from stdout.
async fn ngit_issue_create(repo: &Repo, title: &str, body: &str) -> Result<EventId> {
    let out = repo
        .ngit(["issue", "create", "--subject", title, "--body", body])
        .output()
        .await
        .context("failed to spawn `ngit issue create`")?;
    anyhow::ensure!(
        out.status.success(),
        "`ngit issue create` exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    parse_issue_event_id(&stdout).context(format!(
        "no `issue created:` line found in stdout; full stdout:\n{stdout}"
    ))
}

fn parse_issue_event_id(stdout: &str) -> Option<EventId> {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("issue created:") {
            let rest = line[idx + "issue created:".len()..].trim();
            if let Ok(id) = EventId::from_hex(rest) {
                return Some(id);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Status-event lookup / tag accessors
// ---------------------------------------------------------------------------

/// Find the single `Kind::GitStatusApplied` event on the grasp's relay
/// signed by `signer_pubkey` whose root `e` tag points at
/// `proposal.root_event_id`. Bails on zero or multiple matches — the push
/// pipeline emits exactly one status event per merged proposal per push.
async fn find_merge_status_event(
    harness: &Harness,
    proposal: &MergedProposal,
    signer_pubkey: PublicKey,
) -> Result<Event> {
    let events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(signer_pubkey)
                .kind(Kind::GitStatusApplied),
        )
        .await?;
    let mut matches: Vec<Event> = events
        .into_iter()
        .filter(|e| event_root_e_tag(e) == Some(proposal.root_event_id))
        .collect();
    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => anyhow::bail!(
            "no Kind::GitStatusApplied event from {signer_pubkey} found on grasp `repo` \
             whose root `e` tag matches proposal.root_event_id={}",
            proposal.root_event_id,
        ),
        _ => anyhow::bail!(
            "expected exactly 1 Kind::GitStatusApplied event from {signer_pubkey} for \
             proposal.root_event_id={}; found {}",
            proposal.root_event_id,
            matches.len(),
        ),
    }
}

async fn count_merge_status_events(
    harness: &Harness,
    proposal: &MergedProposal,
    signer_pubkey: PublicKey,
) -> Result<usize> {
    let events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(signer_pubkey)
                .kind(Kind::GitStatusApplied),
        )
        .await?;
    Ok(events
        .into_iter()
        .filter(|e| event_root_e_tag(e) == Some(proposal.root_event_id))
        .count())
}

/// Find the single `Kind::GitStatusApplied` event signed by `signer_pubkey`
/// whose root `e` tag points at the given issue id.
async fn find_issue_resolved_status_event(
    harness: &Harness,
    issue_id: EventId,
    signer_pubkey: PublicKey,
) -> Result<Event> {
    let events = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(signer_pubkey)
                .kind(Kind::GitStatusApplied),
        )
        .await?;
    let mut matches: Vec<Event> = events
        .into_iter()
        .filter(|e| event_root_e_tag(e) == Some(issue_id))
        .collect();

    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => anyhow::bail!(
            "no Kind::GitStatusApplied event from {signer_pubkey} found for issue {issue_id}"
        ),
        _ => anyhow::bail!(
            "expected exactly 1 Kind::GitStatusApplied event from {signer_pubkey} for issue {issue_id}; found {}",
            matches.len(),
        ),
    }
}

/// EventId carried by the `e` tag with marker `root`. `create_merge_status`
/// emits exactly one such tag pointing at the merged proposal (see
/// `push.rs:1428-1434`); when a revision is involved a second `e/root` tag
/// is added for the revision (`push.rs:1447-1454`), so we don't insist on
/// uniqueness here — only that *some* root-marked `e` tag points at the
/// proposal id under test.
///
/// The matcher inspects every position in the tag for the literal `"root"`
/// rather than indexing into position 3, because the relay-url position
/// can be omitted / present depending on whether `repo_ref.relays` was
/// non-empty at sign time.
fn event_root_e_tag(event: &Event) -> Option<EventId> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) != Some("e") {
            return None;
        }
        if !s.iter().any(|v| v == "root") {
            return None;
        }
        s.get(1)
            .and_then(|hex| EventId::from_hex(hex.as_str()).ok())
    })
}

/// First value of the `["<key>", <value>, ...]` tag, if any. Used for
/// `alt` and other single-valued discriminator tags.
fn tag_first_value<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            s.get(1).map(String::as_str)
        } else {
            None
        }
    })
}

/// All values of the first tag whose key matches. `merge-commit-id` /
/// `applied-as-commits` are emitted as a **single** tag carrying every
/// merge-commit oid in `create_merge_status` (`push.rs:1474-1484`), so
/// "the first such tag" is unambiguous. Returns `None` when no tag with
/// that key exists, an empty Vec when the tag has no values past the key.
fn tag_values<'a>(event: &'a Event, key: &str) -> Option<Vec<&'a str>> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some(key) {
            Some(s.iter().skip(1).map(String::as_str).collect())
        } else {
            None
        }
    })
}

/// `true` when `event` carries `["r", <oid>]` for the given commit oid.
/// `push.rs:1486-1492` emits one such tag per merge commit, in addition
/// to the `["r", <repo-root-commit>]` advertisement at `push.rs:1471-1473`,
/// so the test checks for a specific value rather than a count.
fn has_r_tag(event: &Event, oid: &str) -> bool {
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("r") && s.get(1).map(String::as_str) == Some(oid)
    })
}

/// `true` when `event` carries a `["q", <hex>, ...]` tag whose first value
/// is the hex of `target`. `create_merge_status` emits one such tag per
/// entry in `merged_patches.values()` (`push.rs:1437-1446`); for PR-kind
/// the same `pr.event_id` may appear multiple times (one per matching
/// commit) but only need-to-be-present semantics are asserted here.
fn has_q_tag(event: &Event, target: EventId) -> bool {
    let hex = target.to_hex();
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("q") && s.get(1).map(String::as_str) == Some(&*hex)
    })
}

// ---------------------------------------------------------------------------
// Tests — merge commit path (one per proposal kind)
// ---------------------------------------------------------------------------

/// Maintainer fetches the contributor's PR, merges it with `--no-ff`, and
/// pushes the resulting merge commit to `origin/main`. The push pipeline
/// must publish a single `Kind::GitStatusApplied` event tying the PR to
/// the merge commit.
#[tokio::test]
async fn merge_commit_publishes_status_event_referencing_proposal_and_commit() -> Result<()> {
    let Setup {
        harness,
        published,
        maintainer_repo,
        proposal,
    } = setup_pr().await?;

    let merge_oid = merge_pr_with_merge_commit(&maintainer_repo, &proposal).await?;

    // Push must go via `nostr_push` so the auto-generated kind-30618
    // state event covering the new main tip doesn't collide on
    // `created_at` with the previous state event from `publish_repo`'s
    // post-init push — see `test_harness::clock` for the writeup.
    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after merge")?;

    let event =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;

    assert_merge_commit_status_event(&event, &proposal, &merge_oid);

    Ok(())
}

#[tokio::test]
async fn later_default_branch_push_does_not_republish_existing_merge_status() -> Result<()> {
    let Setup {
        harness,
        published,
        maintainer_repo,
        proposal,
    } = setup_pr().await?;

    merge_pr_with_merge_commit(&maintainer_repo, &proposal).await?;
    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after merge")?;
    assert_eq!(
        count_merge_status_events(&harness, &proposal, published.maintainer_keys.public_key())
            .await?,
        1,
        "first merge push should publish exactly one status event"
    );

    let out = maintainer_repo
        .git([
            "commit",
            "--allow-empty",
            "-m",
            "unrelated default-branch work",
        ])
        .output()
        .await
        .context("create unrelated default-branch commit")?;
    anyhow::ensure!(
        out.status.success(),
        "git commit --allow-empty failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after unrelated commit")?;

    assert_eq!(
        count_merge_status_events(&harness, &proposal, published.maintainer_keys.public_key())
            .await?,
        1,
        "later default-branch pushes must not republish an already-applied PR status"
    );

    Ok(())
}

/// Patch-kind sibling of the above. The contributor publishes a patch
/// series (with cover letter) rather than a PR; everything else is identical
/// from the maintainer's perspective because `git-remote-nostr list`
/// reconstructs the patches into the same `refs/heads/pr/<branch>(<8hex>)`
/// ref shape the PR-kind path uses.
///
/// The added value over the PR-kind test is that the status event's
/// `e/root` must point at the **cover letter** id (not at any per-commit
/// patch event), and the `q` tags must enumerate the per-commit patches.
#[tokio::test]
async fn merge_commit_publishes_status_event_referencing_patch_series_cover_letter_and_commit()
-> Result<()> {
    let Setup {
        harness,
        published,
        maintainer_repo,
        proposal,
    } = setup_patch_series().await?;

    let merge_oid = merge_pr_with_merge_commit(&maintainer_repo, &proposal).await?;

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after merge")?;

    let event =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;

    assert_merge_commit_status_event(&event, &proposal, &merge_oid);

    // Patch-kind specific: each per-commit patch event must be referenced
    // via a `q` tag. PR-kind has a single root event_id so this assertion
    // is uninteresting there (a single q tag, duplicated per commit), but
    // for Patch-kind it catches a regression that would only emit the
    // first patch's q tag.
    for expected in &proposal.expected_q_event_ids {
        assert!(
            has_q_tag(&event, *expected),
            "patch-kind status event should q-tag every per-commit patch; \
             missing q tag for patch event {expected}\nfull tags: {:?}",
            event.tags,
        );
    }

    Ok(())
}

/// Regression coverage for PR updates (kind 1619): the original PR event's
/// `c` tag is stale after the contributor publishes an update, so push-time
/// merge-status detection must match the merge commit's second parent against
/// the PR update event and resolve that update back to the original PR via its
/// uppercase `E` tag.
#[tokio::test]
async fn ngit_merge_of_updated_pr_publishes_status_event_for_original_pr() -> Result<()> {
    let harness = build_harness().await?;
    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("updated-pr maintainer".into()),
            identifier: Some("updated-pr-merge-status".into()),
            ..Default::default()
        })
        .await?;

    let contributor = harness
        .clone_published_repo(
            &published,
            CloneLogin::AsContributor {
                display_name: "updated-pr contributor".into(),
            },
        )
        .await?;
    let contributor_nsec = contributor
        .config("nostr.nsec")
        .await?
        .context("contributor clone missing nostr.nsec")?;
    let contributor_pubkey = Keys::parse(&contributor_nsec)
        .context("contributor nsec is not a valid key")?
        .public_key();

    git_ok(
        &contributor,
        ["checkout", "-b", "feature"],
        "git checkout -b feature",
    )
    .await?;
    std::fs::write(contributor.dir().join("a.md"), "alpha\n").context("failed to write a.md")?;
    git_ok(&contributor, ["add", "a.md"], "git add a.md").await?;
    git_ok(
        &contributor,
        ["commit", "-m", "add a.md", "--no-gpg-sign"],
        "git commit add a.md",
    )
    .await?;
    std::fs::write(contributor.dir().join("b.md"), "beta\n").context("failed to write b.md")?;
    git_ok(&contributor, ["add", "b.md"], "git add b.md").await?;
    git_ok(
        &contributor,
        ["commit", "-m", "add b.md", "--no-gpg-sign"],
        "git commit add b.md",
    )
    .await?;
    let original_tip = rev_parse(&contributor, "HEAD").await?;

    let send_original = contributor
        .ngit([
            "send",
            "HEAD~2",
            "--force-pr",
            "--title",
            "updated pr",
            "--description",
            "original description",
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-pr")?;
    anyhow::ensure!(
        send_original.status.success(),
        "ngit send --force-pr exited {:?}\nstdout: {}\nstderr: {}",
        send_original.status,
        String::from_utf8_lossy(&send_original.stdout),
        String::from_utf8_lossy(&send_original.stderr),
    );

    let original_pr_event = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(test_harness::KIND_PULL_REQUEST),
        )
        .await?
        .into_iter()
        .find(|e| tag_first_value(e, "branch-name") == Some("feature"))
        .context("no original PR event for branch feature found on grasp repo")?;
    assert_eq!(
        tag_first_value(&original_pr_event, "c"),
        Some(original_tip.as_str()),
        "original PR event should point at the initial feature tip",
    );

    // `ngit merge` selects the latest PR update by event timestamp. Keep the
    // update strictly newer than the original PR even when this test runs in
    // parallel with the rest of the integration file.
    clock::tick_to_next_second().await;

    std::fs::write(contributor.dir().join("c.md"), "gamma\n").context("failed to write c.md")?;
    git_ok(&contributor, ["add", "c.md"], "git add c.md").await?;
    git_ok(
        &contributor,
        ["commit", "-m", "add c.md", "--no-gpg-sign"],
        "git commit add c.md",
    )
    .await?;
    let updated_tip = rev_parse(&contributor, "HEAD").await?;
    assert_ne!(
        updated_tip, original_tip,
        "test setup must make the PR update tip differ from the original PR tip",
    );

    let original_pr_id = original_pr_event.id.to_hex();
    let send_update = contributor
        .ngit([
            "send",
            "HEAD~3",
            "--force-pr",
            "--in-reply-to",
            &original_pr_id,
            "--title",
            "updated pr v2",
            "--description",
            "updated description",
        ])
        .output()
        .await
        .context("failed to spawn ngit send --force-pr --in-reply-to")?;
    anyhow::ensure!(
        send_update.status.success(),
        "ngit send --force-pr --in-reply-to exited {:?}\nstdout: {}\nstderr: {}",
        send_update.status,
        String::from_utf8_lossy(&send_update.stdout),
        String::from_utf8_lossy(&send_update.stderr),
    );

    let pr_update_event = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(contributor_pubkey)
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?
        .into_iter()
        .find(|e| tag_first_value(e, "c") == Some(updated_tip.as_str()))
        .context("no PR update event with updated c tip found on grasp repo")?;
    assert_eq!(
        tag_first_value(&pr_update_event, "E"),
        Some(original_pr_id.as_str()),
        "PR update must point back to the original PR through uppercase E",
    );

    let merge_out = maintainer_repo
        .ngit(["merge", &original_pr_id])
        .output()
        .await
        .context("failed to spawn ngit merge for updated PR")?;
    anyhow::ensure!(
        merge_out.status.success(),
        "ngit merge updated PR exited {:?}\nstdout: {}\nstderr: {}",
        merge_out.status,
        String::from_utf8_lossy(&merge_out.stdout),
        String::from_utf8_lossy(&merge_out.stderr),
    );
    let merge_oid = rev_parse(&maintainer_repo, "HEAD").await?;
    let merged_parent = rev_parse(&maintainer_repo, "HEAD^2").await?;
    assert_eq!(
        merged_parent, updated_tip,
        "ngit merge should merge the latest PR update tip, not the stale original PR tip",
    );

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after ngit merge of updated PR")?;

    let proposal = MergedProposal {
        kind: ProposalKind::Pr,
        root_event_id: original_pr_event.id,
        branch_name: "feature".into(),
        tip: updated_tip,
        expected_q_event_ids: vec![pr_update_event.id],
    };
    let event =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;

    assert_merge_commit_status_event(&event, &proposal, &merge_oid);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — fast-forward path (one per proposal kind)
// ---------------------------------------------------------------------------

/// Fast-forward of a PR-kind proposal: maintainer runs `git merge
/// --ff-only`, `main` advances to the PR tip, push produces a status event
/// whose `merge-commit-id` tag carries **every commit** in the FF range
/// (not the PR tip alone — that would be the single-oid shape of the
/// merge-commit path).
#[tokio::test]
async fn fast_forward_merge_with_pr_kind_proposal_publishes_status_event() -> Result<()> {
    let Setup {
        harness,
        published,
        maintainer_repo,
        proposal,
    } = setup_pr().await?;

    let ff_tip = merge_pr_with_fast_forward(&maintainer_repo, &proposal).await?;
    // Sanity for the test itself: FF should leave HEAD == proposal.tip,
    // which is the assertion we feed into the status-event check below.
    assert_eq!(ff_tip, proposal.tip);

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after fast-forward merge")?;

    let event =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;

    assert_fast_forward_status_event(&event, &proposal);

    Ok(())
}

/// Patch-kind sibling — same fast-forward shape, except commits are
/// reconstructed from patch events (not pushed by the contributor as a
/// `refs/heads/pr/<branch>` on the git server). The `q` tags must
/// enumerate the per-commit patch event ids (one per merged commit), not
/// just the cover letter id — catches a regression where
/// `get_merged_proposals_info` accidentally treated the cover letter as
/// a `commit`-tagged event.
#[tokio::test]
async fn fast_forward_merge_with_patch_kind_proposal_publishes_status_event() -> Result<()> {
    let Setup {
        harness,
        published,
        maintainer_repo,
        proposal,
    } = setup_patch_series().await?;

    let ff_tip = merge_pr_with_fast_forward(&maintainer_repo, &proposal).await?;
    assert_eq!(ff_tip, proposal.tip);

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after fast-forward merge")?;

    let event =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;

    assert_fast_forward_status_event(&event, &proposal);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — apply-as-commits path (patch-kind only)
// ---------------------------------------------------------------------------

/// Maintainer replays the patch series via `ngit apply` on top of an
/// **extra** commit on main, so the resulting commit oids differ from
/// every patch's `commit` tag. The push pipeline must classify the new
/// commits as `PatchApplied { event_id }` via the per-commit author-match
/// branch (`push.rs:1245-1278`) and emit a status event whose tag is
/// `applied-as-commits` (not `merge-commit-id`).
///
/// PR-kind has no equivalent because PR events don't carry an `author`
/// tag for the author-match fallback to match on — see the strategy's
/// doc-comment for the full chain.
#[tokio::test]
async fn apply_as_commits_with_patch_kind_proposal_publishes_status_event() -> Result<()> {
    let Setup {
        harness,
        published,
        maintainer_repo,
        proposal,
    } = setup_patch_series().await?;

    let applied = apply_pr_with_ngit_apply(&maintainer_repo, &proposal).await?;
    assert_eq!(
        applied.len(),
        proposal.expected_q_event_ids.len(),
        "ngit apply should land one commit per patch event",
    );
    // Every applied oid must differ from every original proposal commit —
    // that's what triggers the author-match fallback. If they collided we
    // would land in the PatchCommit branch and the tag name below would
    // be wrong; surface that here, before the kind-1631 assertion, so the
    // failure points at the strategy rather than at the assertion.
    assert_ne!(
        applied.last().unwrap(),
        &proposal.tip,
        "ngit apply produced an oid equal to the original patch tip — \
         author-match fallback won't fire; check the apply strategy",
    );

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after ngit apply")?;

    let event =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;

    // Discriminator: tag name flips from `merge-commit-id` to
    // `applied-as-commits` (push.rs:1474-1484). A regression that merged
    // the two branches in `create_merge_status` would either still emit
    // `merge-commit-id` or emit both.
    let applied_tag = tag_values(&event, "applied-as-commits").with_context(|| {
        format!(
            "status event {} has no `applied-as-commits` tag — full event: {event:?}",
            event.id,
        )
    })?;
    assert!(
        tag_values(&event, "merge-commit-id").is_none(),
        "apply-as-commits path must not also emit a `merge-commit-id` tag",
    );

    // Tag values must exactly cover the applied oids — neither under- nor
    // overshoot. Asserting length first catches the "extra commit silently
    // entered the apply set via the author-match overwrite" regression
    // (which is what would happen if the `--author` override in the
    // strategy were dropped — see the strategy doc-comment). Asserting
    // set-containment after that catches "oids in the tag don't match
    // what we applied".
    assert_eq!(
        applied_tag.len(),
        applied.len(),
        "applied-as-commits should carry exactly one oid per applied commit \
         (expected {}, got {applied_tag:?})",
        applied.len(),
    );
    let applied_set: std::collections::HashSet<&str> = applied_tag.iter().copied().collect();
    for oid in &applied {
        assert!(
            applied_set.contains(oid.as_str()),
            "applied commit oid {oid} not present in `applied-as-commits` tag values \
             {applied_tag:?}",
        );
    }

    // Canonical alt summary — same for all three merge paths
    // (`push.rs:1424-1427`).
    assert_eq!(
        tag_first_value(&event, "alt"),
        Some("git proposal merged / applied"),
        "alt tag should match the canonical merge / applied summary",
    );

    // One `["r", <oid>]` per applied commit, plus the repo-root `r` tag
    // emitted unconditionally at `push.rs:1471-1473`.
    for oid in &applied {
        assert!(
            has_r_tag(&event, oid),
            "applied commit oid {oid} should have a matching `r` tag; \
             full tags: {:?}",
            event.tags,
        );
    }

    // q-tag shape on the apply path is more lax than on the FF / merge-commit
    // paths because `get_merged_proposals_info`'s author-match branch at
    // `push.rs:1257-1278` **overwrites** the per-commit entry in
    // `merged_patches` for every patch event that author-matches the
    // commit's author tuple. Since every patch in the series carries the
    // same author tuple (the contributor's identity) and that tuple matches
    // every applied commit (whose author was preserved by `git am`), each
    // commit ends up labelled with whichever patch event happened to come
    // last in the iteration — HashMap iteration order, undefined.
    //
    // So the test cannot assert "every expected_q_event_id appears". What
    // it can assert is:
    //
    //   (1) one q tag per applied commit (one per merged_patches entry);
    //   (2) every q tag value is *some* event id from the proposal's
    //       per-commit patches (catches a regression that q-tagged the
    //       cover letter, or some unrelated event).
    let q_values: Vec<&str> = event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("q") {
                s.get(1).map(String::as_str)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        q_values.len(),
        applied.len(),
        "apply-as-commits status event should carry one q tag per applied commit; \
         got q values {q_values:?}",
    );
    let expected_q_hex: std::collections::HashSet<String> = proposal
        .expected_q_event_ids
        .iter()
        .map(|id| id.to_hex())
        .collect();
    for q in &q_values {
        assert!(
            expected_q_hex.contains(*q),
            "q tag value {q} is not one of the proposal's per-commit patch event ids \
             ({:?}); did the apply path accidentally q-tag the cover letter or an \
             unrelated event?",
            proposal.expected_q_event_ids,
        );
    }

    Ok(())
}

/// Commit-message issue references should auto-resolve the issue when the
/// commit reaches the default branch, preserving the original wording in the
/// status content and tagging both source + merge commits as `r` tags.
#[tokio::test]
async fn merge_commit_with_implements_keyword_resolves_issue() -> Result<()> {
    let harness = build_harness().await?;
    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("issue-resolution maintainer".into()),
            identifier: Some("issue-resolution-repo".into()),
            ..Default::default()
        })
        .await?;

    let issue_id = ngit_issue_create(
        &maintainer_repo,
        "offline queue",
        "track offline queue persistence",
    )
    .await?;
    let issue_short = &issue_id.to_hex()[..8];

    git_ok(
        &maintainer_repo,
        ["checkout", "-b", "fix-offline-queue"],
        "git checkout -b fix-offline-queue",
    )
    .await?;
    std::fs::write(maintainer_repo.dir().join("offline.md"), "offline queue\n")
        .context("failed to write offline.md")?;
    git_ok(
        &maintainer_repo,
        ["add", "offline.md"],
        "git add offline.md",
    )
    .await?;
    let commit_subject = format!("implements #{issue_short} offline queue");
    git_ok(
        &maintainer_repo,
        ["commit", "-m", &commit_subject, "--no-gpg-sign"],
        "git commit issue-fix branch",
    )
    .await?;
    let source_commit = rev_parse(&maintainer_repo, "HEAD").await?;

    git_ok(&maintainer_repo, ["checkout", "main"], "git checkout main").await?;
    git_ok(
        &maintainer_repo,
        [
            "merge",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "Merge fix-offline-queue",
            "fix-offline-queue",
        ],
        "git merge --no-ff fix-offline-queue",
    )
    .await?;
    let merge_commit = rev_parse(&maintainer_repo, "HEAD").await?;

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after issue-fix merge")?;

    let event = find_issue_resolved_status_event(
        &harness,
        issue_id,
        published.maintainer_keys.public_key(),
    )
    .await?;

    assert_eq!(
        tag_first_value(&event, "alt"),
        Some("issue resolved from commit message"),
        "issue resolution status should carry canonical alt tag",
    );
    assert!(
        has_r_tag(&event, &source_commit),
        "issue resolution status should carry an `r` tag for source commit {source_commit}",
    );
    assert!(
        has_r_tag(&event, &merge_commit),
        "issue resolution status should carry an `r` tag for merge commit {merge_commit}",
    );
    assert!(
        event.content.contains(&commit_subject),
        "issue resolution content should preserve commit wording; got:\n{}",
        event.content,
    );
    assert!(
        event.content.contains(&format!(
            "resolved by commit {source_commit}, when merged in commit {merge_commit}"
        )),
        "issue resolution content should include source+merge commit details; got:\n{}",
        event.content,
    );

    Ok(())
}

/// A no-ff merge can carry both a PR merge status and an issue-resolution
/// status when the merge commit message includes a resolution keyword.
#[tokio::test]
async fn merge_commit_can_publish_pr_and_issue_status_events_together() -> Result<()> {
    let harness = build_harness().await?;
    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("dual-status maintainer".into()),
            identifier: Some("dual-status-repo".into()),
            ..Default::default()
        })
        .await?;

    let issue_id = ngit_issue_create(
        &maintainer_repo,
        "queue sync",
        "track queue sync completion",
    )
    .await?;
    let issue_nevent = Nip19Event {
        event_id: issue_id,
        relays: vec![],
        author: None,
        kind: None,
    }
    .to_bech32()
    .context("failed to encode issue nevent")?;

    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: Some("feature".into()),
                commits: vec![
                    ("a.md".to_string(), "alpha\n".to_string()),
                    ("b.md".to_string(), "beta\n".to_string()),
                ],
                title: "merge me".into(),
                description: "please merge".into(),
                in_reply_to: vec![],
            },
        )
        .await?;

    let proposal = MergedProposal::from_pr(&pr);
    let merge_oid = merge_pr_with_merge_commit(&maintainer_repo, &proposal)
        .await
        .context("failed to create merge commit for proposal")?;
    let merge_subject = format!("Merge {}", long_branch(&proposal));
    let merge_body = format!("closes nostr:{issue_nevent}");
    git_ok(
        &maintainer_repo,
        [
            "commit",
            "--amend",
            "--no-gpg-sign",
            "-m",
            &merge_subject,
            "-m",
            &merge_body,
        ],
        "git commit --amend merge message with issue keyword",
    )
    .await?;

    let amended_merge_oid = rev_parse(&maintainer_repo, "HEAD").await?;
    anyhow::ensure!(
        merge_oid != amended_merge_oid,
        "expected amended merge commit id to change when adding issue keyword",
    );

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after combined-status merge")?;

    let merge_status =
        find_merge_status_event(&harness, &proposal, published.maintainer_keys.public_key())
            .await?;
    assert_eq!(
        tag_first_value(&merge_status, "alt"),
        Some("git proposal merged / applied"),
        "merge status event should keep canonical alt text",
    );

    let issue_status = find_issue_resolved_status_event(
        &harness,
        issue_id,
        published.maintainer_keys.public_key(),
    )
    .await?;
    assert_eq!(
        tag_first_value(&issue_status, "alt"),
        Some("issue resolved from commit message"),
        "issue status event should keep canonical alt text",
    );

    Ok(())
}

/// When a single push contains two no-ff merges, each issue-resolution status
/// event should point to the merge commit that actually introduced its source
/// commit (not just the youngest merge commit in the push batch).
#[tokio::test]
async fn two_no_ff_merges_in_one_push_attribute_each_issue_to_correct_merge_commit() -> Result<()> {
    let harness = build_harness().await?;
    let (maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("multi-merge issue-resolution maintainer".into()),
            identifier: Some("multi-merge-issue-resolution-repo".into()),
            ..Default::default()
        })
        .await?;

    let issue_one = ngit_issue_create(&maintainer_repo, "issue one", "first issue").await?;
    let issue_two = ngit_issue_create(&maintainer_repo, "issue two", "second issue").await?;
    let issue_one_short = &issue_one.to_hex()[..8];
    let issue_two_short = &issue_two.to_hex()[..8];

    // Branch 1 -> issue one.
    git_ok(
        &maintainer_repo,
        ["checkout", "-b", "fix-issue-one"],
        "git checkout -b fix-issue-one",
    )
    .await?;
    std::fs::write(maintainer_repo.dir().join("one.md"), "one\n")
        .context("failed to write one.md")?;
    git_ok(&maintainer_repo, ["add", "one.md"], "git add one.md").await?;
    let issue_one_subject = format!("implements #{issue_one_short} first fix");
    git_ok(
        &maintainer_repo,
        ["commit", "-m", &issue_one_subject, "--no-gpg-sign"],
        "git commit issue one",
    )
    .await?;
    let source_one = rev_parse(&maintainer_repo, "HEAD").await?;

    // Branch 2 -> issue two.
    git_ok(&maintainer_repo, ["checkout", "main"], "git checkout main").await?;
    git_ok(
        &maintainer_repo,
        ["checkout", "-b", "fix-issue-two"],
        "git checkout -b fix-issue-two",
    )
    .await?;
    std::fs::write(maintainer_repo.dir().join("two.md"), "two\n")
        .context("failed to write two.md")?;
    git_ok(&maintainer_repo, ["add", "two.md"], "git add two.md").await?;
    let issue_two_subject = format!("implements #{issue_two_short} second fix");
    git_ok(
        &maintainer_repo,
        ["commit", "-m", &issue_two_subject, "--no-gpg-sign"],
        "git commit issue two",
    )
    .await?;
    let source_two = rev_parse(&maintainer_repo, "HEAD").await?;

    // Merge both branches into main before a single push.
    git_ok(&maintainer_repo, ["checkout", "main"], "git checkout main").await?;
    git_ok(
        &maintainer_repo,
        [
            "merge",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "Merge fix-issue-one",
            "fix-issue-one",
        ],
        "git merge --no-ff fix-issue-one",
    )
    .await?;
    let merge_one = rev_parse(&maintainer_repo, "HEAD").await?;

    git_ok(
        &maintainer_repo,
        [
            "merge",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "Merge fix-issue-two",
            "fix-issue-two",
        ],
        "git merge --no-ff fix-issue-two",
    )
    .await?;
    let merge_two = rev_parse(&maintainer_repo, "HEAD").await?;

    maintainer_repo
        .nostr_push(["origin", "main"])
        .await
        .context("git push origin main after two merges")?;

    let event_one = find_issue_resolved_status_event(
        &harness,
        issue_one,
        published.maintainer_keys.public_key(),
    )
    .await?;
    let event_two = find_issue_resolved_status_event(
        &harness,
        issue_two,
        published.maintainer_keys.public_key(),
    )
    .await?;

    assert!(
        has_r_tag(&event_one, &source_one),
        "issue one status should carry source commit r tag {source_one}; tags: {:?}",
        event_one.tags
    );
    assert!(
        has_r_tag(&event_two, &source_two),
        "issue two status should carry source commit r tag {source_two}; tags: {:?}",
        event_two.tags
    );

    assert!(
        has_r_tag(&event_one, &merge_one),
        "issue one status should carry merge-one r tag {merge_one}; tags: {:?}",
        event_one.tags
    );
    assert!(
        !has_r_tag(&event_one, &merge_two),
        "issue one status should not carry merge-two r tag {merge_two}; tags: {:?}",
        event_one.tags
    );

    assert!(
        has_r_tag(&event_two, &merge_two),
        "issue two status should carry merge-two r tag {merge_two}; tags: {:?}",
        event_two.tags
    );
    assert!(
        !has_r_tag(&event_two, &merge_one),
        "issue two status should not carry merge-one r tag {merge_one}; tags: {:?}",
        event_two.tags
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

/// Assertions that hold for **every** merge-commit status event regardless
/// of proposal kind: single-valued `merge-commit-id` tag pointing at the
/// merge commit (not the proposal tip), canonical `alt`, an `r` tag for
/// the merge oid, and no `applied-as-commits` tag.
fn assert_merge_commit_status_event(event: &Event, proposal: &MergedProposal, merge_oid: &str) {
    // `merge-commit-id` is emitted as a single tag with a single value
    // for the merge-commit branch (one entry per element of `merge_commits`,
    // which is `vec![merge_commit]` at `push.rs:1366-1368`). Two-step
    // assertion catches both "tag missing" and "tag has the wrong number
    // of values".
    let merge_tag_values = tag_values(event, "merge-commit-id")
        .unwrap_or_else(|| panic!("status event has no `merge-commit-id` tag — {event:?}"));
    assert_eq!(
        merge_tag_values.len(),
        1,
        "merge-commit-id should carry exactly one oid in the merge-commit path; \
         got {merge_tag_values:?}",
    );
    assert_eq!(
        merge_tag_values[0], merge_oid,
        "merge-commit-id tag should carry the merge commit's oid",
    );
    assert_ne!(
        merge_tag_values[0], proposal.tip,
        "merge-commit-id must differ from the proposal tip — that's how downstream \
         distinguishes merge commits from fast-forwards",
    );

    // Canonical human-readable alt summary — `push.rs:1424-1427`.
    assert_eq!(
        tag_first_value(event, "alt"),
        Some("git proposal merged / applied"),
        "alt tag should match the canonical merge / applied summary",
    );

    // One `["r", <merge-oid>]` per merge commit, in addition to the
    // unconditional repo-root `r` tag.
    assert!(
        has_r_tag(event, merge_oid),
        "status event should carry an `r` tag with the merge commit oid {merge_oid}; \
         full tags: {:?}",
        event.tags,
    );

    // The merge-commit path must not also emit an `applied-as-commits`
    // tag — that's the patch-application path's discriminator
    // (`push.rs:1474-1484`).
    assert!(
        tag_values(event, "applied-as-commits").is_none(),
        "merge-commit path should not emit an `applied-as-commits` tag",
    );
}

/// Assertions that hold for **every** fast-forward status event regardless
/// of proposal kind: multi-valued `merge-commit-id` tag whose set of oids
/// matches every proposal commit, canonical `alt`, an `r` tag per advanced
/// commit, and no `applied-as-commits` tag. The tag values' order is
/// HashMap-iteration-dependent (`push.rs:1369-1373`) so the test asserts as
/// a set.
fn assert_fast_forward_status_event(event: &Event, proposal: &MergedProposal) {
    let merge_tag_values = tag_values(event, "merge-commit-id")
        .unwrap_or_else(|| panic!("status event has no `merge-commit-id` tag — {event:?}"));

    // Tag length must equal the proposal commit count: every commit in
    // `ahead` becomes a `PatchCommit` entry, all of which end up in
    // `merge_commits` for the FF path (`push.rs:1369-1373`).
    assert_eq!(
        merge_tag_values.len(),
        proposal.expected_q_event_ids.len(),
        "merge-commit-id should carry one oid per FF-advanced commit",
    );

    // The proposal tip must appear among the values — this is what
    // catches "we collapsed FF onto a single-oid merge-commit-id".
    let tag_set: std::collections::HashSet<&str> = merge_tag_values.iter().copied().collect();
    assert!(
        tag_set.contains(proposal.tip.as_str()),
        "FF merge-commit-id values should include the proposal tip {}; got {:?}",
        proposal.tip,
        merge_tag_values,
    );

    // Canonical alt summary — same as the merge-commit path.
    assert_eq!(
        tag_first_value(event, "alt"),
        Some("git proposal merged / applied"),
        "alt tag should match the canonical merge / applied summary",
    );

    // One `["r", <oid>]` per advanced commit.
    for oid in &merge_tag_values {
        assert!(
            has_r_tag(event, oid),
            "FF status event should carry an `r` tag for every advanced commit; \
             missing r tag for {oid}\nfull tags: {:?}",
            event.tags,
        );
    }

    // The FF path uses `merge-commit-id`, not `applied-as-commits`. A
    // regression that flipped `applied` for the no-`PatchApplied` case
    // would be caught here.
    assert!(
        tag_values(event, "applied-as-commits").is_none(),
        "fast-forward path should not emit an `applied-as-commits` tag",
    );

    // PR-kind has a single `pr.event_id` for `expected_q_event_ids`;
    // Patch-kind has one entry per per-commit patch event. Either way,
    // every expected event id must be q-tagged.
    for expected in &proposal.expected_q_event_ids {
        assert!(
            has_q_tag(event, *expected),
            "FF status event should q-tag every merged-patch event; \
             missing q tag for {expected}\nfull tags: {:?}",
            event.tags,
        );
    }
}
