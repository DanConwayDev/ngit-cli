use git2::{Email, EmailCreateOptions};
use indicatif::ProgressBar;
use nostr::{Keys, Event};

use crate::{repos::repo::Repo, utils::{create_client, load_event, save_event}, ngit_tag::{tag_is_commit, tag_extract_value}, patch::initialize_patch, repo_config::RepoConfig};

pub fn create_and_broadcast_patches_from_oid(
    oids_ancestors_first: Vec<git2::Oid>,
    git_repo: &git2::Repository,
    repo_dir_path: &std::path::PathBuf,
    repo: &Repo,
    branch_id: &String,
    keys: &Keys,
) {
    let mut patches: Vec<Event> = vec![];
    for oid in oids_ancestors_first {
        patches.push(
            create_and_save_patch_from_oid(
                &oid,
                &patches,
                &git_repo,
                &repo_dir_path.join(".ngit"),
                &repo,
                &branch_id,
                &keys,
            )
        );
    }

            // update branch update timestamp
            match patches.last() {
                Some(p) => {
                    let mut repo_config = RepoConfig::open(&repo_dir_path);
                    repo_config.set_last_patch_update_time(
                        branch_id.clone(),
                        p.created_at.clone(),
                    );
                }
                None => (),
            };


    // broadcast patches
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(format!("Broadcasting... if this takes 20s+, there was a problem broadcasting to one or more relays even if it says 'Pushed {} patches!'.",patches.len()));

    let client = create_client(&keys, repo.relays.clone())
        .expect("create_client to return client for create_and_broadcast_patches");
    for e in &patches { 
        match client.send_event(e.clone()) {
            Ok(_) => (),
            // TODO: this isn't working - if a relay is specified with a type it will wait 30ish secs and then return successful
            Err(e) => { println!("error broadcasting patch event: {}",e); },
        }
        // TODO: better error handling here / reporting. potentially warn if taking a while and report on troublesome relays
    }
    spinner.finish_with_message(format!("Pushed {} commits!.",patches.len()));
}

pub fn create_and_save_patch_from_oid(
    oid: &git2::Oid,
    patches: &Vec<Event>,
    git_repo: &git2::Repository,
    ngit_path: &std::path::PathBuf,
    repo: &Repo,
    branch_id: &String,
    keys: &Keys,
) -> Event {
    let commit_id = format!("{}",oid);
    let commit = git_repo.find_commit(*oid)
        .expect("revwalk returns oid that matches a comit in the repository");
    let message = match commit.message() {
        None => "",
        Some(m) => m
    }.to_string();
    let email = Email::from_commit(
        &commit,
        &mut EmailCreateOptions::default(),
    ).expect("renders a commit as an email diff");
    let parent_commit_id: Option<String> = match &commit.parent_id(0) {
        Ok(parent_oid) => Some(format!("{}",parent_oid)),
        Err(_) => None,
    };
    let parent_patch_id = match &parent_commit_id {
        None => None,
        Some(id) => Some({
            // search for parent in current batch of patches
            match patches.iter().find(|p|
                p.tags.iter().find(|t|tag_is_commit(t) && id.clone() == tag_extract_value(&t)).is_some()
            ) {
                Some(p) => p.id.to_string(),
                None => {
                    let parent_patch_path = ngit_path.join(format!("patches/{}.json",id));
                    if parent_patch_path.exists() {
                        load_event(parent_patch_path)
                            .expect("patch in json file that exists produces valid event")
                            .id.to_string()
                    } else {
                        panic!("cannot find parent patch. ngit may have ordered ancestors without patches incorrectly");
                    }
                },
            }
        }),
    };
    let event = initialize_patch(
        &keys,
        &repo.id.to_string(),
        &branch_id,
        &email.as_slice(),
        &message,
        &vec![commit_id.to_string()],
        parent_patch_id,
        parent_commit_id,
    );
    // save patch 
    save_event(ngit_path.join(
        // TODO: consider what happens if a commit gets published twice, which one would get priority? The one from a maintiner?
        format!("patches/{}.json",commit_id),
    ), &event)
        .expect("save_event to store event in /patches");
    event
}
