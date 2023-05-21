use std::path::PathBuf;

use nostr::{ Event };

use crate::{ngit_tag::{tag_is_branch, tag_extract_value, tag_is_commit}, branch_refs::BranchRefs, utils::load_event, kind::Kind};

/// finds latest patch that needs applying. It might not be the latest created_at date if an earlier patch was 'merged' more recently
pub fn find_latest_patch(
    branch_id: &String,
    patch_events:&Vec<Event>,
    merges_into_branch: &Vec<Event>,
    branch_refs:&BranchRefs,
    repo_dir_path: &PathBuf,
) -> Option<Event> {

    // ensure only patch events make it into patch_events - we cant rely on relays for this
    let patch_events:Vec<Event> = patch_events.iter().filter(|p|
        // kind is patch
        p.kind == nostr_sdk::Kind::Custom(u64::from(Kind::Patch))
    ).map(|p|p.clone()).collect();

    let directly_authorised_patches: Vec<Event> = patch_events.iter().filter(|p|
        // kind is patch
        p.kind == nostr_sdk::Kind::Custom(u64::from(Kind::Patch))
        // on branch
        && p.tags.iter().any(
            |t|tag_is_branch(t)
            && tag_extract_value(t) == branch_id.clone()
        )
        // authorized
        && match &branch_refs.is_authorized(Some(&branch_id), &p.pubkey) {
            None => { false },
            Some(is_authorized) => { is_authorized.clone() }
        }
    ).map(|p|p.clone()).collect();

    let latest_authorized_patch = find_latest_event(&directly_authorised_patches);

    let authorised_merges: Vec<Event> = merges_into_branch.iter().filter(|m|
        // kind is merge
        m.kind == nostr_sdk::Kind::Custom(u64::from(Kind::Merge))
        // into branch
        && m.tags.iter().any(
            |t|tag_is_branch(t)
            && tag_extract_value(t) == branch_id.clone()
        )
        // merge authorized
        && match &branch_refs.is_authorized(Some(&branch_id), &m.pubkey) {
            None => { false },
            Some(is_authorized) => { is_authorized.clone() }
        }
    ).map(|p|p.clone()).collect();

    let latest_authorised_merge = find_latest_event(&authorised_merges);

    // find latest patch

    match latest_authorised_merge {
        // no merge - return patch or None
        None => latest_authorized_patch,
        Some(m) => {
            match latest_authorized_patch {
                // merge but no patch, return the patch related to the merge
                None => {
                    Some(get_merge_patch(
                        &m,
                        &patch_events,
                        repo_dir_path,
                    ))
                },
                // a merge and a patch
                Some(p) => {
                    // return the patch if it is later than merge
                    if m.created_at < p.created_at {
                        Some(p.clone())
                    }
                    // return the patch related to the merge if the merge is later
                    else {
                        Some(get_merge_patch(
                            &m,
                            &patch_events,
                            repo_dir_path,
                        ))
                    }
                }
            }
        }
    }
}

fn find_latest_event(events:&Vec<Event>) -> Option<Event> {
    let mut latest = match events.get(0) {
        None => { return None },
        Some(e) => e.clone(),
    };
    for e in events.iter() {
        if e.created_at > latest.created_at {
            latest = e.clone();
        }
    }
    Some(latest)
}

fn get_merge_patch(
    merge: &Event,
    patch_events: &Vec<Event>,
    repo_dir_path: &PathBuf,
) -> Event{
    let commit_id = tag_extract_value(
        merge.tags.iter().find(|tag| tag_is_commit(tag))
        .expect("merge event will have a commit tag")
    );
    // search in patch_events vector
    match patch_events.iter().find(|p|
        tag_extract_value(
            p.tags.iter().find(|tag| tag_is_commit(tag))
                .expect("patch event will have a commit tag")
        ) == *commit_id
    ) {
        // found merge patch in patch_events
        Some(patch) => patch.clone(),
        None => {
            let patch_path = repo_dir_path.join(format!(
                ".ngit/patches/{}.json",
                commit_id
            ));
            if patch_path.exists() {
                // found merge patch in .ngit/patches
                load_event(patch_path)
                    .expect("patch at path that exists renders as event")
            }
            else {
                panic!("cannot find patch from merge event in event vector or .ngit folder");
            }
        },
    }
}
