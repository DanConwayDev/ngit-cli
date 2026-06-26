use anyhow::{Context, Result, bail};
use ngit::{
    client::{
        Params, get_all_proposal_patch_pr_pr_update_events_from_cache,
        get_proposals_and_revisions_from_cache, send_events, sign_event,
    },
    git_events::{
        KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        get_pr_tip_event_or_most_recent_patch_with_ancestors, get_status, status_kinds, tag_value,
    },
};
use nostr::{
    EventBuilder, Kind, Tag,
    nips::{
        nip01::Nip01Tag,
        nip10::{Marker, Nip10Tag},
    },
};

use crate::{
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache,
        get_repo_ref_from_cache, warn_if_invited_as_maintainer,
    },
    git::{Repo, RepoActions, str_to_sha1},
    git_events::event_to_cover_letter,
    login,
    repo_ref::get_repo_coordinates_when_remote_unknown,
    sub_commands::id_resolver::{pr_description, proposal_roots, resolve_pr_root_or_prefix},
};

#[allow(clippy::too_many_lines)]
pub async fn launch(id: &str, squash: bool, offline: bool) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    // Login to verify maintainer status
    let (signer, user_ref, _) =
        login::login_or_signup(&Some(&git_repo), &None, &None, Some(&client), true).await?;

    let user_pubkey = signer.get_public_key().await?;

    if !repo_ref.maintainers.contains(&user_pubkey) {
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
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals_and_revisions.iter().map(|e| e.id)),
                nostr::Filter::default()
                    .custom_tags(
                        nostr::filter::SingleLetterTag::uppercase(nostr::filter::Alphabet::E),
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

    let proposals_vec: Vec<nostr::Event> =
        proposal_roots(&proposals_and_revisions).cloned().collect();

    let current_status = get_status(&proposal, &repo_ref, &statuses, &proposals_vec);

    if current_status == Kind::GitStatusApplied {
        bail!("PR is already applied/merged");
    }
    if current_status == Kind::GitStatusClosed {
        bail!("PR is closed; reopen it before merging");
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
    let mut public_keys: std::collections::HashSet<nostr::PublicKey> =
        repo_ref.maintainers.iter().copied().collect();
    public_keys.insert(proposal.pubkey);

    let alt_tag = Tag::parse(["alt", "PR merged"])?;
    let r_tag = Tag::parse(["r", &repo_ref.root_commit])?;
    let applied_event = sign_event(
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
        "mark PR as applied".to_string(),
    )
    .await?;

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

    println!("PR '{}' merged and marked as applied", cover_letter.title);
    println!(
        "{}",
        console::style("Push to update the nostr state: git push").yellow()
    );

    Ok(())
}
