use anyhow::{Context, Result, bail};
use ngit::{
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache,
    },
    fetch::ensure_commit_local,
    git_events::{
        KIND_COVER_NOTE, KIND_LABEL, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, is_event_proposal_root_for_branch,
        pr_event_clone_tag_urls, process_cover_note, process_subject, tag_value,
    },
    login::{get_curent_user, user::extract_user_metadata},
};
use nostr::{
    EventId, FromBech32, PublicKey, RelayUrl, ToBech32,
    nips::nip19::{Nip19, Nip19Event},
};

use crate::{
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions, str_to_sha1},
    git_events::event_to_cover_letter,
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

#[allow(clippy::too_many_lines)]
pub async fn launch(id: Option<&str>, offline: bool, exclude_description: bool) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    // Refuse to operate on a dirty tree. We are about to switch to the default
    // branch and create a merge commit there; carrying (un)staged or untracked
    // changes across that switch cannot be done safely in the general case, so
    // we abort and let the user stash or commit first.
    if git_repo.has_outstanding_changes()? {
        bail!(
            "working directory has uncommitted changes (staged, unstaged or untracked). Commit or stash them before merging."
        );
    }

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let proposals_and_revisions =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    // Resolve which proposal to merge: either the explicit id, or — when no id
    // is given — the proposal encoded in the checked-out `pr/...` branch name.
    let event_id = if let Some(id) = id {
        parse_event_id(id)?
    } else {
        resolve_event_id_from_current_branch(&git_repo, &proposals_and_revisions)?
    };

    let proposal = proposals_and_revisions
        .iter()
        .find(|e| e.id == event_id)
        .context(format!(
            "PR with id {} not found in cache",
            event_id.to_hex()
        ))?
        .clone();

    let cover_letter = event_to_cover_letter(&proposal).context("failed to extract PR details")?;
    // Canonical branch name created by `ngit pr checkout`:
    // `pr/<name>(<8-hex-of-event-id>)`.
    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;
    // Bare branch name a self-submitting author pushes by hand
    // (`git push -u origin pr/<name>`); it carries no shorthand. The same
    // mapping `git-remote-nostr` and `resolve_event_id_from_current_branch`
    // use to link such a branch back to its published PR.
    let bare_branch_name = format!("pr/{}", cover_letter.branch_name_without_id_or_prefix);

    // Find the PR tip commit.
    let commits_events = get_all_proposal_patch_pr_pr_update_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal.id,
    )
    .await?;

    let tip_chain = get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events)
        .context("failed to find any PR or patch events on this proposal")?;

    let tip_event = tip_chain.first().context("tip chain is empty")?;
    let is_pr_kind = tip_chain
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind));

    let tip_commit_str = if is_pr_kind {
        tag_value(tip_event, "c").context("PR event missing tip commit tag 'c'")?
    } else {
        ngit::git_events::get_commit_id_from_patch(tip_event)
            .context("failed to get commit id from patch")?
    };

    // Determine which local branch represents this PR. `ngit pr checkout`
    // creates the canonical `pr/<name>(<shorthand>)` form, but a self-
    // submitting author who pushed `pr/<name>` by hand has only the bare form
    // checked out. Either may carry unpublished local commits, so we must run
    // the drift check against whichever one actually exists locally.
    let local_branch_names = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?;
    let canonical_exists = local_branch_names.iter().any(|n| n.eq(&branch_name));
    let bare_exists = local_branch_names.iter().any(|n| n.eq(&bare_branch_name));

    // The branch we will actually merge. The bare self-submitted form is used
    // only when it exists and the canonical form does not; otherwise the
    // canonical (shorthand) form is used — created below at the published tip
    // when neither exists.
    let merge_branch = if !canonical_exists && bare_exists {
        bare_branch_name.clone()
    } else {
        branch_name.clone()
    };

    if canonical_exists || bare_exists {
        // The local PR branch already exists. The merge commit message we
        // autogenerate (subject, nevent, author, cover note) describes the
        // *published* PR state, and other maintainers can only reproduce a
        // merge of the published tip. If the local branch tip has drifted from
        // the published tip, merging would produce a commit that misrepresents
        // — or simply cannot be reproduced from — what is on the relays. Refuse
        // and tell the user how to reconcile.
        ensure_local_branch_matches_published_tip(
            &git_repo,
            &merge_branch,
            &tip_commit_str,
            &event_id,
        )?;
    } else {
        // For PR-kind proposals the tip commit lives on a git server, so try
        // to fetch it. (Patch-kind proposals reconstruct the tip from patch
        // events via `ngit pr checkout`, which we don't replicate here.)
        if !git_repo.does_commit_exist(&tip_commit_str)? && !offline && is_pr_kind {
            let _ = ensure_commit_local(
                &tip_commit_str,
                &git_repo,
                &repo_ref,
                &pr_event_clone_tag_urls(tip_event),
                &console::Term::stderr(),
            );
        }
        if !git_repo.does_commit_exist(&tip_commit_str)? {
            bail!(
                "PR tip commit {tip_commit_str} not found locally. Run `ngit pr checkout {}` first.",
                event_id.to_hex()
            );
        }
        git_repo.create_branch_at_commit(&branch_name, &tip_commit_str)?;
    }

    // Resolve the default branch and check it out before merging.
    let default_branch = git_repo
        .get_default_branch_name(None)?
        .context("could not determine the repository's default branch (e.g. main or master)")?;

    if !git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|n| n.eq(&default_branch))
    {
        bail!(
            "default branch '{default_branch}' does not exist locally; check it out before merging"
        );
    }

    git_repo.checkout(&default_branch).context(format!(
        "failed to check out default branch '{default_branch}'"
    ))?;

    // Resolve the effective (latest edited) title via the #subject label
    // override, falling back to the root proposal's title.
    let label_events = get_events_from_local_cache(
        git_repo_path,
        vec![nostr::Filter::default().event(proposal.id).kind(KIND_LABEL)],
    )
    .await
    .unwrap_or_default();
    let title = process_subject(&proposal, &repo_ref, &label_events)
        .unwrap_or_else(|| cover_letter.title.clone());

    // Compose the merge commit subject: `Merge #<hex8>: <title>`, truncated so
    // it fits within git/gitlint's 72-char subject limit. When the title is
    // truncated the full title is preserved on the first body line so nothing
    // is lost in `git log` (it also remains available via the nevent below).
    let relay_hint = repo_ref.relays.first();
    let nevent = event_id_to_nevent(event_id, relay_hint);
    let (subject, truncated) = build_subject(&event_id.to_hex(), &title);

    let mut message = subject;
    if truncated {
        message.push_str("\n\n");
        message.push_str(title.trim());
    }

    // The nevent is emitted as a bare `nostr:` URI line so it is recognised as
    // a nostr URI and is exempt from body line-length linting (see .gitlint).
    message.push_str("\n\nnostr:");
    message.push_str(&nevent);

    // Attribute the PR's author with a `PR-Author:` trailer. The display name
    // is only emitted when kind-0 metadata for the author is found in the
    // cache; the npub is always emitted as a bare `nostr:` URI line so it is
    // recognised as a nostr URI and exempt from body line-length linting.
    message.push_str(&author_trailer(&proposal.pubkey, git_repo_path).await);

    // Append the cover note (latest authorised kind-1624) when present,
    // otherwise the PR description. Suppressed by --exclude-description.
    if !exclude_description {
        let cover_note_events = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .event(proposal.id)
                    .kind(KIND_COVER_NOTE),
            ],
        )
        .await
        .unwrap_or_default();

        if let Some((cover_note, _)) = process_cover_note(&proposal, &repo_ref, &cover_note_events)
        {
            let body = cover_note.content.trim();
            if !body.is_empty() {
                message.push_str("\n\nCoverNote:\n\n");
                message.push_str(body);
            }
        } else {
            let description = cover_letter.description.trim();
            if !description.is_empty() {
                message.push_str("\n\nPR description:\n\n");
                message.push_str(description);
            }
        }
    }

    let output = std::process::Command::new("git")
        .current_dir(git_repo_path)
        .args(["merge", "--no-ff", "-m", &message, &merge_branch])
        .output()
        .context("failed to run git merge")?;

    if !output.status.success() {
        // A `git merge` that stops on conflicts leaves the merge in progress:
        // `.git/MERGE_HEAD` is written, the index carries the unmerged stages
        // and the working tree has conflict markers. Git does *not* honour the
        // `-m` message in this case — it writes its own generic MERGE_MSG that
        // the user's eventual `git commit` would pick up, silently discarding
        // the nostr provenance ngit composed (subject, nevent, PR-Author
        // trailer, cover note). When we detect the conflict path we therefore
        // overwrite MERGE_MSG with our message and hand the resolution back to
        // the user rather than treating it as a hard error.
        if git_repo.merge_in_progress()? {
            write_prepared_merge_message(&git_repo, &message)
                .context("failed to record the prepared merge commit message")?;

            println!(
                "{}",
                console::style(format!(
                    "the merge has conflicts that must be resolved manually on {default_branch}."
                ))
                .yellow()
            );
            println!(
                "resolve the conflicts (see `git status`), `git add` the resolved files, then run `git commit` to complete the merge."
            );
            println!(
                "the merge commit message describing the PR has been prepared for you, so leave it unchanged when `git commit` opens your editor."
            );
            println!("to abandon the merge, run `git merge --abort`.");
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("git merge failed:\n{stdout}{stderr}");
    }

    println!(
        "{}",
        console::style(format!(
            "merge commit created on {default_branch}. don't forget to push"
        ))
        .green()
    );

    Ok(())
}

