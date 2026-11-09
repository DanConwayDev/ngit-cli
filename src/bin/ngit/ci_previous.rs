//! Resolve published revision order without using CI arrival times.
use anyhow::Result;
use ngit::{
    ci::{events::WorkflowRun, total_order_position},
    client::{
        get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache, get_state_from_cache,
    },
    git::{Repo, RepoActions},
    git_events::{KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, get_commit_id_from_patch},
    repo_ref::RepoRef,
};
use nostr::prelude::EventId;

use crate::ci_projection::Target;

pub struct Candidates {
    pub source: &'static str,
    pub runs: Vec<WorkflowRun>,
    pub commits: Vec<String>,
    pub notice: Option<String>,
}
impl Candidates {
    fn unavailable(message: &str) -> Self {
        Self {
            source: "unknown",
            runs: vec![],
            commits: vec![],
            notice: Some(message.into()),
        }
    }
}
pub async fn resolve(
    repo: &Repo,
    reference: &RepoRef,
    target: &Target,
    scope: &crate::ci_history::Scope,
    runs: Vec<WorkflowRun>,
) -> Result<Candidates> {
    let path = repo.get_path()?;
    let pr = match target {
        Target::PullRequest { root, .. } => Some(*root),
        Target::Commit { .. } if scope.automatic => {
            let branch = repo.get_checked_out_branch_name().unwrap_or_default();
            if branch.starts_with("pr/") {
                let proposals =
                    get_proposals_and_revisions_from_cache(path, reference.coordinates()).await?;
                let Ok(user) = ngit::login::get_curent_user(repo) else {
                    return Ok(Candidates::unavailable(
                        "Could not read the account identity needed to associate this branch with a PR.",
                    ));
                };
                match associated_pr(&branch, &proposals, user.as_ref()) {
                    Ok(id) => Some(id),
                    Err(reason) => return Ok(Candidates::unavailable(reason.message())),
                }
            } else {
                None
            }
        }
        Target::Commit { .. } => None,
    };
    if let Some(root) = pr {
        return pr_candidates(repo, reference, target, root, runs).await;
    }
    if !scope.automatic {
        return Ok(Candidates {
            source: "explicit_commit",
            runs: vec![],
            commits: vec![],
            notice: None,
        });
    }
    branch_candidates(repo, reference, target, scope, runs).await
}
#[derive(Debug, PartialEq, Eq)]
enum AssociationError {
    NotFound,
    NamedNotFound,
    Ambiguous,
}
impl AssociationError {
    fn message(&self) -> &'static str {
        match self {
            Self::NotFound => "No published PR found",
            Self::NamedNotFound => {
                "The PR named by this branch was not found in the available repository data."
            }
            Self::Ambiguous => {
                "Multiple published PRs match this branch; specify a PR ID to view its CI history."
            }
        }
    }
}

// Passive CI history needs a branch/PR association, not just shared Git
// objects. The merge command's exact-tip fallback is appropriate for an
// explicit merge, but could associate a new unpublished branch with a PR it was
// based on here.
fn associated_pr(
    branch: &str,
    proposals: &[nostr::prelude::Event],
    user: Option<&nostr::prelude::PublicKey>,
) -> std::result::Result<EventId, AssociationError> {
    let shorthand = branch
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.strip_suffix(')'))
        .filter(|id| !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let matches: std::collections::BTreeSet<_> =
        crate::sub_commands::id_resolver::proposal_roots(proposals)
            .filter(|event| match shorthand {
                Some(id) => event.id.to_hex().starts_with(&id.to_ascii_lowercase()),
                None => ngit::git_events::is_event_proposal_root_for_branch(event, branch, user)
                    .unwrap_or(false),
            })
            .map(|event| event.id)
            .collect();
    match (matches.len(), matches.first()) {
        (0, _) => Err(if shorthand.is_some() {
            AssociationError::NamedNotFound
        } else {
            AssociationError::NotFound
        }),
        (1, Some(id)) => Ok(*id),
        _ => Err(AssociationError::Ambiguous),
    }
}

