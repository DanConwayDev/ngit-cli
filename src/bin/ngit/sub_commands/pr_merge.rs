use anyhow::{Context, Result, bail};
use ngit::{
    ci::trust::Coverage,
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache, send_events,
    },
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, get_status,
        sign_ordered_status_event, status_kinds, tag_value,
    },
};
use nostr::prelude::{
    EventBuilder, Kind, Tag,
    nip01::Nip01Tag,
    nip10::{Marker, Nip10Tag},
};
use serde_json::{Value, json};

use crate::{
    ci_projection::{
        CiReport, ProjectionRequest, Tier, build_report, pull_request_target, relay_coverage,
    },
    cli::{CiTrustFloor, SignerParams},
    client::{
        Client, Connect, get_events_from_local_cache, get_repo_ref_from_cache,
        warn_if_invited_as_maintainer,
    },
    git::{Repo, RepoActions, str_to_sha1},
    git_events::event_to_cover_letter,
    login,
    repo_ref::get_repo_coordinates_for_publishing,
    sub_commands::{
        id_resolver::{pr_description, proposal_roots, resolve_pr_root_or_prefix},
        repository_fetch::fetching_with_account,
    },
};

#[allow(clippy::too_many_lines)]
pub async fn launch(
    id: &str,
    squash: bool,
    require_ci_trust: Option<CiTrustFloor>,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let mut repo_coordinates = get_repo_coordinates_for_publishing(&git_repo, &mut client).await?;

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
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    // Login to verify maintainer status
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        auth.info,
        auth.password,
        Some(&client),
        true,
    )
    .await?;

    let user_pubkey = signer.get_public_key().await?;

    if !repo_ref.is_authorized_maintainer(&user_pubkey) {
        bail!("only a repository maintainer can merge a PR");
    }

    let proposals_and_revisions =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    let proposal =
        resolve_pr_root_or_prefix(id, proposals_and_revisions.iter(), pr_description)?.clone();

    // Check current status — only open/draft PRs can be merged
    let statuses = {
        let mut s = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
                nostr::prelude::Filter::default()
                    .custom_tags(
                        nostr::filter::SingleLetterTag::UPPERCASE_E,
                        proposals_and_revisions.iter().map(|e| e.id),
                    )
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        s.sort_by_key(|e| e.created_at);
        s.reverse();
        s
    };

    let proposals_vec: Vec<nostr::prelude::Event> =
        proposal_roots(&proposals_and_revisions).cloned().collect();

    let current_status = get_status(&proposal, &repo_ref, &statuses, &proposals_vec);

    if current_status == Kind::GitStatusApplied {
        bail!("PR is already applied/merged");
    }
    if current_status == Kind::GitStatusClosed {
        bail!("PR is closed; reopen it before merging");
    }

    // The Checks summary comes before anything this command changes, so a
    // refused `--require-ci-trust` leaves neither a branch nor a merge behind.
    // It is `ngit ci status`'s own projection, at the same full tier: the
    // maintainer deciding to merge is the reader the evidence is for.
    let ci_input_coverage = fetch_report.as_ref().map_or(Coverage::Complete, |report| {
        relay_coverage(&repo_ref.relays, &report.state_per_relay)
    });
    let ci_target = pull_request_target(git_repo_path, &repo_ref, proposal.id).await?;
    let ci = build_report(
        &git_repo,
        git_repo_path,
        &repo_ref,
        &client,
        &ProjectionRequest {
            target: &ci_target,
            tier: if offline { Tier::Cache } else { Tier::Full },
            // A merge is a decision about the current revision; earlier ones
            // are `ngit pr view`'s business.
            include_outdated: false,
            input_coverage: ci_input_coverage,
        },
    )
    .await?;
    if ci.has_results() {
        ci.print_checks();
    }

    if let Some(reason) = require_ci_trust.and_then(|floor| ci.gate_failure(floor)) {
        if crate::output::is_json() {
            crate::output::set_value(merge_json(
                proposal.id,
                repo_ref.relays.first(),
                None,
                &ci,
                None,
                Some(&reason),
            ));
        }
        println!("{}", console::style(&reason).red());
        // The refusal is this command's output, not a failure to produce it:
        // `main`'s error path would discard the runs that explain it.
        crate::output::finish_and_exit(1);
    }

    // No floor was demanded, so a result that would not have met the default
    // one is a prompt to look rather than a refusal.
    let ci_warning = if require_ci_trust.is_some() {
        None
    } else {
        ci.merge_warning()
    };
    if let Some(warning) = &ci_warning {
        println!("{}", console::style(format!("warning: {warning}")).yellow());
    }

    let cover_letter = event_to_cover_letter(&proposal).context("failed to extract PR details")?;

    let branch_name = cover_letter.get_branch_name_with_pr_prefix_and_shorthand_id()?;

    // Get the PR tip commit
    let commits_events = get_all_proposal_patch_pr_pr_update_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal.id,
    )
    .await?;

    let tip_chain = get_pr_tip_event_or_most_recent_patch_with_ancestors(commits_events)
        .context("failed to find any PR or patch events on this proposal")?;

    let tip_commit_str = if tip_chain
        .iter()
        .any(|e| [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&e.kind))
    {
        let tip_event = tip_chain.first().context("tip chain is empty")?;
        tag_value(tip_event, "c").context("PR event missing tip commit tag 'c'")?
    } else {
        ngit::git_events::get_commit_id_from_patch(
            tip_chain.first().context("patch chain is empty")?,
        )
        .context("failed to get commit id from patch")?
    };

    let _tip_commit = str_to_sha1(&tip_commit_str).context("invalid tip commit OID")?;

    // Ensure the branch exists locally
    let local_branch_exists = git_repo
        .get_local_branch_names()
        .context("failed to get local branch names")?
        .iter()
        .any(|n| n.eq(&branch_name));

    if !local_branch_exists {
        // Try to create the branch at the tip commit
        if !git_repo.does_commit_exist(&tip_commit_str)? {
            bail!(
                "PR tip commit {tip_commit_str} not found locally. Run `ngit pr checkout {id}` first."
            );
        }
        git_repo.create_branch_at_commit(&branch_name, &tip_commit_str)?;
        println!("created local branch '{branch_name}' at PR tip");
    }

    // Perform the git merge
    let merge_args = if squash {
        vec!["merge", "--squash", &branch_name]
    } else {
        vec!["merge", "--no-ff", &branch_name]
    };

    let output = std::process::Command::new("git")
        .args(&merge_args)
        .output()
        .context("failed to run git merge")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git merge failed:\n{stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        print!("{stdout}");
    }

    // Publish GitStatusApplied event
    let mut public_keys: std::collections::HashSet<nostr::prelude::PublicKey> =
        repo_ref.maintainers.iter().copied().collect();
    public_keys.insert(proposal.pubkey);

    let alt_tag = Tag::parse(["alt", "PR merged"])?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let applied_event = sign_ordered_status_event(
        EventBuilder::new(Kind::GitStatusApplied, "").tags(
            [
                vec![
                    alt_tag,
                    Tag::from(Nip10Tag::Event {
                        id: proposal.id,
                        relay_hint: repo_ref.relays.first().cloned(),
                        marker: Some(Marker::Root),
                        public_key: None,
                    }),
                ],
                public_keys.iter().map(|pk| Tag::public_key(*pk)).collect(),
                repo_ref
                    .coordinates()
                    .iter()
                    .map(|c| {
                        Tag::from(Nip01Tag::Coordinate {
                            coordinate: c.coordinate.clone(),
                            relay_hint: c.relays.first().cloned(),
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![r_tag],
            ]
            .concat(),
        ),
        &signer,
        &statuses,
        proposal.id,
        "mark PR as applied".to_string(),
    )
    .await?;
    let applied_event_id = applied_event.id;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![applied_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    if crate::output::is_json() {
        crate::output::set_value(merge_json(
            proposal.id,
            repo_ref.relays.first(),
            Some(applied_event_id),
            &ci,
            ci_warning.as_deref(),
            None,
        ));
    }

    println!("PR '{}' merged and marked as applied", cover_letter.title);
    println!(
        "{}",
        console::style("Push to update the nostr state: git push").yellow()
    );

    Ok(())
}

/// The document a CI-aware merge emits, merged or refused.
///
/// It carries the `ci` object every CI surface shares, plus `ci_warning`: the
/// non-blocking caveat the human output prints, as a field rather than as
/// prose. The key is always present — `null` when the result gave no cause
/// for one — so a consumer never branches on a missing key. It is always
/// `null` when `--require-ci-trust` was passed: there the shortfall is either
/// absent or the refusal in `error`.
///
/// A refusal is the one outcome with an `error` and no `event`, so `status`
/// and `action` are derived from it rather than passed in beside it.
pub(super) fn merge_json(
    proposal_id: nostr::prelude::EventId,
    relay: Option<&nostr::prelude::RelayUrl>,
    applied_event: Option<nostr::prelude::EventId>,
    ci: &CiReport,
    ci_warning: Option<&str>,
    error: Option<&str>,
) -> Value {
    let mut document = json!({
        "command_status": if error.is_some() { "error" } else { "ok" },
        "action": if error.is_some() { "refused" } else { "merged" },
        "entity": "pr",
        "id": crate::output::event_id_to_nevent(proposal_id, relay),
        "ci": ci.to_ci_value(relay),
        "ci_warning": ci_warning,
    });
    if let Some(applied_event) = applied_event {
        document["event"] = json!(crate::output::event_id_to_nevent(applied_event, relay));
    }
    if let Some(error) = error {
        document["error"] = json!(error);
    }
    document
}