/// Overwrite the in-progress merge's prepared commit message
/// (`.git/MERGE_MSG`) with `message`.
///
/// When `git merge` stops on conflicts it writes its own generic `MERGE_MSG`,
/// ignoring the `-m` we passed. The user's eventual `git commit` reads that
/// file, so to preserve ngit's composed message — subject, `nostr:` nevent,
/// `PR-Author:` trailer and cover note — we replace it here once the conflict
/// is detected.
///
/// The path is resolved with `git rev-parse --git-path MERGE_MSG` so it is
/// correct regardless of worktree layout or a separated git dir, rather than
/// assuming `.git/MERGE_MSG` under the working tree.
fn write_prepared_merge_message(git_repo: &Repo, message: &str) -> Result<()> {
    let git_repo_path = git_repo.get_path()?;
    let output = std::process::Command::new("git")
        .current_dir(git_repo_path)
        .args(["rev-parse", "--git-path", "MERGE_MSG"])
        .output()
        .context("failed to locate MERGE_MSG via git rev-parse")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --git-path MERGE_MSG failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let rel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // `--git-path` yields a path relative to the working directory we invoked
    // git in (or already absolute); resolve it against that same directory.
    let path = git_repo_path.join(rel);
    // Preserve a trailing newline so the message matches git's own formatting.
    std::fs::write(&path, format!("{}\n", message.trim_end())).context(format!(
        "failed to write merge message to {}",
        path.display()
    ))?;
    Ok(())
}

