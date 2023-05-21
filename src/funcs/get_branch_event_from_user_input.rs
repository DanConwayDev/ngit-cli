use std::path::PathBuf;

use nostr::{Event};

use crate::{branch_refs::BranchRefs, repo_config::RepoConfig, cli_helpers::valid_event_id_from_input};

pub fn get_branch_event_from_user_input(
    branch_string_param:&Option<String>,
    branch_refs: &BranchRefs,
    repo_dir_path: &PathBuf,
) -> Event {
    get_branch_event_with_options(
        branch_string_param,
        branch_refs,
        repo_dir_path,
        true,
    )
}

pub fn get_unmapped_branch_event_from_user_input(
    branch_string_param:&Option<String>,
    branch_refs: &BranchRefs,
    repo_dir_path: &PathBuf,
) -> Event {
    get_branch_event_with_options(
        branch_string_param,
        branch_refs,
        repo_dir_path,
        false,
    )
}    

fn get_branch_event_with_options(
        branch_string_param:&Option<String>,
        branch_refs: &BranchRefs,
        repo_dir_path: &PathBuf,
        retrun_unmapped_branches: bool,
) -> Event {

    let mut string_param = branch_string_param.clone();
    loop {
        let valid_id = valid_event_id_from_input(
            string_param.clone(),
            &"nevent note or hex of remote branch to pull".to_string(),
        );

        match branch_refs.branches.iter().find(|g| g.id.eq(&valid_id)) {
            Some(branch_event) => {
                let repo_config = RepoConfig::open(repo_dir_path);
                if !retrun_unmapped_branches {
                    match repo_config.branch_name_from_id(&valid_id.to_string()) {
                        // branch is alreay mapped
                        Some(name) => {
                            println!(
                                "local branch '{}' is already linked to this nostr branch. having multiple local branches linked to a nostr branch isn't supported right now. feel free to create a feature request :)",
                                name,
                            );
                            string_param = None;
                            continue
                        }
                        None => (),
                    }
                }
                break branch_event.clone();
            },
            None => {
                println!("valid id but the branch cannot be found in this respository on the specified relays. try again.");
                string_param = None;
                continue
            }
        }
    }
}
