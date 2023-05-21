use std::{path::PathBuf, str::FromStr};

use git2::Repository;
use nostr::{Event, Filter, EventId};
use nostr_sdk::blocking::Client;

use crate::{ngit_tag::{tag_is_patch_parent, tag_is_initial_commit, tag_extract_value, tag_is_patch, tag_is_branch, tag_is_commit_parent, tag_is_commit}, utils::{load_event}, funcs::find_latest_patch::find_latest_patch, patch::{patch_commit_id, patch_is_commit}, branch_refs::BranchRefs, repo_config::RepoConfig, kind::Kind};


/// ancessor patch events first
pub fn get_updates_of_patches (
    client: &Client,
    branch_refs: &mut BranchRefs,
    git_repo: &Repository,
    repo_dir_path: &PathBuf,
    branch_id: &String,
    branch_name: &Option<String>,
    pull_new_branch: bool,
) -> Vec<Event> {

    let repo_config = RepoConfig::open(repo_dir_path);
    let last_patch_timestamp = repo_config.last_patch_update_time(branch_id.clone());
    
    // create direct patches filter
    let direct_patches_filter = Filter::new()
        .event(
            EventId::from_str(branch_id)
                .expect("branch_id to render as EventId")
        )
        .kinds(vec![Kind::Patch.into_sdk_custom_kind()]);
    
    let mut filters = vec![
        match &last_patch_timestamp {
            None => direct_patches_filter,
            Some(timestamp) => {
                direct_patches_filter.since(timestamp.clone())
            }
        }
    ];

    // get maintainers group
    if branch_refs.maintainers_group(Some(&branch_id)).is_none() {
        // fetch branch mantainers group and check again
        client.add_relays(
            branch_refs.branch_as_repo(Some(branch_id))
                .relays
                .clone().iter().map(|url| (url, None)).collect()
        )
            .expect("branch relays to be added to client");
        let mut group_events = client.get_events_of(
            vec![
                // use the opportunity to get all the remaining groups
                Filter::new().ids(branch_refs.group_ids_for_branches_without_cached_groups()),
            ],
            None,
        )
            .expect("get_events_of to not return an error");
        group_events.sort();
        group_events.dedup();
        branch_refs.updates(group_events);
    }

    // create indirect pacthes filter
    let merges_into_branch: Vec<Event> = branch_refs.merges.iter().filter(|event|
            // merged into branch
            event.tags.iter().any(|t|
                tag_is_branch(t)
                && tag_extract_value(t) == branch_id.clone()
            )
            // merge timestamp is after last_patch_timestamp - we already have patches before this date
            && match &last_patch_timestamp {
                None => true,
                Some(timestamp) => timestamp < &event.created_at
            }
            // author is member of branch maintainers group
            && branch_refs.is_authorized(Some(branch_id), &event.pubkey)
                .expect("found group event for branch after checking on speficied relays")
    ).map(|e|e.clone())
    .collect();

    if !merges_into_branch.is_empty() {
        filters.push(
            // ids for all patches referenced in merges
            Filter::new()
                .ids(
                    merges_into_branch.iter().flat_map(|event|
                        event.tags.iter()
                            .filter(|t| tag_is_patch(t))
                            .map(|t| tag_extract_value(t).clone())
                            .collect::<Vec<String>>()
                    )
                        .collect::<Vec<String>>()
                )
            //     .kinds(vec![Kind::Patch.into_sdk_custom_kind()])
        )
    }

    // find patch events
    let mut patch_events: Vec<Event> = client.get_events_of(
        filters,
        None,
    )
        .expect("get_events_of to not return an error when looking for patches");

    patch_events.sort();
    patch_events.dedup();

    // find patch tip on branch
    let latest_patch_on_branch = match find_latest_patch(
        &branch_id,
        &patch_events,
        &merges_into_branch,
        &branch_refs,
        &repo_dir_path,
    ) {
        // no patches return empty vector
        None => { return vec![] }, // for pull_new_branch do we set the branch to the latest commit referneced even if we have it?
        Some(event) => event,
    };

    let mut new_patches_on_branch = vec![];
    // for pull_new_branch - cycle through patch parents until we find any patch that exists in our commit history
    if pull_new_branch {
        let mut patch_event_id = latest_patch_on_branch.id.to_string();
        let mut patch_commit_id = tag_extract_value(
            latest_patch_on_branch.tags.iter().find(|t|tag_is_commit(t))
                .expect("all patch events to have a commit tag")
        );

        loop {
            let patch = match patch_events.iter().find(|p| p.id.to_string() == patch_event_id.clone()) {
                // patch event found in patch_events
                Some(patch) => patch,
                None => {
                    // loop for parent locally
                    if repo_dir_path.join(format!(
                        ".ngit/patches/{}.json",
                        patch_commit_id,
                    )).exists() {
                // break out of loop when we identify the commit where the branch begins
                        break
                    }
                    else {
                        panic!("cannot find parent patch locally or in patch_events.  This will fail if the branch does not share a commit with main / master")
                    }
                }
            };
            // add patch to list of patches to apply to new branch
            new_patches_on_branch.push(patch.clone());
            // prepare loop for next patch - set patch_event_id to current patches parent
            patch_event_id = tag_extract_value(
                patch.tags.iter().find(|t|tag_is_patch_parent(t))
                    .expect("patch to always have a patch parent.")
            );
            patch_commit_id = tag_extract_value(
                patch.tags.iter().find(|t|tag_is_commit_parent(t))
                    .expect("patch to always have a commit parent. This will fail if the branch does not share a commit with main / master")
            );
        };
    }

    // cycle through patch parents until we the latest commit in our local branch, or error if detects a rebase (it exists in our branch history)
    else {
        // revwalk through branch to identify forced push
        let mut revwalk = git_repo.revwalk()
            .expect("revwalk to not error on git_repo");
        match &branch_name {
            Some(name) => {
                revwalk.push(
                    git_repo.find_branch(
                        name.as_str(),
                        git2::BranchType::Local
                    )
                        .expect("branch found from the branch_name")
                        .get()
                        .peel_to_commit()
                        .expect("branch reference to peel back to a commit")
                        .id()
                )
                    .expect("revwalk push_glob(branch_name) not to error if branch name is not None");
            }
            None => (),
        }
        let commit_ids_in_branch: Vec<String> = if branch_name.is_none() { vec![] } else {
            revwalk.map(|oid|
            oid
                .expect("revwalk to produce oids without error")
                .to_string()
        ).collect()
        };

        let latest_commit: Option<&String> = match commit_ids_in_branch.get(0) {
            None => None,
            Some(latest_commit) => {
                // return empty if latest patch is in current chain
                if commit_ids_in_branch.iter().any(|id|
                    patch_commit_id(&latest_patch_on_branch) == id.to_string()
                ) { return vec![]; }
                Some(latest_commit)
            },
        };
        
        // work back thorugh commit chain until we reach a commit in our branch history (tip or ealier for rebase)
        new_patches_on_branch = vec![latest_patch_on_branch.clone()];
        loop {
            let next_parent_patch = new_patches_on_branch.last()
                .expect("chain to contain at least latest_patch_on_main")
                .clone();
            match next_parent_patch.tags.iter().find(|t|tag_is_patch_parent(t)) {
                None => { 
                    // found root patch or error
                    next_parent_patch.tags.iter().find(|t|tag_is_initial_commit(t))
                        // tag_is_initial_commit is false when it should be true. is it always false or just the oposite?
                        .expect(
                            &format!(
                                "reach a patch which doesn't contain a either a tag_is_patch_parent or tag_is_initial_commit{:#?}",
                                &next_parent_patch
                            )
                        );
                    break;
                },
                Some(t) => {
                    let next_patch = match patch_events.iter().find(|event|event.id.to_string() == tag_extract_value(t)) {
                        None => {
                            let patch_path = repo_dir_path.join(format!(
                                ".ngit/patches/{}.json",
                                tag_extract_value(
                                    next_parent_patch.tags.iter().find(|t|tag_is_commit_parent(t))
                                    .expect("patch to always have a commit parent if it has a patch parent")
                                ),
                            ));
                            if patch_path.exists() {
                                load_event(patch_path)
                                    .expect("patch json at location that exists loads into event")
                            }
                            else {
                                panic!("cannot find parent patch id {} from patch {:#?}",tag_extract_value(t), next_parent_patch);
                            }
                        },
                        Some(event) => event.clone(),
                    };
                    // if reached current tip - break
                    if latest_commit.is_some() && patch_is_commit(
                        &next_patch,
                        latest_commit.unwrap(),
                    ) { break; }
                    // detect rebase
                    if commit_ids_in_branch.iter().any(|id|
                        patch_commit_id(&next_patch) == id.to_string()
                    ) {
                        panic!("force push detected. This branch has been force pushed since you last pulled. ngit doesnt handle this yet");
                        }
                    // new patch
                    new_patches_on_branch.push(next_patch.clone());
                    
                },
            }
        }
    }
    // oldest first
    new_patches_on_branch.reverse();
    new_patches_on_branch
}
