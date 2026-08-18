//! `ngit ci status` — CI results for a PR, a commit-ish, or HEAD, with the
//! trust context of every signer behind them.
//!
//! The CI events themselves arrive through the repository fetch every PR
//! command performs (`client::get_filter_ci_events`); this command resolves
//! its target and hands it to [`crate::ci_projection`], the projection
//! `ngit pr view` and `ngit pr list` render from as well.

use std::path::Path;

use anyhow::{Context, Result, bail};
use ngit::{
    ci::trust::Coverage,
    client::{
        Client, Connect, Params, get_events_from_local_cache,
        get_proposals_and_revisions_from_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions},
    git_events::event_is_revision_root,
    repo_ref::RepoRef,
};
use nostr::prelude::{Event, EventId, Filter};

use crate::{
    ci_projection::{
        ProjectionRequest, Target, Tier, build_report, pull_request_target, relay_coverage, short,
    },
    cli::{CiTrustFloor, SignerParams},
    repo_ref::get_repo_coordinates_when_remote_unknown,
    sub_commands::{
        id_resolver::{parse_event_id, pr_description, resolve_pr_root_or_prefix},
        repository_fetch::fetching_with_account,
    },
};

/// Run `ngit ci status`.
///
/// # Errors
///
/// Returns an error when the repository, the target, or the local cache
/// cannot be read. A failed `--require-ci-trust` check is not an error: the
/// document is emitted in full and the process exits non-zero.
pub async fn launch(
    target: Option<&str>,
    offline: bool,
    require_ci_trust: Option<CiTrustFloor>,
    json: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let mut repo_coordinates =
        get_repo_coordinates_when_remote_unknown(&git_repo, &mut client).await?;

    let fetch_report = if offline {
        None
    } else {
        Some(
            fetching_with_account(
                &git_repo,
                git_repo_path,
                &mut client,
                &mut repo_coordinates,
                auth,
            )
            .await?,
        )
    };

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    // A repository relay that did not answer leaves this view incomplete
    // whatever the trust layer finds locally.
    let input_coverage = fetch_report.as_ref().map_or(Coverage::Complete, |report| {
        relay_coverage(&repo_ref.relays, &report.state_per_relay)
    });
    let target = resolve_target(&git_repo, git_repo_path, &repo_ref, target).await?;
    let report = build_report(
        &git_repo,
        git_repo_path,
        &repo_ref,
        &client,
        &ProjectionRequest {
            target: &target,
            tier: if offline { Tier::Cache } else { Tier::Full },
            // `ci status` describes one revision; the detail view is where
            // earlier ones are listed.
            include_outdated: false,
            input_coverage,
        },
    )
    .await?;

    let gate = require_ci_trust.and_then(|floor| report.gate_failure(floor));

    if json {
        crate::output::set_value(report.to_json(&target, repo_ref.relays.first(), gate.as_deref()));
    }
    report.print(&target);

    if let Some(reason) = gate {
        println!("{reason}");
        // The document is this command's real output, so it is emitted before
        // exiting: `main`'s error path would replace it with an error document
        // and lose the runs that explain the refusal.
        crate::output::finish_and_exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// Resolve `<target>` in the documented order.
///
/// 1. `#<hex-prefix>` is always an event prefix.
/// 2. An nevent/note or a full 64-character hex id is always an event id, and
///    must name a cached PR root or one of its revisions.
/// 3. Otherwise a commit-ish, resolved by git.
/// 4. A bare short hex that is not a commit-ish falls back to a PR prefix.
/// 5. No target is HEAD.
async fn resolve_target(
    git_repo: &Repo,
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    target: Option<&str>,
) -> Result<Target> {
    let Some(raw) = target.map(str::trim).filter(|raw| !raw.is_empty()) else {
        let head = git_repo
            .get_head_commit()
            .context("failed to resolve HEAD; is this an empty repository?")?;
        return Ok(Target::Commit {
            ids: vec![head.to_string()],
            described: "HEAD".to_owned(),
        });
    };

    let proposals =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    if raw.starts_with('#') {
        let root = resolve_pr_root_or_prefix(raw, proposals.iter(), pr_description)?;
        return pull_request_target(git_repo_path, repo_ref, root.id).await;
    }

    if let Ok(event_id) = parse_event_id(raw) {
        let root = pr_root_for_event(git_repo_path, &proposals, event_id).await?;
        return pull_request_target(git_repo_path, repo_ref, root.id).await;
    }

    if let Some(commit) = commit_ish_target(git_repo, raw) {
        return Ok(commit);
    }

    if is_hex_prefix(raw) {
        let root = resolve_pr_root_or_prefix(raw, proposals.iter(), pr_description).with_context(
            || {
                format!(
                    "`{raw}` is not a commit-ish in this repository, so it was resolved as a PR event-id prefix; \
                     use `#{raw}` to skip the commit-ish attempt"
                )
            },
        )?;
        return pull_request_target(git_repo_path, repo_ref, root.id).await;
    }

    bail!(
        "`{raw}` is neither a commit-ish nor an event id; use `#<hex-prefix>`, an nevent, or a full event id for a PR"
    );
}

/// Map an exact event id onto the PR thread root it belongs to.
///
/// A revision root — a patch revision, or the kind-1618 root of an upgraded
/// patch thread — resolves to the proposal it revises, as does a kind-1619
/// update, so `ngit ci status <nevent-of-a-revision>` describes that PR
/// rather than failing.
async fn pr_root_for_event(
    git_repo_path: &Path,
    proposals: &[Event],
    event_id: EventId,
) -> Result<Event> {
    if let Some(root) = proposals
        .iter()
        .find(|event| event.id == event_id && !event_is_revision_root(event))
    {
        return Ok(root.clone());
    }

    let cached = get_events_from_local_cache(git_repo_path, vec![Filter::default().id(event_id)])
        .await?
        .into_iter()
        .next()
        .with_context(|| format!("no cached PR or revision with id {event_id}"))?;

    let root_id = root_reference(&cached).with_context(|| {
        format!("cached event {event_id} is not a PR root and names no PR root")
    })?;
    proposals
        .iter()
        .find(|event| event.id == root_id)
        .cloned()
        .with_context(|| format!("PR {root_id}, revised by {event_id}, is not in the cache"))
}

/// The proposal a revision names, in decreasing order of confidence: the
/// NIP-22 `E` tag of a kind-1619 update, a NIP-10 `e` tag marked `root` on a
/// patch revision, and finally an unmarked `e` — which is the shape a
/// patch-to-PR upgrade root uses to back-reference the patch it upgrades
/// (`git_events::generate_unsigned_pr_or_update_event`).
fn root_reference(event: &Event) -> Option<EventId> {
    let referenced = |name: &'static str, marked_root: bool| {
        event
            .tags
            .iter()
            .filter(move |tag| {
                tag.as_slice().first().is_some_and(|first| first == name)
                    && (!marked_root
                        || tag.as_slice().get(3).is_some_and(|marker| marker == "root"))
            })
            .filter_map(|tag| tag.as_slice().get(1))
            .find_map(|value| EventId::parse(value).ok())
    };
    referenced("E", false)
        .or_else(|| referenced("e", true))
        .or_else(|| referenced("e", false))
}

/// Resolve `raw` as a commit-ish, peeling an annotated tag to its commit
/// while keeping the tag object id as a second `#c` candidate.
fn commit_ish_target(git_repo: &Repo, raw: &str) -> Option<Target> {
    let object = git_repo.git_repo.revparse_single(raw).ok()?;
    let commit = object.peel_to_commit().ok()?;
    let mut ids = vec![commit.id().to_string()];
    let object_id = object.id().to_string();
    if object_id != ids[0] {
        ids.push(object_id);
    }
    Some(Target::Commit {
        described: format!("{raw} ({})", short(&ids[0])),
        ids,
    })
}

fn is_hex_prefix(value: &str) -> bool {
    !value.is_empty() && value.len() < 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use ngit::{git_events::KIND_PULL_REQUEST_UPDATE, utils::pr_upgrade_root};
    use nostr::prelude::Kind;

    use super::*;

    fn event(kind: Kind, tags: Vec<nostr::prelude::Tag>) -> Event {
        use nostr::prelude::event::FinalizeEvent;
        nostr::prelude::EventBuilder::new(kind, "")
            .tags(tags)
            .finalize(&nostr::prelude::Keys::generate())
            .expect("test event finalizes")
    }

    fn tag(values: &[&str]) -> nostr::prelude::Tag {
        nostr::prelude::Tag::parse(
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
        )
        .expect("test tag parses")
    }

    #[test]
    fn a_patch_upgraded_to_a_pr_anchors_on_the_upgrade_root() {
        // The thread root stays the patch; ngit's `pr_upgrade_root` rule
        // decides which event later updates — and CI `E` tags — reference.
        let patch_root = event(Kind::GitPatch, vec![tag(&["t", "root"])]);
        let upgrade_root = event(
            ngit::git_events::KIND_PULL_REQUEST,
            vec![tag(&["e", &patch_root.id.to_hex()])],
        );
        assert!(
            event_is_revision_root(&upgrade_root),
            "the upgrade root is a revision root, so it is not a thread root"
        );
        assert_eq!(
            pr_upgrade_root(&[patch_root.clone(), upgrade_root.clone()]).map(|event| event.id),
            Some(upgrade_root.id),
        );
        assert_eq!(
            root_reference(&upgrade_root),
            Some(patch_root.id),
            "an unmarked `e` is how an upgrade root names the patch it upgrades"
        );
    }

    #[test]
    fn a_revision_names_its_root_through_the_strongest_reference_it_carries() {
        let root = event(Kind::GitPatch, vec![tag(&["t", "root"])]);
        let update = event(
            KIND_PULL_REQUEST_UPDATE,
            vec![
                tag(&["E", &root.id.to_hex()]),
                tag(&["e", &"cc".repeat(32)]),
            ],
        );
        assert_eq!(root_reference(&update), Some(root.id), "`E` wins");

        let revision = event(
            Kind::GitPatch,
            vec![
                tag(&["t", "revision-root"]),
                tag(&["e", &"dd".repeat(32), "", "reply"]),
                tag(&["e", &root.id.to_hex(), "", "root"]),
            ],
        );
        assert_eq!(
            root_reference(&revision),
            Some(root.id),
            "a marked root `e` wins over an unmarked one"
        );
    }

    #[test]
    fn a_short_hex_is_a_prefix_but_a_full_id_is_not() {
        assert!(is_hex_prefix("dead"));
        assert!(!is_hex_prefix(&"a".repeat(64)));
        assert!(!is_hex_prefix("feature-1"));
        assert!(!is_hex_prefix(""));
    }
}
