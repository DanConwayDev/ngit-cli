use git2::{Branch, Repository};

pub fn checkout_branch(
    git_repo: &Repository,
    branch: Branch,
) {
    // checkout branch
    let (object, reference) = git_repo.revparse_ext(
        branch.name()
            .expect("valid name not to error")
            .expect("valid name name to exist")
    )
        .expect("object to be located from branch name");
    match git_repo.checkout_tree(&object, None) {
            Ok(_) => (),
            Err(err) => {
                panic!("You cannot checkout branch because {}", err.message());
            }
    }
    // set head to branch
    match reference {
        Some(gref) => git_repo.set_head(gref.name().unwrap()),
        None => git_repo.set_head_detached(object.id()),
    }
        .expect("succesfully set head");    
}

pub fn checkout_branch_from_name(
    git_repo: &Repository,
    branch_name: &String,
) {
        // checkout branch
        let (object, reference) = git_repo.revparse_ext(branch_name)
            .expect("object to be located from branch name");
        match git_repo.checkout_tree(&object, None) {
            Ok(_) => (),
            Err(err) => {
                panic!("You cannot checkout branch because {}", err.message());
            }
        }
        // set head to branch
        match reference {
            Some(gref) => git_repo.set_head(gref.name().unwrap()),
            None => git_repo.set_head_detached(object.id()),
        }
            .expect("succesfully set head");    
}