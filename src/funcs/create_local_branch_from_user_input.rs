use std::path::PathBuf;

use dialoguer::{Input, theme::ColorfulTheme};
use git2::{Repository, Oid};

use crate::repo_config::RepoConfig;

use super::checkout_branch::checkout_branch;


/// prompts user for name of local branch, creates the branch (checking if it is valid, if not looping) and returns it.
pub fn create_local_branch_from_user_input(
    repo_dir_path: &PathBuf,
    git_repo: &Repository,
    suggestion:&Option<String>,
    commit_id:&String,
    branch_id: &String,
) -> String {

    let target_commit = git_repo.find_commit(
        Oid::from_str(
            commit_id
        )
            .expect("commit_id supplied to be a valid Oid")
    )
        .expect("commit_id supplied exists in git repository");

    loop {
        let response: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("local branch name")
            .with_initial_text(match &suggestion {
                None => "".to_string(),
                Some(s) => s.to_string(),
            })
            .report(true)
            .interact_text()
            .unwrap();
        match git_repo.branch(
            &response,
            &target_commit,
            false,
        ) {
            Ok(branch) => {
                // check out branch
                checkout_branch(git_repo, branch);
                // set mapping
                let mut repo_config = RepoConfig::open(repo_dir_path);
                repo_config.set_mapping(&response, branch_id);
                break response;
            },
            Err(_) => {
                println!("not a valid nevent, note or hex string. try again. (or a local branch with that name exists)");
                continue
            },
        }
    }
}
