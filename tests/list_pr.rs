//! Migrated regression coverage for
//! `git-remote-nostr list`'s **open-proposal listing** code path —
//! specifically the PR-kind branch in
//! `src/bin/git_remote_nostr/list.rs:247-273` (read tip from the
//! KIND_PULL_REQUEST event's `c` tag, fetch that one OID from a git
//! server, advertise only when the OID is locally resolvable).
//!
//! Patch-kind sibling lives in `tests/list_patch.rs`; the
//! `get_open_and_draft_proposals_state` function in `list.rs` branches
//! sharply between the two kinds at line 247, so a single test file
//! couldn't cover both without `if branch { ... } else { ... }`
//! duplication. Same split rationale as `tests/pr_checkout.rs` /
//! `tests/pr_checkout_patch.rs`.
//!
//! Replaces legacy
//! `tests/legacy/git_remote_nostr/list.
//! rs::when_there_are_open_proposals::open_proposal_listed_in_prs_namespace`.
//! The legacy version ran `cli_tester_create_proposals` which produced
//! **patch-kind** proposals; the new harness uses
//! [`Harness::publish_three_open_proposals`] which produces PR-kind
//! proposals (and the migration plan's "PR-kind by default, patch
//! exception" disposition). Patch-kind regression for the same
//! assertions lives in `list_patch.rs`.
//!
//! ## What `list` advertises for an open proposal
//!
//! Three ref forms per open proposal (see `list.rs:247-296` + `:300-336`):
//!
//! - `refs/heads/pr/<branch>(<8-char-shorthand>)` — the "checkout this PR
//!   locally" form. `(<shorthand>)` is the first 8 hex chars of the proposal
//!   root event id; only present when the listing repo isn't logged in as the
//!   proposal author (our `CloneLogin::None` clone is always in this state).
//! - `refs/pr/<branch>(<8-char-shorthand>)` — same name, different namespace;
//!   the "scratch / fetch raw" form.
//! - `refs/pr/<event-id-hex>/head` — canonical "pinned by event id" form.
//!
//! All three resolve to the PR's `tip` (last commit in the series).

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use anyhow::{Context, Result};
use nostr::prelude::{Filter, ToBech32};
use test_harness::{
    CloneLogin, Harness, KIND_PULL_REQUEST_UPDATE, PublishRepoOpts, PublishedPr, PublishedRepo,
    Repo,
};

async fn setup() -> Result<(Harness, PublishedRepo, [PublishedPr; 3])> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("list-pr maintainer".into()),
            identifier: Some("list-pr-repo".into()),
            ..Default::default()
        })
        .await?;

    let prs = harness.publish_three_open_proposals(&published).await?;
    Ok((harness, published, prs))
}

/// `refs/heads/pr/<branch>(<8-hex>)` long-form — same construction as
/// `tests/pr_checkout.rs::expected_branch_name`, but here we need the bare
/// branch name (without the `refs/heads/` prefix) because we're matching
/// against the parsed `git ls-remote` ref→oid map directly.
fn expected_long_branch(pr: &PublishedPr) -> String {
    let hex = pr.event_id.to_hex();
    format!("pr/{}({})", pr.branch_name, &hex[..8])
}

#[derive(Debug)]
struct LsRemoteOutput {
    refs: BTreeMap<String, String>,
}

async fn ls_remote(repo: &Repo, remote: &str) -> Result<LsRemoteOutput> {
    ls_remote_with_global_home(repo, remote, None).await
}