/// Build the merge commit subject `Merge #<hex8>: <title>`, truncating the
/// title with an ellipsis so the whole line stays within git/gitlint's 72-char
/// subject limit. The first 8 hex chars of the event id mirror the web UI's
/// `#e2df2001` shorthand.
///
/// Returns `(subject, truncated)` where `truncated` is `true` when the title
/// did not fit and was shortened — the caller then preserves the full title on
/// the first body line so it is not lost.
fn build_subject(event_id_hex: &str, title: &str) -> (String, bool) {
    const MAX_SUBJECT_LEN: usize = 72;
    let shorthand = &event_id_hex[..8.min(event_id_hex.len())];
    let prefix = format!("Merge #{shorthand}: ");
    let title = title.trim();

    // Characters available for the title after the prefix.
    let budget = MAX_SUBJECT_LEN.saturating_sub(prefix.chars().count());
    let title_chars = title.chars().count();

    if title_chars <= budget {
        return (format!("{prefix}{title}"), false);
    }

    // Truncate to budget, reserving one char for the ellipsis. Iterate over
    // chars (not bytes) to avoid splitting multi-byte UTF-8.
    let keep = budget.saturating_sub(1);
    let truncated: String = title.chars().take(keep).collect();
    (format!("{prefix}{}\u{2026}", truncated.trim_end()), true)
}

fn parse_event_id(id: &str) -> Result<EventId> {
    if let Ok(nip19) = Nip19::from_bech32(id) {
        match nip19 {
            Nip19::Event(e) => return Ok(e.event_id),
            Nip19::EventId(event_id) => return Ok(event_id),
            _ => {}
        }
    }
    if let Ok(event_id) = EventId::from_hex(id) {
        return Ok(event_id);
    }
    bail!("invalid event-id or nevent: {id}")
}

