use std::path::PathBuf;

use git2::{Repository, Oid};

pub fn find_commits_ahead (
    git_repo: &Repository,
    repo_dir_path: &PathBuf,
    branch_name: &String,

) -> Vec<Oid> {
    // get revwalk of commits
    let mut revwalk = git_repo.revwalk()
        .expect("revwalk not to error");
    // using the specified branch
    match revwalk.push_glob(&branch_name) {
        Ok(_) => (),
        // errors when there are no commits
        Err(_) => { return vec![]; }
    }

    // find commit that need pushing
    let mut revwalk = git_repo.revwalk()
        .expect("git_repo.revwalk() to not error");
    revwalk.push_head()
        .expect("revwalk.push_head not to error. already checked for some commits. headless?");

    let mut new_commits = vec![];

    for oid in revwalk {
        // whatever branch we are on, we are only interested in returning how many unpublished commits we are ahead.
        if repo_dir_path.join(".ngit").join(format!(
            "patches/{}.json",
            oid.as_ref().expect("oid to refernce commits").clone(),
        )).exists()  { 
            break;
        }
        new_commits.push(oid.expect("oid to refernce commits"));
    }
    // most often used ancestor first
    new_commits.reverse();
    new_commits
}
