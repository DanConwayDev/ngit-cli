use std::{io::Write, ops::Add};

use anyhow::{bail, Context, Result};

use super::send::event_is_patch_set_root;
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms},
    client::Connect,
    git::{str_to_sha1, Repo, RepoActions},
    repo_ref::{self, RepoRef, REPO_REF_KIND},
    sub_commands::send::{
        commit_msg_from_patch_oneliner, event_is_cover_letter, event_is_revision_root,
        event_to_cover_letter, patch_supports_commit_ids, PATCH_KIND,
    },
};

#[allow(clippy::too_many_lines)]
pub async fn launch() -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    // TODO: check for empty repo
    // TODO: check for existing maintaiers file
    // TODO: check for other claims

    #[cfg(not(test))]
    let client = Client::default();
    #[cfg(test)]
    let client = <MockConnect as std::default::Default>::default();

    let repo_ref = repo_ref::fetch(
        &git_repo,
        root_commit.to_string(),
        &client,
        client.get_fallback_relays().clone(),
        true,
    )
    .await?;

    println!("finding proposals...");

    let proposal_events_and_revisions: Vec<nostr::Event> =
        find_proposal_events(&client, &repo_ref, &root_commit.to_string()).await?;

    if proposal_events_and_revisions.is_empty() {
        println!("no proposals found... create one? try `ngit send`");
        return Ok(());
    }
    let proposal_events: Vec<&nostr::Event> = proposal_events_and_revisions
        .iter()
        .filter(|e| !event_is_revision_root(e))
        .collect::<Vec<&nostr::Event>>();

    loop {
        let selected_index = Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt("all proposals")
                .with_choices(
                    proposal_events
                        .iter()
                        .map(|e| {
                            if let Ok(cl) = event_to_cover_letter(e) {
                                cl.title
                            } else if let Ok(msg) = tag_value(e, "description") {
                                msg.split('\n').collect::<Vec<&str>>()[0].to_string()
                            } else {
                                e.id.to_string()
                            }
                        })
                        .collect(),
                ),
        )?;

        let cover_letter = event_to_cover_letter(proposal_events[selected_index])
            .context("cannot extract proposal details from proposal root event")?;

        println!("finding commits...");

        let commits_events: Vec<nostr::Event> = find_commits_for_proposal_root_events(
            &client,
            &[
                vec![proposal_events[selected_index]],
                proposal_events_and_revisions
                    .iter()
                    .filter(|e| {
                        e.tags.iter().any(|t| {
                            t.as_vec().len().gt(&1)
                                && t.as_vec()[1].eq(&proposal_events[selected_index].id.to_string())
                        })
                    })
                    .collect::<Vec<&nostr::Event>>(),
            ]
            .concat(),
            &repo_ref,
        )
        .await?;
        // for commit in &commits_events {
        //     println!("cevent: {:?}", commit.as_json());
        // }

        let Ok(most_recent_proposal_patch_chain) =
            get_most_recent_patch_with_ancestors(commits_events.clone())
        else {
            if Interactor::default().confirm(
                PromptConfirmParms::default()
                    .with_default(true)
                    .with_prompt(
                        "cannot find any patches on this proposal. choose another proposal?",
                    ),
            )? {
                continue;
            }
            return Ok(());
        };
        // for commit in &most_recent_proposal_patch_chain {
        //     println!("recent_event: {:?}", commit.as_json());
        // }

        let binding_patch_text_ref = format!("{} commits", most_recent_proposal_patch_chain.len());
        let patch_text_ref = if most_recent_proposal_patch_chain.len().gt(&1) {
            binding_patch_text_ref.as_str()
        } else {
            "1 commit"
        };

        let no_support_for_patches_as_branch = most_recent_proposal_patch_chain
            .iter()
            .any(|event| !patch_supports_commit_ids(event));

        if no_support_for_patches_as_branch {
            println!("{patch_text_ref}");
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        "learn why 'patch only' proposals can't be checked out".to_string(),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    println!("Some proposals are posted as 'patch only'\n");
                    println!(
                        "they are not anchored against a particular state of the code base like a standard proposal or a GitHub Pull Request can be\n"
                    );
                    println!(
                        "they are designed to reviewed by studying the diff (in a tool like gitworkshop.dev) and if acceptable by a maintainer, applied to the latest version of master with any conflicts resolved as the do so\n"
                    );
                    println!(
                        "this has proven to be a smoother workflow for large scale projects with a high frequency of changes, even when patches are exchanged via email\n"
                    );
                    println!(
                        "by default ngit posts proposals that support both the branch and patch model so either workflow can be used"
                    );
                    Interactor::default().choice(
                        PromptChoiceParms::default()
                            .with_default(0)
                            .with_choices(vec!["back".to_string()]),
                    )?;
                    continue;
                }
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let branch_exists = git_repo
            .get_local_branch_names()
            .context("gitlib2 will not show a list of local branch names")?
            .iter()
            .any(|n| n.eq(&cover_letter.branch_name));

        let checked_out_proposal_branch = git_repo
            .get_checked_out_branch_name()?
            .eq(&cover_letter.branch_name);

        let proposal_base_commit = str_to_sha1(&tag_value(
            most_recent_proposal_patch_chain.last().context(
                "there should be at least one patch as we have already checked for this",
            )?,
            "parent-commit",
        )?)
        .context("cannot get valid parent commit id from patch")?;

        let (main_branch_name, master_tip) = git_repo.get_main_or_master_branch()?;

        if !git_repo.does_commit_exist(&proposal_base_commit.to_string())? {
            println!("your '{main_branch_name}' branch may not be up-to-date.");
            println!("the proposal parent commit doesnt exist in your local repository.");
            return match Interactor::default().choice(PromptChoiceParms::default().with_default(0).with_choices(
                vec![
                    format!(
                        "manually run `git pull` on '{main_branch_name}' and select proposal again"
                    ),
                    format!("apply to current branch with `git am`"),
                    format!("download to ./patches"),
                    "back".to_string(),
                ],
            ))? {
                0 | 3 => continue,
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let proposal_tip = str_to_sha1(
            &get_commit_id_from_patch(most_recent_proposal_patch_chain.first().context(
                "there should be at least one patch as we have already checked for this",
            )?)
            .context("cannot get valid commit_id from patch")?,
        )
        .context("cannot get valid commit_id from patch")?;

        let (_, proposal_behind_main) =
            git_repo.get_commits_ahead_behind(&master_tip, &proposal_base_commit)?;

        // branch doesnt exist
        if !branch_exists {
            return match Interactor::default()
                .choice(PromptChoiceParms::default().with_default(0).with_choices(vec![
                format!(
                    "create and checkout proposal branch ({} ahead {} behind '{main_branch_name}')",
                    most_recent_proposal_patch_chain.len(),
                    proposal_behind_main.len(),
                ),
                format!("apply to current branch with `git am`"),
                format!("download to ./patches"),
                "back".to_string(),
            ]))? {
                0 => {
                    check_clean(&git_repo)?;
                    let _ = git_repo
                        .apply_patch_chain(
                            &cover_letter.branch_name,
                            most_recent_proposal_patch_chain,
                        )
                        .context("cannot apply patch chain")?;

                    println!(
                        "checked out proposal as '{}' branch",
                        cover_letter.branch_name
                    );
                    Ok(())
                }
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let local_branch_tip = git_repo.get_tip_of_branch(&cover_letter.branch_name)?;

        // up-to-date
        if proposal_tip.eq(&local_branch_tip) {
            if checked_out_proposal_branch {
                println!("branch checked out and up-to-date");
                return match Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_default(0)
                        .with_choices(vec!["exit".to_string(), "back".to_string()]),
                )? {
                    0 => Ok(()),
                    1 => continue,
                    _ => {
                        bail!("unexpected choice")
                    }
                };
            }

            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!(
                            "checkout proposal branch ({} ahead {} behind '{main_branch_name}')",
                            most_recent_proposal_patch_chain.len(),
                            proposal_behind_main.len(),
                        ),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    check_clean(&git_repo)?;
                    git_repo.checkout(&cover_letter.branch_name)?;
                    println!(
                        "checked out proposal as '{}' branch",
                        cover_letter.branch_name
                    );
                    Ok(())
                }
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        let (local_ahead_of_main, local_beind_main) =
            git_repo.get_commits_ahead_behind(&master_tip, &local_branch_tip)?;

        // new appendments to proposal
        if let Some(index) = most_recent_proposal_patch_chain.iter().position(|patch| {
            get_commit_id_from_patch(patch)
                .unwrap_or_default()
                .eq(&local_branch_tip.to_string())
        }) {
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!("checkout proposal branch and apply {} appendments", &index,),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    check_clean(&git_repo)?;
                    git_repo.checkout(&cover_letter.branch_name)?;
                    let _ = git_repo
                        .apply_patch_chain(
                            &cover_letter.branch_name,
                            most_recent_proposal_patch_chain,
                        )
                        .context("cannot apply patch chain")?;
                    println!(
                        "checked out proposal branch and applied {} appendments ({} ahead {} behind '{main_branch_name}')",
                        &index,
                        local_ahead_of_main.len().add(&index),
                        local_beind_main.len(),
                    );
                    Ok(())
                }
                1 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
                2 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
                3 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        // new proposal revision / rebase
        // tip of local in proposal history (new, amended or rebased version but no
        // local changes)
        if commits_events.iter().any(|patch| {
            get_commit_id_from_patch(patch)
                .unwrap_or_default()
                .eq(&local_branch_tip.to_string())
        }) {
            println!(
                "updated proposal available ({} ahead {} behind '{main_branch_name}'). existing version is {} ahead {} behind '{main_branch_name}'",
                most_recent_proposal_patch_chain.len(),
                proposal_behind_main.len(),
                local_ahead_of_main.len(),
                local_beind_main.len(),
            );
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!("checkout and overwrite existing proposal branch"),
                        format!("checkout existing outdated proposal branch"),
                        format!("apply to current branch with `git am`"),
                        format!("download to ./patches"),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    check_clean(&git_repo)?;
                    git_repo.create_branch_at_commit(
                        &cover_letter.branch_name,
                        &proposal_base_commit.to_string(),
                    )?;
                    git_repo.checkout(&cover_letter.branch_name)?;
                    let chain_length = most_recent_proposal_patch_chain.len();
                    let _ = git_repo
                        .apply_patch_chain(
                            &cover_letter.branch_name,
                            most_recent_proposal_patch_chain,
                        )
                        .context("cannot apply patch chain")?;
                    println!(
                        "checked out new version of proposal ({} ahead {} behind '{main_branch_name}'), replacing old version ({} ahead {} behind '{main_branch_name}')",
                        chain_length,
                        proposal_behind_main.len(),
                        local_ahead_of_main.len(),
                        local_beind_main.len(),
                    );
                    Ok(())
                }
                1 => {
                    check_clean(&git_repo)?;
                    git_repo.checkout(&cover_letter.branch_name)?;
                    println!(
                        "checked out old proposal in existing branch ({} ahead {} behind '{main_branch_name}')",
                        local_ahead_of_main.len(),
                        local_beind_main.len(),
                    );
                    Ok(())
                }
                2 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
                3 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
                4 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }
        // tip of proposal in branch in history (local appendments made to up-to-date
        // proposal)
        else if git_repo.ancestor_of(&local_branch_tip, &proposal_tip)? {
            let (local_ahead_of_proposal, _) = git_repo
                .get_commits_ahead_behind(&proposal_tip, &local_branch_tip)
                .context("cannot get commits ahead behind for propsal_top and local_branch_tip")?;

            println!(
                "local proposal branch exists with {} unpublished commits on top of the most up-to-date version of the proposal ({} ahead {} behind '{main_branch_name}')",
                local_ahead_of_proposal.len(),
                local_ahead_of_main.len(),
                proposal_behind_main.len(),
            );
            return match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        format!(
                            "checkout proposal branch with {} unpublished commits",
                            local_ahead_of_proposal.len(),
                        ),
                        "back".to_string(),
                    ]),
            )? {
                0 => {
                    git_repo.checkout(&cover_letter.branch_name)?;
                    println!(
                        "checked out proposal branch with {} unpublished commits ({} ahead {} behind '{main_branch_name}')",
                        local_ahead_of_proposal.len(),
                        local_ahead_of_main.len(),
                        proposal_behind_main.len(),
                    );
                    Ok(())
                }
                1 => continue,
                _ => {
                    bail!("unexpected choice")
                }
            };
        }

        println!("you have an amended/rebase version the proposal that is unpublished");
        // user probably has a unpublished amended or rebase version of the latest
        // proposal version
        // if tip of proposal commits exist (were once part of branch but have been
        // amended and git clean up job hasn't removed them)
        if git_repo.does_commit_exist(&proposal_tip.to_string())? {
            println!(
                "you have previously applied the latest version of the proposal ({} ahead {} behind '{main_branch_name}') but your local proposal branch has amended or rebased it ({} ahead {} behind '{main_branch_name}')",
                most_recent_proposal_patch_chain.len(),
                proposal_behind_main.len(),
                local_ahead_of_main.len(),
                local_beind_main.len(),
            );
        }
        // user probably has a unpublished amended or rebase version of an older
        // proposal version
        else {
            println!(
                "your local proposal branch ({} ahead {} behind '{main_branch_name}') has conflicting changes with the latest published proposal ({} ahead {} behind '{main_branch_name}')",
                local_ahead_of_main.len(),
                local_beind_main.len(),
                most_recent_proposal_patch_chain.len(),
                proposal_behind_main.len(),
            );

            println!(
                "its likely that you have rebased / amended an old proposal version because git has no record of the latest proposal commit."
            );
            println!(
                "it is possible that you have been working off the latest version and git has delete this commit as part of a clean up"
            );
        }
        println!("to view the latest proposal but retain your changes:");
        println!("  1) create a new branch off the tip commit of this one to store your changes");
        println!("  2) run `ngit list` and checkout the latest published version of this proposal");

        println!("if you are confident in your changes consider running `ngit push --force`");

        return match Interactor::default().choice(
            PromptChoiceParms::default()
                .with_default(0)
                .with_choices(vec![
                    format!("checkout local branch with unpublished changes"),
                    format!("discard unpublished changes and checkout new revision",),
                    format!("apply to current branch with `git am`"),
                    format!("download to ./patches"),
                    "back".to_string(),
                ]),
        )? {
            0 => {
                check_clean(&git_repo)?;
                git_repo.checkout(&cover_letter.branch_name)?;
                println!(
                    "checked out old proposal in existing branch ({} ahead {} behind '{main_branch_name}')",
                    local_ahead_of_main.len(),
                    local_beind_main.len(),
                );
                Ok(())
            }
            1 => {
                check_clean(&git_repo)?;
                git_repo.create_branch_at_commit(
                    &cover_letter.branch_name,
                    &proposal_base_commit.to_string(),
                )?;
                let chain_length = most_recent_proposal_patch_chain.len();
                let _ = git_repo
                    .apply_patch_chain(&cover_letter.branch_name, most_recent_proposal_patch_chain)
                    .context("cannot apply patch chain")?;

                git_repo.checkout(&cover_letter.branch_name)?;
                println!(
                    "checked out latest version of proposal ({} ahead {} behind '{main_branch_name}'), replacing unpublished version ({} ahead {} behind '{main_branch_name}')",
                    chain_length,
                    proposal_behind_main.len(),
                    local_ahead_of_main.len(),
                    local_beind_main.len(),
                );
                Ok(())
            }
            2 => launch_git_am_with_patches(most_recent_proposal_patch_chain),
            3 => save_patches_to_dir(most_recent_proposal_patch_chain, &git_repo),
            4 => continue,
            _ => {
                bail!("unexpected choice")
            }
        };
    }
}