async fn ls_remote_with_global_home(
    repo: &Repo,
    remote: &str,
    global_home: Option<&Path>,
) -> Result<LsRemoteOutput> {
    let mut command = repo.git(["ls-remote", remote]);
    if let Some(path) = global_home {
        command.env("HOME", path);
        command.env_remove("GIT_CONFIG_GLOBAL");
    }
    let out = command
        .output()
        .await
        .with_context(|| format!("spawn git ls-remote {remote}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git ls-remote {remote} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).context("ls-remote stdout not utf-8")?;
    let mut refs = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("ref: ") {
            continue;
        }
        let (oid, name) = line
            .split_once('\t')
            .with_context(|| format!("malformed ls-remote line: {line:?}"))?;
        refs.insert(name.to_string(), oid.to_string());
    }
    Ok(LsRemoteOutput { refs })
}

async fn set_global_config(repo: &Repo, home: &Path, key: &str, value: &str) -> Result<()> {
    std::fs::create_dir_all(home).context("failed to create isolated global config home")?;
    let config_path = home.join(".gitconfig");
    let args: [&std::ffi::OsStr; 5] = [
        std::ffi::OsStr::new("config"),
        std::ffi::OsStr::new("--file"),
        config_path.as_os_str(),
        std::ffi::OsStr::new(key),
        std::ffi::OsStr::new(value),
    ];
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("failed to set global git config {key}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git config --file <isolated-global> {key} exited {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
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

async fn rev_parse(repo: &Repo, reference: &str) -> Result<String> {
    let out = repo
        .git(["rev-parse", reference])
        .output()
        .await
        .with_context(|| format!("failed to spawn git rev-parse {reference}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git rev-parse {reference} exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git rev-parse stdout not utf-8")?
        .trim()
        .to_string())
}

async fn current_branch(repo: &Repo) -> Result<String> {
    let out = repo
        .git(["symbolic-ref", "--short", "HEAD"])
        .output()
        .await
        .context("failed to spawn git symbolic-ref HEAD")?;
    anyhow::ensure!(
        out.status.success(),
        "git symbolic-ref --short HEAD exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)
        .context("git symbolic-ref stdout not utf-8")?
        .trim()
        .to_string())
}

/// Folds legacy
/// `when_there_are_open_proposals::open_proposal_listed_in_prs_namespace`.
///
/// The legacy version round-tripped `cli_tester_create_proposals` (three
/// patch-kind proposals) and then asserted the union of:
/// - main + example-branch from the state event
/// - per-proposal: `refs/heads/<long-branch>` + `refs/<long-branch>` +
///   `refs/pr/<event-id>/head`
/// equalled the entire ls-remote output (a `HashSet<String>` equality).
/// We collapse the same per-proposal expectations into one assertion
/// loop, and assert ⊆-style (every expected ref is present with the
/// right oid) — strict equality breaks under the new harness because
/// `git ls-remote` also advertises a few internal refs (e.g.
/// `HEAD` itself, the `^{}` peeled tag refs if any) that the legacy
/// PTY-driven raw `list` output didn't include.
#[tokio::test]
async fn enabling_auto_pr_branches_lists_open_prs_under_pr_namespaces() -> Result<()> {
    let (harness, published, prs) = setup().await?;

    // CloneLogin::None: no `nostr.npub` set, so `list.rs:236` always
    // takes the long-form `pr/<branch>(<shorthand>)` ref-name path —
    // matching the legacy test_repo shape.
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;
    git_ok(
        &test_repo,
        ["config", "--local", "nostr.auto-pr-branches", "true"],
        "enable automatic PR branches",
    )
    .await?;

    let ls = ls_remote(&test_repo, "origin").await?;

    // For each PR, every advertised ref must resolve to pr.tip.
    for (idx, pr) in prs.iter().enumerate() {
        let long = expected_long_branch(pr);
        let event_id_hex = pr.event_id.to_hex();
        let expected: HashMap<String, &str> = [
            (format!("refs/heads/{long}"), pr.tip.as_str()),
            (format!("refs/{long}"), pr.tip.as_str()),
            (format!("refs/pr/{event_id_hex}/head"), pr.tip.as_str()),
        ]
        .into_iter()
        .collect();

        for (ref_name, want_oid) in expected {
            let got = ls.refs.get(&ref_name).cloned();
            assert_eq!(
                got.as_deref(),
                Some(want_oid),
                "PR #{idx} ({:?}): expected {ref_name} → {want_oid}, got {got:?}\n\
                 full ls-remote refs: {:#?}",
                pr.branch_name,
                ls.refs,
            );
        }
    }

    // main was published with `publish_repo`'s seed; the announcement's
    // state event covers it. Sanity check that the proposal refs aren't
    // displacing the regular refs.
    assert_eq!(
        ls.refs.get("refs/heads/main").map(String::as_str),
        Some(published.initial_oid.as_str()),
        "main should still be listed alongside the PR namespaces",
    );

    Ok(())
}

#[tokio::test]
async fn auto_pr_branches_config_respects_global_and_local_precedence() -> Result<()> {
    let (harness, published, prs) = setup().await?;
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    let default_disabled = ls_remote(&test_repo, "origin").await?;
    assert!(
        default_disabled
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/heads/pr/") && !name.starts_with("refs/pr/")),
        "automatic PR refs should be disabled by default: {:#?}",
        default_disabled.refs,
    );

    let global_home = test_repo.dir().join(".git/test-global-home");
    set_global_config(&test_repo, &global_home, "nostr.auto-pr-branches", "true").await?;
    let globally_enabled =
        ls_remote_with_global_home(&test_repo, "origin", Some(&global_home)).await?;
    for pr in &prs {
        assert!(
            globally_enabled
                .refs
                .contains_key(&format!("refs/heads/{}", expected_long_branch(pr))),
            "global true should enable automatic PR refs for {:?}",
            pr.branch_name,
        );
    }

    git_ok(
        &test_repo,
        ["config", "--local", "nostr.auto-pr-branches", "false"],
        "disable automatic PR branches locally",
    )
    .await?;
    let locally_disabled =
        ls_remote_with_global_home(&test_repo, "origin", Some(&global_home)).await?;
    assert!(
        locally_disabled
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/heads/pr/") && !name.starts_with("refs/pr/")),
        "local false should override global true: {:#?}",
        locally_disabled.refs,
    );

    Ok(())
}

#[tokio::test]
async fn default_checkout_tracks_pull_and_maintainer_push_updates() -> Result<()> {
    let (harness, published, prs) = setup().await?;
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;

    let before_checkout = ls_remote(&test_repo, "origin").await?;
    assert!(
        before_checkout
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/heads/pr/") && !name.starts_with("refs/pr/")),
        "no PR refs should be advertised before an explicit checkout: {:#?}",
        before_checkout.refs,
    );

    let snapshot = test_repo.snapshot()?;
    assert!(
        snapshot
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/remotes/origin/pr/")),
        "a default clone should not create foreign PR tracking branches: {:#?}",
        snapshot.refs,
    );

    let selected = &prs[0];
    let branch = expected_long_branch(selected);
    let checkout = test_repo
        .ngit(["pr", "checkout", &selected.event_id.to_hex()])
        .output()
        .await
        .context("failed to spawn ngit pr checkout")?;
    anyhow::ensure!(
        checkout.status.success(),
        "ngit pr checkout exited {:?}\nstdout: {}\nstderr: {}",
        checkout.status,
        String::from_utf8_lossy(&checkout.stdout),
        String::from_utf8_lossy(&checkout.stderr),
    );
    assert_eq!(
        current_branch(&test_repo).await?,
        branch,
        "checkout should create the friendly shorthand-suffixed branch name",
    );

    assert_eq!(
        test_repo
            .config(&format!("branch.{branch}.remote"))
            .await?
            .as_deref(),
        Some("origin"),
        "checked-out PR branch should track the nostr remote",
    );
    assert_eq!(
        test_repo
            .config(&format!("branch.{branch}.merge"))
            .await?
            .as_deref(),
        Some(format!("refs/heads/{branch}").as_str()),
        "checked-out PR branch should track its remote branch",
    );

    let after_checkout = ls_remote(&test_repo, "origin").await?;
    assert_eq!(
        after_checkout
            .refs
            .get(&format!("refs/heads/{branch}"))
            .map(String::as_str),
        Some(selected.tip.as_str()),
        "the explicitly checked-out PR should remain fetchable",
    );
    for unselected in &prs[1..] {
        assert!(
            !after_checkout
                .refs
                .contains_key(&format!("refs/heads/{}", expected_long_branch(unselected))),
            "unselected PR {:?} should remain hidden",
            unselected.branch_name,
        );
    }

    git_ok(&test_repo, ["checkout", "main"], "git checkout main").await?;
    git_ok(
        &test_repo,
        ["branch", "-f", &branch, &selected.commits[0]],
        "rewind checked-out PR branch",
    )
    .await?;
    git_ok(
        &test_repo,
        ["update-ref", "-d", &format!("refs/remotes/origin/{branch}")],
        "remove selected PR remote-tracking ref",
    )
    .await?;
    git_ok(
        &test_repo,
        ["checkout", &branch],
        "check out rewound PR branch",
    )
    .await?;
    git_ok(
        &test_repo,
        ["pull", "--ff-only"],
        "pull explicitly selected PR branch",
    )
    .await?;

    assert_eq!(
        rev_parse(&test_repo, "HEAD").await?,
        selected.tip,
        "git pull should fast-forward the selected PR branch",
    );
    assert_eq!(
        rev_parse(&test_repo, &format!("refs/remotes/origin/{branch}")).await?,
        selected.tip,
        "git pull should recreate the selected PR remote-tracking ref",
    );

    std::fs::write(
        test_repo.dir().join("maintainer-follow-up.md"),
        "maintainer follow-up\n",
    )
    .context("failed to write maintainer-follow-up.md")?;
    git_ok(
        &test_repo,
        ["add", "maintainer-follow-up.md"],
        "git add maintainer follow-up",
    )
    .await?;
    git_ok(
        &test_repo,
        ["commit", "-m", "add maintainer follow-up", "--no-gpg-sign"],
        "git commit maintainer follow-up",
    )
    .await?;
    let update_tip = rev_parse(&test_repo, "HEAD").await?;

    test_repo.nostr_push(["origin", &branch]).await?;

    let update = harness
        .grasp("repo")
        .events(
            Filter::new()
                .author(published.maintainer_keys.public_key())
                .kind(KIND_PULL_REQUEST_UPDATE),
        )
        .await?
        .into_iter()
        .find(|event| {
            event.tags.iter().any(|tag| {
                let values = tag.as_slice();
                values.first().map(String::as_str) == Some("c")
                    && values.get(1).map(String::as_str) == Some(update_tip.as_str())
            })
        })
        .context("maintainer push did not publish a PR update at the new tip")?;
    let selected_event_id = selected.event_id.to_hex();
    assert!(
        update.tags.iter().any(|tag| {
            let values = tag.as_slice();
            values.first().map(String::as_str) == Some("E")
                && values.get(1).map(String::as_str) == Some(selected_event_id.as_str())
        }),
        "PR update should reference the contributor's original proposal",
    );
    assert_eq!(
        rev_parse(&test_repo, &format!("refs/remotes/origin/{branch}")).await?,
        update_tip,
        "git push should advance the selected PR remote-tracking ref",
    );

    Ok(())
}

/// A proposal author addresses their own PR by its bare branch name
/// (`pr/<branch>`), but `ngit pr checkout` creates the shorthand-id suffixed
/// name regardless of authorship. With automatic PR branches disabled by
/// default, either
/// local branch must opt the author's own proposal back in.
#[tokio::test]
async fn default_auto_pr_branches_own_pr_opts_in_via_bare_branch_or_checkout() -> Result<()> {
    let (harness, published, prs) = setup().await?;
    let test_repo = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await?;

    let own = &prs[0];
    git_ok(
        &test_repo,
        [
            "config",
            "--local",
            "nostr.npub",
            &own.author_pubkey.to_bech32()?,
        ],
        "configure the clone as the proposal author",
    )
    .await?;

    // Fetch the proposal object once through the explicit compatibility
    // setting, then remove the override so both phases below exercise the
    // default-disabled selection behavior.
    git_ok(
        &test_repo,
        ["config", "--local", "nostr.auto-pr-branches", "true"],
        "temporarily enable automatic PR branches",
    )
    .await?;
    git_ok(&test_repo, ["fetch", "origin"], "fetch own proposal object").await?;
    git_ok(
        &test_repo,
        ["config", "--local", "--unset", "nostr.auto-pr-branches"],
        "restore the default automatic PR branch setting",
    )
    .await?;

    // Phase 1: the author's original machine keeps the bare branch name.
    let bare_branch = format!("pr/{}", own.branch_name);
    git_ok(
        &test_repo,
        ["branch", &bare_branch, &own.tip],
        "create the author's bare proposal branch",
    )
    .await?;

    let with_bare = ls_remote(&test_repo, "origin").await?;
    assert_eq!(
        with_bare
            .refs
            .get(&format!("refs/heads/{bare_branch}"))
            .map(String::as_str),
        Some(own.tip.as_str()),
        "the bare local branch should keep the author's own PR advertised",
    );
    assert_eq!(
        with_bare
            .refs
            .keys()
            .filter(|name| name.starts_with("refs/heads/pr/"))
            .count(),
        1,
        "only the author's opted-in proposal should be advertised: {:#?}",
        with_bare.refs,
    );

    // Phase 2: a fresh machine has no bare branch; checkout must opt back in
    // even though it creates the suffixed branch name.
    git_ok(
        &test_repo,
        ["branch", "-D", &bare_branch],
        "delete the bare proposal branch",
    )
    .await?;
    let hidden = ls_remote(&test_repo, "origin").await?;
    assert!(
        hidden
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/heads/pr/")),
        "no PR refs should be advertised without a matching local branch: {:#?}",
        hidden.refs,
    );

    let checkout = test_repo
        .ngit(["pr", "checkout", &own.event_id.to_hex()])
        .output()
        .await
        .context("failed to spawn ngit pr checkout")?;
    anyhow::ensure!(
        checkout.status.success(),
        "ngit pr checkout exited {:?}\nstdout: {}\nstderr: {}",
        checkout.status,
        String::from_utf8_lossy(&checkout.stdout),
        String::from_utf8_lossy(&checkout.stderr),
    );

    let suffixed_branch = expected_long_branch(own);
    assert_eq!(
        test_repo
            .config(&format!("branch.{suffixed_branch}.merge"))
            .await?
            .as_deref(),
        Some(format!("refs/heads/{suffixed_branch}").as_str()),
        "checked-out own PR branch should track its remote branch",
    );

    let after_checkout = ls_remote(&test_repo, "origin").await?;
    assert_eq!(
        after_checkout
            .refs
            .get(&format!("refs/heads/{suffixed_branch}"))
            .map(String::as_str),
        Some(own.tip.as_str()),
        "checkout must opt the author's own PR back in under the suffixed name",
    );

    Ok(())
}
