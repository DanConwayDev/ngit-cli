use std::{path::PathBuf, process::Command, fs::{self, File}, io::Write};

use git2::Repository;
use nostr::Event;

use crate::{utils::save_event, ngit_tag::{tag_extract_value, tag_is_commit}};


pub fn apply_patches(
    git_repo: &Repository,
    repo_dir_path: &PathBuf,
    patches_correctly_ordered:&mut Vec<Event>,

) {
    // check git is installed
    match Command::new("git").output() {
        Ok(_o) => (),
        Err(_e) => {
            panic!("git isn't installed :( Install git and then you can use ngit :)");
        }
    }
    let ngit_path = repo_dir_path.join(".ngit");

    println!("{} commits to apply",patches_correctly_ordered.len());
    fs::create_dir(ngit_path.join("patches/mbox"))
        .expect("patches/mbox to be created by create_dir");
    for (i, event) in patches_correctly_ordered.iter().enumerate() {
        save_event(
            ngit_path.join(format!(
                "patches/{}.json",
                tag_extract_value(
                    &event.tags.iter().find(|t| tag_is_commit(t))
                        .expect("each patch contains commit tag")
                )
            )),
            &event,
        )
            .expect("patch to be saved with [commit_id].json using save_event");
        // extract mbox patch and save to file for 'git am' to recieve
        let patch_path = format!("patches/mbox/{:0>5}.patch",i);
        let mut f = File::create(ngit_path.join(&patch_path))
            .expect("patch file can be created at patch_path location");
        f.write_all(&event.content.as_bytes())
            .expect("can use write_all to write event content to patch file");
        // gitoxide or libgit2 do not support applying patches whilst maintaining the commit ids so we fall back to indirectly using git
        // it turns out that git am doesnt retain commit ids. for now we will modify the committer author and timestamp to correct the commit id.
        match Command::new("git")
        .current_dir(&repo_dir_path)
        .args([
            "am",
            ngit_path.join(&patch_path).to_string_lossy().to_string().as_str(),
        ])
        .output() {
            Ok(_o) => {
                let mut revwalk = git_repo.revwalk()
                    .expect("revwalk not to error");
                revwalk.push_head()
                    .expect("revwalk.push_head not to error");
                
                for (i, oid) in revwalk.enumerate() {
                    if i == 0 {
                        let old_commit = git_repo.find_commit(
                            oid
                                .expect("oid of newly added commit")
                        )
                            .expect("commit from newly added commit oid");
                        // create commit using amend with relects the original commit id (assumes committer should be identical to author
                        // TODO: in git push add a tag if the committer information is different to author. Then here use that info instead.
                        let updated_commit_oid = old_commit.amend(
                            None,
                            None,
                            Some(&old_commit.author()),
                            None,
                            None,
                            None,
                        )
                            .expect("ammend commit to produce new oid");
                        // replace the commit with the wrong oid with the newly created one with the correct oid
                        git_repo.head()
                            .expect("to return head of git_repo")
                            .set_target(
                                updated_commit_oid,
                                "ref commit with fix committer details",
                            )
                                .expect("branch to be updated with fixed commt");
                    }
                };
            },
            Err(_e) => { panic!(":( git error: {:#?}",_e); },
        }
    }
    // clear up by removing mbox directory
    fs::remove_dir_all(ngit_path.join("patches/mbox"))
        .expect("patches/mbox to be removed recursively now we are done with it");
}