fn launch_git_am_with_patches(mut patches: Vec<nostr::Event>) -> Result<()> {
    println!("applying to current branch with `git am`");
    // TODO: add PATCH x/n to appended patches
    patches.reverse();

    let mut am = std::process::Command::new("git")
        .arg("am")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to spawn git am")?;

    let stdin = am
        .stdin
        .as_mut()
        .context("git am process failed to take stdin")?;

    for patch in patches {
        stdin
            .write(format!("{}\n\n", patch.content).as_bytes())
            .context("failed to write patch content into git am stdin buffer")?;
    }
    stdin.flush()?;
    let output = am
        .wait_with_output()
        .context("failed to read git am stdout")?;
    print!("{:?}", output.stdout);
    Ok(())
}

fn event_id_extra_shorthand(event: &nostr::Event) -> String {
    event.id.to_string()[..5].to_string()
}

fn save_patches_to_dir(mut patches: Vec<nostr::Event>, git_repo: &Repo) -> Result<()> {
    // TODO: add PATCH x/n to appended patches
    patches.reverse();
    let path = git_repo.get_path()?.join("patches");
    std::fs::create_dir_all(&path)?;
    let id = event_id_extra_shorthand(
        patches
            .first()
            .context("there must be at least one patch to save")?,
    );
    for (i, patch) in patches.iter().enumerate() {
        let path = path.join(format!(
            "{}-{:0>4}-{}.patch",
            &id,
            i.add(&1),
            commit_msg_from_patch_oneliner(patch)?
        ));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .context("open new patch file with write and truncate options")?;
        file.write_all(patch.content().as_bytes())?;
        file.write_all("\n\n".as_bytes())?;
        file.flush()?;
    }
    println!("created {} patch files in ./patches/{id}-*", patches.len());
    Ok(())
}

