use anyhow::{Context, Result, bail};
use ngit::{
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache,
    },
    fetch::ensure_commit_local,
    git_events::{
        KIND_COVER_NOTE, KIND_LABEL, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, pr_event_clone_tag_urls,
        process_cover_note, process_subject, tag_value,
    },
};
use nostr::{
    EventId, FromBech32, RelayUrl, ToBech32,
    nips::nip19::{Nip19, Nip19Event},
};

use crate::{
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions},
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
    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;

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

    // Ensure the PR branch exists locally at the published tip.
    let local_branch_exists = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|n| n.eq(&branch_name));

    if !local_branch_exists {
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
        .args(["merge", "--no-ff", "-m", &message, &branch_name])
        .output()
        .context("failed to run git merge")?;

    if !output.status.success() {
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

/// When invoked without an id, infer the PR from the checked-out branch. PR
/// branches are named `pr/<name>(<first-8-hex-of-event-id>)`; we extract the
/// shorthand and match it against the known proposals.
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

    let shorthand = branch
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .context(format!(
            "branch '{branch}' does not encode a PR id; specify a PR event-id or nevent"
        ))?;

    let matches: Vec<&nostr::Event> = proposals_and_revisions
        .iter()
        .filter(|e| e.id.to_hex().starts_with(shorthand))
        .collect();

    match matches.as_slice() {
        [] => bail!(
            "no known PR matches branch '{branch}' (shorthand {shorthand}); specify a PR event-id or nevent"
        ),
        [only] => Ok(only.id),
        _ => bail!(
            "branch shorthand {shorthand} is ambiguous; specify the full PR event-id or nevent"
        ),
    }
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