async fn pr_candidates(
    repo: &Repo,
    reference: &RepoRef,
    target: &Target,
    root: EventId,
    runs: Vec<WorkflowRun>,
) -> Result<Candidates> {
    let thread =
        get_all_proposal_patch_pr_pr_update_events_from_cache(repo.get_path()?, reference, &root)
            .await?;
    let pr_target =
        crate::ci_projection::pull_request_target(repo.get_path()?, reference, root).await?;
    let Target::PullRequest {
        anchor, revision, ..
    } = pr_target
    else {
        unreachable!()
    };
    let mut revisions: Vec<_> = thread
        .iter()
        .filter(|e| {
            [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind) && e.verify().is_ok()
        })
        .collect();
    revisions.sort_by_key(|e| std::cmp::Reverse(total_order_position(e.created_at, e.id)));
    let Some(current) = revisions.iter().find(|e| e.id == revision) else {
        return Ok(Candidates::unavailable(
            "Published PR revision unavailable; no previous tip inferred.",
        ));
    };
    let Ok(published) = get_commit_id_from_patch(current) else {
        return Ok(Candidates::unavailable(
            "Published PR commit unavailable; no previous tip inferred.",
        ));
    };
    let current_commit = match target {
        Target::Commit { ids, .. } => {
            let head = ids.first().map(String::as_str).unwrap_or_default();
            if !same_or_descendant(repo, head, &published) {
                return Ok(Candidates::unavailable(
                    "Local HEAD is behind, divergent from, or cannot be compared with the published PR tip; no previous tip inferred.",
                ));
            }
            head.to_owned()
        }
        Target::PullRequest { .. } => published,
    };
    let cutoff = total_order_position(current.created_at, current.id);
    let mut commits = Vec::new();
    let mut selected = Vec::new();
    for revision in revisions
        .into_iter()
        .filter(|e| total_order_position(e.created_at, e.id) <= cutoff)
    {
        let Ok(commit) = get_commit_id_from_patch(revision) else {
            continue;
        };
        if commit == current_commit || commits.contains(&commit) {
            continue;
        }
        let matching: Vec<_> = runs
            .iter()
            .filter(|r| {
                r.pr_root == Some(anchor)
                    && r.commit() == commit
                    && r.supplying_event.unwrap_or(anchor) == revision.id
            })
            .cloned()
            .collect();
        if !matching.is_empty() {
            commits.push(commit);
            selected.extend(matching);
        }
    }
    Ok(Candidates {
        source: "published_pr_revision",
        runs: selected,
        commits,
        notice: None,
    })
}
fn same_or_descendant(repo: &Repo, head: &str, published: &str) -> bool {
    let (Ok(head), Ok(published)) = (git2::Oid::from_str(head), git2::Oid::from_str(published))
    else {
        return false;
    };
    head == published
        || repo
            .git_repo
            .graph_descendant_of(head, published)
            .unwrap_or(false)
}
async fn branch_candidates(
    repo: &Repo,
    reference: &RepoRef,
    target: &Target,
    scope: &crate::ci_history::Scope,
    runs: Vec<WorkflowRun>,
) -> Result<Candidates> {
    let Target::Commit { ids, .. } = target else {
        unreachable!()
    };
    let head = ids.first().map(String::as_str).unwrap_or_default();
    let Ok(branch) = repo.get_checked_out_branch_name() else {
        return Ok(Candidates::unavailable(
            "Detached HEAD: no published branch inferred.",
        ));
    };
    let config = repo.git_repo.config()?;
    let Ok(merge) = config.get_string(&format!("branch.{branch}.merge")) else {
        return Ok(Candidates::unavailable(
            "No upstream branch configured; no previous tip inferred.",
        ));
    };
    let Some(remote) = repo.get_upstream_remote_for_branch(&branch)? else {
        return Ok(Candidates::unavailable(
            "No upstream remote configured; no previous tip inferred.",
        ));
    };
    if scope.selected_remote.as_deref() != Some(remote.as_str()) {
        return Ok(Candidates::unavailable(
            "The upstream remote cannot be verified as the selected repository remote; no previous tip inferred.",
        ));
    }
    let nostr_remote = repo
        .git_repo
        .find_remote(&remote)
        .is_ok_and(|r| r.url().is_ok_and(|url| url.starts_with("nostr://")));
    if !nostr_remote {
        return Ok(Candidates::unavailable(
            "Upstream publication was not refreshed from Nostr; no previous tip inferred.",
        ));
    }
    let state = get_state_from_cache(Some(repo.get_path()?), reference)
        .await
        .ok();
    let Some(published) = state.as_ref().and_then(|s| s.state.get(&merge)) else {
        return Ok(Candidates::unavailable(
            "Published branch tip unavailable; no previous tip inferred.",
        ));
    };
    if !same_or_descendant(repo, head, published) {
        return Ok(Candidates::unavailable(
            "Local HEAD is behind, divergent from, or cannot be compared with the published branch tip; no previous tip inferred.",
        ));
    }
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|r| r.pr_root.is_none() && r.common().git_ref() == Some(merge.as_str()))
        .collect();
    let mut walk = repo.git_repo.revwalk()?;
    walk.push(git2::Oid::from_str(published)?)?;
    walk.simplify_first_parent()?;
    let commits = walk
        .filter_map(Result::ok)
        .map(|id| id.to_string())
        .filter(|id| id != head && runs.iter().any(|r| r.commit() == id))
        .take(1)
        .collect();
    Ok(Candidates{source:"repository_branch_history",runs,commits,notice:Some("Published branch context comes from retained repository state; relay coverage may be incomplete.".into())})
}