fn check_clean(git_repo: &Repo) -> Result<()> {
    if git_repo.has_outstanding_changes()? {
        bail!(
            "cannot pull proposal branch when repository is not clean. discard or stash (un)staged changes and try again."
        );
    }
    Ok(())
}

pub fn tag_value(event: &nostr::Event, tag_name: &str) -> Result<String> {
    Ok(event
        .tags
        .iter()
        .find(|t| t.as_vec()[0].eq(tag_name))
        .context(format!("tag '{tag_name}'not present"))?
        .as_vec()[1]
        .clone())
}

pub fn get_commit_id_from_patch(event: &nostr::Event) -> Result<String> {
    let value = tag_value(event, "commit");

    if value.is_ok() {
        value
    } else if event.content.starts_with("From ") && event.content.len().gt(&45) {
        Ok(event.content[5..45].to_string())
    } else {
        bail!("event is not a patch")
    }
}

fn get_event_parent_id(event: &nostr::Event) -> Result<String> {
    Ok(if let Some(reply_tag) = event
        .tags
        .iter()
        .find(|t| t.as_vec().len().gt(&3) && t.as_vec()[3].eq("reply"))
    {
        reply_tag
    } else {
        event
            .tags
            .iter()
            .find(|t| t.as_vec().len().gt(&3) && t.as_vec()[3].eq("root"))
            .context("no reply or root e tag present".to_string())?
    }
    .as_vec()[1]
        .clone())
}

