use nostr::{Event, EventBuilder, Keys };
use std::str;

use crate::{ngit_tag::{tag_repo, tag_branch, tag_commit_parent, tag_commit, tag_initial_commit, tag_patch_parent, tag_is_commit, tag_extract_value, tag_commit_message, tag_hashtag, tag_into_event}, kind::Kind};

pub fn initialize_patch(
    keys: &Keys,
    repoistory:&String,
    branch: &String,
    patch:&[u8],
    message: &String,
    commit_ids: &Vec<String>,
    patch_parent_id:Option<String>,
    parent_commit_id:Option<String>,
) -> Event {
    let mut tags = vec![
        tag_repo(repoistory),
        tag_into_event(tag_repo(repoistory)),
        tag_branch(branch),
        tag_into_event(tag_branch(branch)),
        tag_commit_message(message),
        tag_hashtag("ngit-event"),
        tag_hashtag("ngit-format-0.0.1"),
];
    for id in commit_ids {
        tags.push(tag_commit(id));
    }
    match parent_commit_id {
        None => { tags.push(tag_initial_commit()); },
        Some(id) => { tags.push(tag_commit_parent(&id)); }
    };
    match patch_parent_id {
        None => (),
        Some(id) => {
            tags.push(tag_patch_parent(&id));
            tags.push(tag_into_event(tag_patch_parent(&id)));
        }
    };
    let content = str::from_utf8(patch)
        .expect("patch Vec<u8> to convert to string");
    EventBuilder::new(
        Kind::Patch.into_sdk_custom_kind(),
        content,
        &tags,
    )
    .to_unsigned_event(keys.public_key())
    .sign(&keys)
    .unwrap()
}

pub fn patch_is_commit(event:&Event, oid:&String) -> bool {
    event.tags.iter().any(
        |t|tag_is_commit(t)
        && tag_extract_value(t) == oid.clone()
    )
}

pub fn patch_commit_id(event:&Event) -> String {
    match event.tags.iter().find(
        |t|tag_is_commit(t)
    ) {
        None => { String::new() },
        Some(t) => { tag_extract_value(t) },
    }
}