/// When invoked without an id, infer the PR from the checked-out branch.
///
/// Two branch-naming conventions are recognised:
///
/// 1. Branches created by `ngit pr checkout` are named
///    `pr/<name>(<first-8-hex-of-event-id>)`; the shorthand is extracted and
///    matched against the known proposals.
///
/// 2. Branches the current user authored and published themselves with a plain
///    `git push <remote> -u pr/<name>` carry no shorthand. These are linked to
///    a published PR by matching the bare `pr/<name>` against proposals
///    authored by the logged-in user — the same mapping `git-remote-nostr` uses
///    on push (`is_event_proposal_root_for_branch`).
fn resolve_event_id_from_current_branch(
    git_repo: &Repo,
    proposals_and_revisions: &[nostr::Event],
) -> Result<EventId> {
    let branch = git_repo
        .get_checked_out_branch_name()
        .context("failed to determine the checked-out branch")?;

    if !branch.starts_with("pr/") {
        bail!("not on a `pr/` branch; specify a PR event-id or nevent, e.g. `ngit merge <id>`");
    }

    // Convention 1: `pr/<name>(<8-hex>)` created by `ngit pr checkout`.
    if let Some(shorthand) = branch
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
    {
        let matches: Vec<&nostr::Event> = proposals_and_revisions
            .iter()
            .filter(|e| e.id.to_hex().starts_with(shorthand))
            .collect();

        return match matches.as_slice() {
            [] => bail!(
                "no known PR matches branch '{branch}' (shorthand {shorthand}); specify a PR event-id or nevent"
            ),
            [only] => Ok(only.id),
            _ => bail!(
                "branch shorthand {shorthand} is ambiguous; specify the full PR event-id or nevent"
            ),
        };
    }

    // Convention 2: a plain `pr/<name>` the current user pushed themselves
    // (`git push <remote> -u pr/<name>`). Link it to a published PR the
    // logged-in user authored, mirroring the push-side mapping.
    let current_user =
        get_curent_user(git_repo).context("failed to read the logged-in user from git config")?;

    let matches: Vec<&nostr::Event> = proposals_and_revisions
        .iter()
        .filter(|e| {
            is_event_proposal_root_for_branch(e, &branch, current_user.as_ref()).unwrap_or(false)
        })
        .collect();

    match matches.as_slice() {
        [only] => Ok(only.id),
        [] => {
            if current_user.is_none() {
                bail!(
                    "branch '{branch}' does not encode a PR id and no logged-in user is configured to link it to a published PR; specify a PR event-id or nevent, or run `ngit login`"
                );
            }
            bail!(
                "branch '{branch}' does not encode a PR id and no PR you authored matches it; specify a PR event-id or nevent"
            )
        }
        _ => bail!(
            "branch '{branch}' matches more than one of your PRs; specify the PR event-id or nevent"
        ),
    }
}

/// Refuse to merge when the local `pr/...` branch tip has drifted from the
/// published PR tip.
///
/// `ngit merge` autogenerates the merge commit message — subject, nevent,
/// `PR-Author:` trailer and cover note — to describe the *published* state of
/// the PR, and other maintainers can only reproduce the merge against the tip
/// recorded on the relays. If the local branch points somewhere else we must
/// not merge:
///
/// * **local ahead** — the user has commits that are not yet published; merging
///   would bake unpublished work into the default branch and produce a merge no
///   one else can reproduce. They should push first so the PR tip is updated.
/// * **local behind** — the local branch is stale; merging would land an older
///   revision than what is published. They should fast-forward to the published
///   tip first.
/// * **diverged** — both, or histories that share no relationship.
///
/// When the local tip already equals the published tip this is a no-op.
fn ensure_local_branch_matches_published_tip(
    git_repo: &Repo,
    branch_name: &str,
    tip_commit_str: &str,
    event_id: &EventId,
) -> Result<()> {
    let local_tip = git_repo.get_tip_of_branch(branch_name).context(format!(
        "failed to read local tip of branch '{branch_name}'"
    ))?;

    let published_tip =
        str_to_sha1(tip_commit_str).context("PR event recorded an invalid tip commit id")?;

    if local_tip.eq(&published_tip) {
        return Ok(());
    }

    // If the published tip is not in the local object database we cannot
    // classify the drift; report the mismatch plainly.
    if !git_repo.does_commit_exist(tip_commit_str)? {
        bail!(
            "local branch '{branch_name}' (at {local_tip}) does not match the published PR tip {tip_commit_str}, which is not present locally. Reconcile the branch with the published PR before merging (e.g. fetch, then `ngit pr checkout {}`).",
            event_id.to_hex()
        );
    }

    // Both commits are present locally: classify the drift to give precise
    // guidance. `get_commits_ahead_behind(base, latest)` returns
    // `(ahead, behind)` relative to `latest`: `ahead` = commits on the local
    // branch missing from the published tip, `behind` = commits on the
    // published tip missing locally. It errors when the two share no common
    // ancestor at all, which we surface as an unrelated-histories mismatch.
    let Ok((ahead, behind)) = git_repo.get_commits_ahead_behind(&published_tip, &local_tip) else {
        bail!(
            "local branch '{branch_name}' (at {local_tip}) shares no history with the published PR tip {tip_commit_str}. Reconcile the branch with the published PR before merging (e.g. `ngit pr checkout {}`).",
            event_id.to_hex()
        )
    };

    match (ahead.is_empty(), behind.is_empty()) {
        // ahead only: unpublished local commits.
        (false, true) => bail!(
            "local branch '{branch_name}' is {} commit(s) ahead of the published PR tip {tip_commit_str}. Push your changes so the PR is updated before merging, e.g. `git push <remote> {branch_name}`.",
            ahead.len()
        ),
        // behind only: stale local branch.
        (true, false) => bail!(
            "local branch '{branch_name}' is {} commit(s) behind the published PR tip {tip_commit_str}. Fast-forward it to the published tip before merging, e.g. `git checkout {branch_name} && git merge --ff-only {tip_commit_str}`.",
            behind.len()
        ),
        // diverged: both ahead and behind.
        (false, false) => bail!(
            "local branch '{branch_name}' has diverged from the published PR tip {tip_commit_str} ({} ahead, {} behind). Reconcile it with the published PR (push your changes or reset to the published tip) before merging.",
            ahead.len(),
            behind.len()
        ),
        // equal commit lists but differing tips is unreachable (we returned
        // early on equality) — treat defensively as a plain mismatch.
        (true, true) => bail!(
            "local branch '{branch_name}' (at {local_tip}) does not match the published PR tip {tip_commit_str}. Reconcile the branch with the published PR before merging."
        ),
    }
}