pub fn get_most_recent_patch_with_ancestors(
    mut patches: Vec<nostr::Event>,
) -> Result<Vec<nostr::Event>> {
    patches.sort_by_key(|e| e.created_at);

    let youngest_patch = patches.last().context("no patches found")?;

    let patches_with_youngest_created_at: Vec<&nostr::Event> = patches
        .iter()
        .filter(|p| p.created_at.eq(&youngest_patch.created_at))
        .collect();

    let mut res = vec![];

    let mut event_id_to_search = patches_with_youngest_created_at
        .clone()
        .iter()
        .find(|p| {
            !patches_with_youngest_created_at.iter().any(|p2| {
                if let Ok(reply_to) = get_event_parent_id(p2) {
                    reply_to.eq(&p.id.to_string())
                } else {
                    false
                }
            })
        })
        .context("cannot find patches_with_youngest_created_at")?
        .id
        .to_string();

    while let Some(event) = patches
        .iter()
        .find(|e| e.id.to_string().eq(&event_id_to_search))
    {
        res.push(event.clone());
        if event_is_patch_set_root(event) {
            break;
        }
        event_id_to_search = get_event_parent_id(event).unwrap_or_default();
    }
    Ok(res)
}

pub async fn find_proposal_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_ref: &RepoRef,
    root_commit: &str,
) -> Result<Vec<nostr::Event>> {
    let mut proposals = client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .custom_tag(
                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::T),
                        vec!["root"],
                    )
                    .custom_tag(
                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::A),
                        repo_ref
                            .maintainers
                            .iter()
                            .map(|m| format!("{REPO_REF_KIND}:{m}:{}", repo_ref.identifier)),
                    ),
                // also pick up proposals from the same repo but no target at our maintainers repo
                // events
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .custom_tag(
                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::T),
                        vec!["root"],
                    )
                    .reference(root_commit),
            ],
        )
        .await
        .context("cannot get proposal events")?
        .iter()
        .filter(|e| {
            event_is_patch_set_root(e)
                && (e
                    .tags
                    .iter()
                    .any(|t| t.as_vec().len() > 1 && t.as_vec()[1].eq(root_commit))
                    || e.tags.iter().any(|t| {
                        t.as_vec().len() > 1
                            && repo_ref
                                .maintainers
                                .iter()
                                .map(|m| format!("{REPO_REF_KIND}:{m}:{}", repo_ref.identifier))
                                .any(|d| t.as_vec()[1].eq(&d))
                    }))
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect::<Vec<nostr::Event>>();
    proposals.sort_by_key(|e| e.created_at);
    proposals.reverse();
    Ok(proposals)
}

pub async fn find_commits_for_proposal_root_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    proposal_root_events: &Vec<&nostr::Event>,
    repo_ref: &RepoRef,
) -> Result<Vec<nostr::Event>> {
    let mut patch_events: Vec<nostr::Event> = client
        .get_events(
            repo_ref.relays.clone(),
            vec![
                nostr::Filter::default()
                    .kind(nostr::Kind::Custom(PATCH_KIND))
                    .events(
                        proposal_root_events
                            .iter()
                            .map(|e| e.id)
                            .collect::<Vec<nostr::EventId>>(),
                    ),
            ],
        )
        .await
        .context("cannot fetch patch events")?
        .iter()
        .filter(|e| {
            e.kind.as_u64() == PATCH_KIND
                && e.tags.iter().any(|t| {
                    t.as_vec().len() > 2
                        && proposal_root_events
                            .iter()
                            .any(|e| t.as_vec()[1].eq(&e.id.to_string()))
                })
        })
        .map(std::borrow::ToOwned::to_owned)
        .collect();

    for e in proposal_root_events {
        if !event_is_cover_letter(e) && !patch_events.iter().any(|e2| e2.id.eq(&e.id)) {
            patch_events.push(e.to_owned().clone());
        }
    }
    Ok(patch_events)
}
