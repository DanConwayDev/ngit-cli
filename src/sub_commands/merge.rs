use std::env::current_dir;

use clap::Args;




use crate::{ merge::initialize_merge, repo_config::RepoConfig, utils::{load_event, get_stored_keys, get_or_generate_keys, save_event, create_client}, config::load_config, repos::repo::Repo, fetch_pull_push::fetch_pull_push, funcs::checkout_branch::checkout_branch, branch_refs::BranchRefs};

#[derive(Args)]
pub struct MergeSubCommand {
    // TODO: add support merging intop branches
    // /// branch nevent or hex to pull into a new local branch
    // #[arg(short, long)]
    // branch_from: Option<String>,
    // /// branch nevent or hex to pull into a new local branch
    // #[arg(short, long)]
    // branch_to: Option<String>,
}

pub fn merge(_sub_command_args: &MergeSubCommand) {

    // TODO: add support for merging other branches
    // loop {
    //     let proposed_to= valid_event_id_from_input(
    //         sub_command_args.branch_from.clone(),
    //         &"branch to merge into (nevent note or hex)".to_string(),
    //     );
    //     // check that branch is in mapping
    // }

    let repo_dir_path = current_dir().unwrap();

    let git_repo = git2::Repository::open(&repo_dir_path)
        .expect("git repo not initialized. run ngit init first");

    let repo = Repo::open(&repo_dir_path);

    let mut cfg = load_config();

    let keys = match get_stored_keys(&mut cfg) {
        None => {
            get_or_generate_keys(&mut cfg)
        },
        Some(k) => k.clone(),
    };

    let repo_config = RepoConfig::open(&repo_dir_path);

    // check branch isn't main/master.
    let head = git_repo.head()
        .expect("head to be returned");
    if !head.is_branch() {
        return println!("checkout a branch to merge into main/master");
    }
    let branch_name = match head.name() {
        None => {
            return println!("checkout a branch to merge into main/master");
        },
        Some(head_name) => {
            let name = head_name.replace("refs/heads/","");
            if name == "main" || name == "master" {
                return println!("checkout a branch to merge into main/master");
            }
            name
        },
    };

    // check user is repo maintainer
    let branch_refs = BranchRefs::new(vec![], repo_dir_path.clone());
    if !match branch_refs.is_authorized(None, &keys.public_key()) {
        None => false,
        Some(auth) => auth,
    } {
        return println!("You are not a repository maintainer so you cannot merge into main/master.");
    }

    // get latest commit / check the patches are issued
    let commit_id = head.peel_to_commit()
        .expect("branch reference to peel back to a commit")
        .id()
        .to_string();


    let branch_tip_patch_path = repo_dir_path.join(
        format!(".ngit/patches/{}.json",&commit_id)
    );

    if !branch_tip_patch_path.exists() {
        return println!("your branch needs pushing before you can merge!");
    }

    // TODO: check we have the latest master from relays and that merging isnt a force push (ie.the merged branch must start at the latest commit in master)

    // create merge event referencing the commit id, patch, branch form (TODO other commits not in main, and Pull Requests)
    let merge_event = initialize_merge(
        &keys,
        &repo.id.to_string(),
        &repo.id.to_string(),
        &repo_config.branch_id_from_name(&branch_name.to_string())
            .expect("current branch, that has already been pushed, to be in mapping"),
        &commit_id,
        &load_event(&branch_tip_patch_path)
            .expect("patch event to load from json in path")
            .id.to_string(),
    );
    let merge_path = repo_dir_path.join(
        format!(".ngit/merges/{}.json",&merge_event.id.to_string())
    );
    // save event so fetch_pull_push picks it up when runing get_branch_refs
    save_event(&merge_path, &merge_event)
        .expect("merge event to save to path");

    // broadcast 
    let client = create_client(&keys, repo.relays.clone())
            .expect("create_client returns client");

    // TODO try and apply locally and abort if there are errors before broadcasting
    match client.send_event(merge_event.clone()) {
        Ok(_) => (),
        // TODO: this isn't working - if a relay is specified with a type it will wait 30ish secs and then return successful
        Err(e) => { println!("error broadcasting event: {}",e); },
    }
    // TODO: better error handling here / reporting. potentially warn if taking a while and report on troublesome relays

    // checkout master / main
    let master_branch = match git_repo.find_branch("master", git2::BranchType::Local) {
        Ok(branch) => branch,
        Err(_) => {
            git_repo.find_branch("main", git2::BranchType::Local)
                .expect("the main branch to be called main or master")
        },
    };
    checkout_branch(&git_repo, master_branch);
    // apply commits to master/main
    // TODO: there should be no need to reach out to the relays agin but using fetch_pull_push without modification is convinient
    fetch_pull_push(
        Some(&keys),
        true,
        false,
        None,
        false,
        None,
        Some(client),
    );


}
