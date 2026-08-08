use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use bitcoin_hashes::sha1::Hash as Sha1Hash;
use nostr::prelude::{Event, EventId, FromBech32, Nip19};

use crate::{
    client::get_all_proposal_patch_pr_pr_update_events_from_cache,
    git::{Repo, RepoActions, str_to_sha1},
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE, get_commit_id_from_patch, tag_value,
    },
    repo_ref::RepoRef,
};

pub type Proposals = HashMap<EventId, (Event, Vec<Event>, Option<Event>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplicitBase {
    pub commit: Sha1Hash,
    pub description: String,
}

fn event_id(reference: &str) -> Option<EventId> {
    match Nip19::from_bech32(reference).ok() {
        Some(Nip19::Event(event)) => Some(event.event_id),
        Some(Nip19::EventId(id)) => Some(id),
        _ => EventId::parse(reference).ok(),
    }
}

fn resolve_event_id_or_prefix(reference: &str, events: &[Event]) -> Result<Option<EventId>> {
    let reference = reference.trim();
    if !reference.starts_with('#') {
        if let Some(id) = event_id(reference) {
            return Ok(Some(id));
        }
    }

    let prefix = reference.strip_prefix('#').unwrap_or(reference);
    if prefix.is_empty() || prefix.len() >= 64 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }

    let matches: Vec<&Event> = events
        .iter()
        .filter(|event| event.id.to_hex().starts_with(&prefix.to_ascii_lowercase()))
        .collect();
    match matches.as_slice() {
        [] => bail!(
            "couldn't find a PR or PR update matching event-id prefix #{prefix} in this repository"
        ),
        [event] => Ok(Some(event.id)),
        _ => bail!(
            "event-id prefix #{prefix} matches multiple PR or PR update events:\n{}\nspecify a longer prefix, full event-id, or nevent",
            matches
                .iter()
                .map(|event| format!(
                    "  #{} kind:{}",
                    &event.id.to_hex()[..8],
                    event.kind.as_u16()
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

pub fn visible_branch_tip(git_repo: &Repo, branch: &str) -> Result<Option<Sha1Hash>> {
    let tips = git_repo.get_branch_tips(branch)?;
    if tips.is_empty() {
        return Ok(None);
    }

    let most_advanced = tips.iter().find(|candidate| {
        tips.iter().all(|other| {
            candidate == &other || git_repo.ancestor_of(candidate, other).unwrap_or(false)
        })
    });
    match most_advanced {
        Some(tip) => Ok(Some(*tip)),
        None => {
            bail!("branch '{branch}' has divergent local or remote tips; specify a commit instead")
        }
    }
}

/// Validate a proposal target and resolve the newest unambiguous visible tip.
///
/// `reject_default` is used for new proposals, where spelling out the current
/// default is redundant. Existing proposals retain their immutable target even
/// if the repository default later changes to that branch.
pub fn resolve_target_branch_tip(
    git_repo: &Repo,
    branch: &str,
    declared_default: Option<&str>,
    reject_default: bool,
) -> Result<Sha1Hash> {
    if branch.is_empty() || !git2::Reference::is_valid_name(&format!("refs/heads/{branch}")) {
        bail!("invalid target branch name '{branch}'");
    }
    let tip = visible_branch_tip(git_repo, branch)?.with_context(|| {
        format!("target branch '{branch}' does not exist locally or on a remote")
    })?;
    if reject_default
        && git_repo
            .get_default_branch_name(declared_default)?
            .as_deref()
            == Some(branch)
    {
        bail!("target branch '{branch}' is the repository default; omit target-branch");
    }
    Ok(tip)
}

/// Preserve the latest event's merge base when `tip` is a fast-forward update.
///
/// A rewritten proposal must recompute its fork point (or supply `--base`), but
/// a fast-forward update must not silently collapse an explicit stack base back
/// to the repository default.
pub fn merge_base_for_fast_forward_update(
    git_repo: &Repo,
    latest: &Event,
    tip: &Sha1Hash,
) -> Result<Option<Sha1Hash>> {
    let previous_tip = git_repo
        .get_commit_or_tip_of_reference(&get_commit_id_from_patch(latest)?)
        .context("previous PR tip is not available locally")?;
    if previous_tip != *tip && !git_repo.ancestor_of(tip, &previous_tip)? {
        return Ok(None);
    }

    let Ok(value) = tag_value(latest, "merge-base") else {
        return Ok(None);
    };
    let merge_base = str_to_sha1(&value).context("previous PR merge-base tag is invalid")?;
    let (_, behind) = git_repo.get_commits_ahead_behind(&merge_base, tip)?;
    if behind.is_empty() {
        Ok(Some(merge_base))
    } else {
        Ok(None)
    }
}

/// Resolve a user-selected proposal base from the repository state already
/// fetched by ngit.
pub async fn resolve_explicit_base(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    reference: &str,
    proposals: &Proposals,
) -> Result<ExplicitBase> {
    if let Some(commit) = visible_branch_tip(git_repo, reference)? {
        return Ok(ExplicitBase {
            commit,
            description: format!("branch '{reference}'"),
        });
    }

    if let Ok(commit) = git_repo.get_commit_or_tip_of_reference(reference) {
        return Ok(ExplicitBase {
            commit,
            description: format!("git revision '{reference}'"),
        });
    }

    let mut proposal_events = Vec::new();
    for (root, _, pr_upgrade_root) in proposals.values() {
        proposal_events.push(root.clone());
        if let Some(upgrade) = pr_upgrade_root {
            proposal_events.push(upgrade.clone());
        }
        proposal_events.extend(
            get_all_proposal_patch_pr_pr_update_events_from_cache(
                git_repo.get_path()?,
                repo_ref,
                &root.id,
            )
            .await?
            .into_iter()
            .filter(|event| event.kind == KIND_PULL_REQUEST_UPDATE),
        );
    }
    proposal_events.sort_by_key(|event| event.id);
    proposal_events.dedup_by_key(|event| event.id);

    let id = resolve_event_id_or_prefix(reference, &proposal_events)?.with_context(|| {
        format!("base '{reference}' is not a Git revision, branch, or PR event reference")
    })?;

    if let Some((root, events, _)) = proposals.get(&id) {
        if root.kind != KIND_PULL_REQUEST {
            bail!("base event '{reference}' is not a PR in this repository");
        }
        let latest = events.first().context("base PR has no authorized tip")?;
        let commit = git_repo.get_commit_or_tip_of_reference(&get_commit_id_from_patch(latest)?)?;
        return Ok(ExplicitBase {
            commit,
            description: "the latest version of the selected PR".to_string(),
        });
    }

    if let Some((_, events, Some(_))) = proposals
        .values()
        .find(|(_, _, upgrade)| upgrade.as_ref().is_some_and(|event| event.id == id))
    {
        let latest = events.first().context("base PR has no authorized tip")?;
        let commit = git_repo.get_commit_or_tip_of_reference(&get_commit_id_from_patch(latest)?)?;
        return Ok(ExplicitBase {
            commit,
            description: "the latest version of the selected PR".to_string(),
        });
    }

    if let Some(update) = proposal_events
        .iter()
        .find(|event| event.id == id && event.kind == KIND_PULL_REQUEST_UPDATE)
    {
        let commit = git_repo.get_commit_or_tip_of_reference(&get_commit_id_from_patch(update)?)?;
        return Ok(ExplicitBase {
            commit,
            description: "the selected historical PR update".to_string(),
        });
    }

    bail!("couldn't find base event '{reference}' among PRs or PR updates for this repository")
}

pub fn commits_after_base(
    git_repo: &Repo,
    base: &ExplicitBase,
    tip: &Sha1Hash,
) -> Result<Vec<Sha1Hash>> {
    let (mut ahead, behind) = git_repo.get_commits_ahead_behind(&base.commit, tip)?;
    if !behind.is_empty() {
        bail!("proposal tip is not descended from {}", base.description);
    }
    if ahead.is_empty() {
        bail!("proposal has no commits after {}", base.description);
    }
    ahead.reverse();
    Ok(ahead)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use nostr::{
        event::{EventBuilder, FinalizeEvent},
        prelude::{Tag, ToBech32},
    };

    use super::*;
    use crate::git::{oid_to_sha1, test_helpers::GitTestRepo};

    #[test]
    fn git_revision_resolves_and_base_must_leave_commits() -> Result<()> {
        let fixture = GitTestRepo::default();
        fixture.populate()?;
        let git_repo = Repo::from_path(&fixture.dir)?;
        let head = git_repo.get_head_commit()?;

        let base = ExplicitBase {
            commit: git_repo.get_commit_or_tip_of_reference("HEAD~1")?,
            description: "git revision 'HEAD~1'".to_string(),
        };
        assert_eq!(commits_after_base(&git_repo, &base, &head)?.len(), 1);

        let head_as_base = ExplicitBase {
            commit: head,
            description: "HEAD".to_string(),
        };
        assert!(commits_after_base(&git_repo, &head_as_base, &head).is_err());
        Ok(())
    }

    #[test]
    fn divergent_visible_branch_tips_require_an_explicit_commit() -> Result<()> {
        let fixture = GitTestRepo::default();
        fixture.initial_commit()?;
        fixture.create_branch("left")?;
        fixture.checkout("left")?;
        fs::write(fixture.dir.join("left"), "left")?;
        let left = fixture.stage_and_commit("left")?;
        fixture.checkout("main")?;
        fixture.create_branch("right")?;
        fixture.checkout("right")?;
        fs::write(fixture.dir.join("right"), "right")?;
        let right = fixture.stage_and_commit("right")?;
        fixture
            .git_repo
            .remote("one", "https://one.example/repo.git")?;
        fixture
            .git_repo
            .remote("two", "https://two.example/repo.git")?;
        fixture
            .git_repo
            .reference("refs/remotes/one/topic", left, true, "test branch tip")?;
        fixture
            .git_repo
            .reference("refs/remotes/two/topic", right, true, "test branch tip")?;

        let git_repo = Repo::from_path(&fixture.dir)?;
        let error = visible_branch_tip(&git_repo, "topic").unwrap_err();
        assert!(error.to_string().contains("divergent"));
        assert_ne!(oid_to_sha1(&left), oid_to_sha1(&right));
        let left_base = ExplicitBase {
            commit: oid_to_sha1(&left),
            description: "left branch".to_string(),
        };
        assert!(
            commits_after_base(&git_repo, &left_base, &oid_to_sha1(&right))
                .unwrap_err()
                .to_string()
                .contains("not descended")
        );
        Ok(())
    }

    #[test]
    fn event_prefixes_resolve_uniquely_and_report_missing_matches() -> Result<()> {
        let event = EventBuilder::new(KIND_PULL_REQUEST, "proposal")
            .finalize(&nostr::prelude::Keys::generate())?;
        let shorthand = format!("#{}", &event.id.to_hex()[..8]);

        assert_eq!(
            resolve_event_id_or_prefix(&shorthand, std::slice::from_ref(&event))?,
            Some(event.id)
        );
        assert!(
            resolve_event_id_or_prefix("#00000000", &[event])
                .unwrap_err()
                .to_string()
                .contains("couldn't find")
        );
        Ok(())
    }

    #[test]
    fn event_references_accept_nevents_and_reject_ambiguous_prefixes() -> Result<()> {
        let keys = nostr::prelude::Keys::generate();
        let events = (0..17)
            .map(|index| {
                EventBuilder::new(KIND_PULL_REQUEST, format!("proposal {index}")).finalize(&keys)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let nevent = nostr::prelude::Nip19Event {
            event_id: events[0].id,
            relays: vec![],
            author: Some(events[0].pubkey),
            kind: Some(events[0].kind),
        }
        .to_bech32()?;
        assert_eq!(
            resolve_event_id_or_prefix(&nevent, &events)?,
            Some(events[0].id)
        );

        let mut by_first_hex: HashMap<char, Vec<&Event>> = HashMap::new();
        for event in &events {
            by_first_hex
                .entry(event.id.to_hex().chars().next().unwrap())
                .or_default()
                .push(event);
        }
        let prefix = by_first_hex
            .iter()
            .find(|(_, matches)| matches.len() > 1)
            .map(|(prefix, _)| format!("#{prefix}"))
            .context("17 event IDs must share at least one hexadecimal prefix")?;
        assert!(
            resolve_event_id_or_prefix(&prefix, &events)
                .unwrap_err()
                .to_string()
                .contains("matches multiple")
        );
        Ok(())
    }

    #[test]
    fn target_resolution_prefers_an_advanced_remote_over_a_stale_local_branch() -> Result<()> {
        let fixture = GitTestRepo::default();
        let advanced = fixture.populate()?;
        let stale = fixture.git_repo.find_commit(advanced)?.parent_id(0)?;
        fixture
            .git_repo
            .remote("origin", "https://example.com/repo.git")?;
        fixture
            .git_repo
            .branch("release/2.x", &fixture.git_repo.find_commit(stale)?, false)?;
        fixture.git_repo.reference(
            "refs/remotes/origin/release/2.x",
            advanced,
            true,
            "advanced remote target",
        )?;
        let git_repo = Repo::from_path(&fixture.dir)?;

        assert_eq!(
            resolve_target_branch_tip(&git_repo, "release/2.x", None, true)?,
            oid_to_sha1(&advanced)
        );
        Ok(())
    }

    #[test]
    fn target_resolution_does_not_match_a_nested_branch_suffix() -> Result<()> {
        let fixture = GitTestRepo::default();
        let tip = fixture.populate()?;
        fixture
            .git_repo
            .remote("origin", "https://example.com/repo.git")?;
        fixture.git_repo.reference(
            "refs/remotes/origin/release/2.x",
            tip,
            true,
            "nested remote branch",
        )?;
        let git_repo = Repo::from_path(&fixture.dir)?;

        assert!(resolve_target_branch_tip(&git_repo, "2.x", None, true).is_err());
        assert_eq!(
            resolve_target_branch_tip(&git_repo, "release/2.x", None, true)?,
            oid_to_sha1(&tip)
        );
        Ok(())
    }

    #[test]
    fn fast_forward_updates_preserve_merge_base_but_rewrites_recompute_it() -> Result<()> {
        let fixture = GitTestRepo::default();
        let base = fixture.populate()?;
        fixture.create_branch("pr/feature")?;
        fixture.checkout("pr/feature")?;
        fs::write(fixture.dir.join("feature-one"), "one")?;
        let previous_tip = fixture.stage_and_commit("feature one")?;
        let latest = EventBuilder::new(KIND_PULL_REQUEST, "proposal")
            .tags([
                Tag::parse(["c", &previous_tip.to_string()])?,
                Tag::parse(["merge-base", &base.to_string()])?,
            ])
            .finalize(&nostr::prelude::Keys::generate())?;

        fs::write(fixture.dir.join("feature-two"), "two")?;
        let fast_forward_tip = fixture.stage_and_commit("feature two")?;
        let git_repo = Repo::from_path(&fixture.dir)?;
        assert_eq!(
            merge_base_for_fast_forward_update(
                &git_repo,
                &latest,
                &oid_to_sha1(&fast_forward_tip)
            )?,
            Some(oid_to_sha1(&base))
        );

        fixture.checkout("main")?;
        fixture.create_branch("pr/rewritten")?;
        fixture.checkout("pr/rewritten")?;
        fs::write(fixture.dir.join("rewritten"), "rewritten")?;
        let rewritten_tip = fixture.stage_and_commit("rewritten feature")?;
        assert_eq!(
            merge_base_for_fast_forward_update(&git_repo, &latest, &oid_to_sha1(&rewritten_tip))?,
            None
        );
        Ok(())
    }
}