/// Build the `PR-Author:` trailer attributing the PR's author.
///
/// The trailer always carries the author's npub on its own line as a bare
/// `nostr:` URI (so git/gitlint treats it as a nostr URI exempt from body
/// line-length linting). When kind-0 metadata for the author is found in the
/// local cache, the resolved display name is emitted on the `PR-Author:` line;
/// otherwise that line carries only the label.
///
/// The lookup reads the **local** cache because that is where
/// `fetching_with_report` lands contributor profiles (kind-0) for proposal
/// authors — see `get_repo_coordinates`'s profile back-fill in client.rs.
async fn author_trailer(author: &PublicKey, git_repo_path: &std::path::Path) -> String {
    let npub = author.to_bech32().unwrap_or_else(|_| author.to_hex());

    // Only surface a display name when kind-0 metadata is actually present in
    // the cache. `extract_user_metadata` falls back to the npub when no
    // human-readable name is set, so treat that fallback as "no name".
    let metadata_events = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .author(*author)
                .kind(nostr::Kind::Metadata),
        ],
    )
    .await
    .unwrap_or_default();

    let display_name = extract_user_metadata(author, &metadata_events)
        .ok()
        .map(|m| m.name.trim().to_string())
        .filter(|name| !name.is_empty() && *name != npub);

    let label = match display_name {
        Some(name) => format!("\n\nPR-Author: {name}"),
        None => "\n\nPR-Author:".to_string(),
    };
    format!("{label}\nnostr:{npub}")
}

fn event_id_to_nevent(event_id: EventId, relay: Option<&RelayUrl>) -> String {
    let relays = relay.map(|r| vec![r.clone()]).unwrap_or_default();
    Nip19Event {
        event_id,
        relays,
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_else(|_| event_id.to_hex())
}

#[cfg(test)]
mod tests {
    use super::build_subject;

    const ID: &str = "e2df2001abcdef0123456789abcdef0123456789abcdef0123456789abcdef01";

    #[test]
    fn short_title_is_not_truncated() {
        let (subject, truncated) = build_subject(ID, "fix the thing");
        assert_eq!(subject, "Merge #e2df2001: fix the thing");
        assert!(!truncated);
    }

    #[test]
    fn long_title_is_truncated_within_72_chars_and_flagged() {
        let title = "a really long pull request title that goes well beyond the subject limit";
        let (subject, truncated) = build_subject(ID, title);
        assert!(
            truncated,
            "an over-length title must be flagged as truncated"
        );
        assert!(
            subject.chars().count() <= 72,
            "subject must fit within 72 chars, got {}: {subject}",
            subject.chars().count(),
        );
        assert!(subject.starts_with("Merge #e2df2001: "));
        assert!(
            subject.ends_with('\u{2026}'),
            "truncated subject ends with an ellipsis"
        );
    }

    #[test]
    fn truncation_does_not_split_multibyte_chars() {
        // 70 multibyte chars guarantees truncation; the result must remain
        // valid UTF-8 (no panic from slicing mid-codepoint).
        let title = "é".repeat(70);
        let (subject, truncated) = build_subject(ID, &title);
        assert!(truncated);
        assert!(subject.chars().count() <= 72);
    }
}
