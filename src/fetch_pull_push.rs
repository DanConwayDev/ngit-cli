use std::{env::current_dir, path::PathBuf, vec};

use dialoguer::{Confirm, theme::ColorfulTheme};
use git2::{BranchType};
use nostr::{Keys};
use nostr_sdk::blocking::Client;

use crate::{groups::groups::Groups, repos::repo::Repo, utils::{create_client, get_stored_keys, get_or_generate_keys}, config::load_config, repo_config::RepoConfig, funcs::{find_commits_ahead::find_commits_ahead, apply_patches::apply_patches, get_updates_of_patches::get_updates_of_patches, create_patches::create_and_broadcast_patches_from_oid, create_branch_and_pr::create_branch_and_pr, get_branch_event_from_user_input::{get_unmapped_branch_event_from_user_input,get_branch_event_from_user_input}, create_local_branch_from_user_input::create_local_branch_from_user_input, checkout_branch::{checkout_branch_from_name}}, branch_refs::{get_branch_refs, BranchRefs}, ngit_tag::{tag_is_commit_parent, tag_extract_value}};

/// will only pull if no rebase required or push if no downstream conflicts detected
pub fn fetch_pull_push(
    keys: Option<&Keys>,
    pull: bool,
    push: bool,
    proposed_branch_to_pull: Option<String>,
    clone: bool,
    repo_dir_path: Option<PathBuf>,
    client: Option<Client>,
) -> BranchRefs {

    let repo_dir_path = match repo_dir_path {
        None => current_dir().unwrap(),
        Some(p) => p,
    };

    let git_repo = git2::Repository::open(&repo_dir_path)
        .expect("git repo not initialized. run ngit init first");

    if !repo_dir_path.join(".ngit").is_dir() {
        panic!("ngit not initialised. Run 'ngit init' first...");
    }

    let repo_has_no_commits = git_repo.branches(Some(BranchType::Local))
        .expect("git_repo.branches to not error even if its a blank repo")
        .count() == 0;

    // check whether we already have a local branch mapped to proposed_branch_to_pull
    let proposed_branch_to_pull = match proposed_branch_to_pull {
        // it was never specified
        None => None,
        Some(id) => {
            let branch_id = get_branch_event_from_user_input(
                &Some(id),
                &BranchRefs::new(
                    vec![],
                    repo_dir_path.clone(),
                ),
                &repo_dir_path,
            ).id.to_string();
            match RepoConfig::open(&repo_dir_path).branch_name_from_id(&branch_id) {
                // we dont have a mapped local branch
                None => Some(branch_id),
                // we have a branch mapping. check it out and do a normal pull
                Some(name) => {
                    checkout_branch_from_name(
                        &git_repo,
                        name,
                    );
                    None
                }
            }
        }
    };

    let branch_name = if clone || repo_has_no_commits || proposed_branch_to_pull.is_some() {
        None
    } else {
        // no commits
        if git_repo.branches(Some(BranchType::Local))
            .expect("git_repo.branches to not error even if its a blank repo")
            .count() == 0 {
            panic!("There are no branches. you should use clone instead")
        }
        let head = git_repo.head()
            .expect("git_repo returns head");
        if !head.is_branch() {
            // TODO: fetch should still work here?
            panic!("check out a branch to continue. you have an another object checked out such as a tag or commit.");
        }
        Some(
            head.shorthand()
                .expect("head is branch so head.shortand() should retunr branch name")
                .to_string()
        )
    };

    let repo = Repo::open(&repo_dir_path);

    let new_commits_to_push =
        if clone || repo_has_no_commits || proposed_branch_to_pull.is_some() { vec![] }
        else {
            find_commits_ahead(
                &git_repo,
                &repo_dir_path,
                &branch_name.clone()
                    .expect("branch_name to always be defined (clone and pull_new_branch do not reach here)"),
            )
        }
    ;

    let mut cfg = load_config();

    let keys = match keys {
        None => {
            match get_stored_keys(&mut cfg) {
                None => {
                    if push { get_or_generate_keys(&mut cfg) }
                    else {Keys::generate() }
                }
                Some(k) => k.clone(),
            }
        },
        Some(k) => k.clone(),
    };

    let client = match client {
        None => create_client(&keys, repo.relays.clone())
            .expect("create_client returns client"),
        Some(client) => client,
    };

    let mut branch_refs = get_branch_refs(&repo, &client, &repo_dir_path);

    let branch_id:String = 
        if clone || repo_has_no_commits { repo.id.to_string() }
        else if proposed_branch_to_pull.is_some() {
            get_unmapped_branch_event_from_user_input(
                &proposed_branch_to_pull,
                &branch_refs,
                &repo_dir_path,
            ).id.to_string()
        }
        else {
            let name = &branch_name.clone()
                .expect("branch_name to always be defined (clone and pull_new_branch do not reach here)");

            match RepoConfig::open(&repo_dir_path)
                .branch_id_from_name(
                    &name,
                ) {
                    None => {
                        if push {
                            create_branch_and_pr(
                                &name,
                                new_commits_to_push.len(),
                                &repo_dir_path,
                                &repo,
                                &mut branch_refs,
                                &keys,
                                &mut cfg,
                                &client,
                            )
                        } else {
                            println!("branch '{}' hasn't been pushed!",&name);
                            return branch_refs;
                        }
                    },
                    Some(id) => id.clone(),
            }
        }
    ;

    let mut patches = get_updates_of_patches(
        &client,
        &mut branch_refs,
        &git_repo,
        &repo_dir_path,
        &branch_id,
        &branch_name,
        proposed_branch_to_pull.is_some(),
    );

    let mut confirmed_branch_name = 
        if clone || repo_has_no_commits { "master/main".to_string() }
        else if proposed_branch_to_pull.is_some() {
            match patches.get(0) {
                None => {
                    match RepoConfig::open(&repo_dir_path)
                    .branch_name_from_id(
                        &branch_id,
                    ) {
                        None => {
                            // TODO you should still be able to check it out -  find the easliest commit, check it out as branch, find the latest commit and set the branch to that commit.
                            println!("No new commits were found. Not pulling the branch.");
                        },
                        Some(name) => {
                            println!("exists as a local branch named '{}'. check it out and then fetch /pull.",name);
                        },
                    }
                    return branch_refs;
                }
                Some(earliest_patch_event) => {
                    // create local branch from off of parent commit
                    create_local_branch_from_user_input(
                        &repo_dir_path,
                        &git_repo,
                        &branch_refs.branch_as_repo(Some(&branch_id)).name,
                        &tag_extract_value(
                            earliest_patch_event.tags.iter()
                            .find(|t|tag_is_commit_parent(t))
                            .expect("patch event to have parent commit"),
                        ),
                        &branch_id
                    )
                }
            }
        }
        else {
            branch_name.clone()
            .expect("branch_name to exists. clone and pull_new_branch don't reach here")
        }
    ;

    // no patches or new commits
    if patches.is_empty()
        && new_commits_to_push.is_empty()
    {
        println!(
            "branch '{}' is up-to-date{}",
            &confirmed_branch_name,
            if push { ". no changes to push." }
            else { "!"}
        );
    }
    // patches with no new commits
    else if new_commits_to_push.is_empty()
    {
        println!(
            "branch '{}' {} behind{}",
            &confirmed_branch_name,
            &patches.len(),
            if push { ". no changes to push." }
            else { "!"}
        );
        if pull || proposed_branch_to_pull.is_some() {
            // apply patches
            apply_patches(
                &git_repo,
                &repo_dir_path,
                &mut patches,
            );

            // update repo_config
            let mut repo_config = RepoConfig::open(&repo_dir_path);
            // update branch mapping
            if clone || repo_has_no_commits {
                confirmed_branch_name = git_repo.head()
                    .expect("we have just cloned and therefore commited to main branch so git_repo.head should not error")
                    .shorthand()
                    .expect("shorthand to be moast / main")
                    .to_string();
                repo_config.set_mapping(&confirmed_branch_name, &repo.id.to_string());
            }
            // update branch update timestamp
            match patches.last() {
                Some(p) => {
                    repo_config.set_last_patch_update_time(
                        branch_id.clone(),
                        p.created_at.clone(),
                    );
                }
                None => (),
            };

            println!(
                "branch '{}' is up-to-date!",
                &confirmed_branch_name
            );
        }
    }
    else {
        let update = format!(
            "branch '{}' {} behind and {} ahead",
            &confirmed_branch_name,
            &patches.len(),
            &new_commits_to_push.len(),
        );
        // new commits and new patches
        if !patches.is_empty() {
            if pull { println!("{update}. TODO enable rebase option... pull to branch?"); }
            else if push { println!("{update} TODO enable for push option. TODO enable rebase option... pull to branch?"); }
            else { println!("{update}"); }
            // there have been 3 more commits on the main branch. would you like to rebase before pushing your new branch?
        // there has been 1 commit(s) the branch you are pushing 'feat:add-stuff'. how would you like to proceed?
        // [ ] rebase my commits
        // [ ] ignore commit(s) 

        }
        // new commits with no patches
        else {
            println!("{update}");
            if push {
                if Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "push {} commits on the '{}' branch?",
                    &new_commits_to_push.len(),
                    &confirmed_branch_name
                ))
                .default(true)
                .interact()
                .unwrap()
                {
                    // get keys
                    let mut cfg = load_config();
                    let keys = get_or_generate_keys(&mut cfg);
            
                    // check permission
                    let groups = Groups::new();
                    let maintainers = groups.by_event_id(
                        repo.maintainers_group.get_first_active_group()
                            .expect("maintainers_group will never be null")
                    )
                        .expect("always will have the maintainers_group initialisaiton event cached")
                        .members();
                    if maintainers.iter().any(|k| keys.public_key() == **k) {
                        println!(
                            "you are a repo maintainer and have the permission to push to '{}'!",
                            &confirmed_branch_name,
                        )
                    } if match branch_refs.is_authorized(Some(&branch_id), &keys.public_key()) {
                        None => false,
                        Some(authorized) => authorized,
                    } {
                        println!(
                            "you have the permission to push to '{}'!",
                            &confirmed_branch_name,
                        )
                    } 
                    else {                        
                        panic!(
                            "You are not a repo maintainer so you  don't have permission to push to '{}' branch :(",
                            &confirmed_branch_name,
                        );
                    }
                    create_and_broadcast_patches_from_oid(
                        new_commits_to_push,
                        &git_repo,
                        &repo_dir_path,
                        &repo,
                        &branch_id,
                        &keys,
                    );
                }
            }
        }
    }

    // there have been 3 more commits on the main branch. would you like to rebase before pushing your new branch?
    // there has been 1 commit(s) the branch you are pushing 'feat:add-stuff'. how would you like to proceed?
    // [ ] rebase my commits
    // [ ] ignore commit(s) 


        // let ngit_path = repo_dir_path.join(".ngit");
    // // CURRENTLY UNUSED identify new merges 
    // let new_merge_ids: Vec<&String> = branch_refs.merged_branches_ids
    //     .iter()
    //     .filter(|id|
    //         ngit_path.join(format!("merges/{}.json",id)).exists()
    //     )
    //     .collect();
    // // TODO: identify new PullRequests to report
    
    // Non closed PRs and branches
    // TODO add a status-update custom tag for so PRs can be marked as closed or reopened.
        // then we can gather status updates and filter out closed branches and build open one.
        // merge - commit, from-branch, to-branch
    
    // find patches
    // get latest chain of patches on main
    
    // identify merged branches
        // will there always be a pull request for a branch?

    // get patches from maitainers or branches merged by maintainers and permission groups for these branches

    branch_refs
}
