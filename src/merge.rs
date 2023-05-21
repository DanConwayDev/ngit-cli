use nostr::{Event, EventBuilder, Keys };


use crate::{ngit_tag::{tag_repo, tag_branch, tag_commit, tag_branch_merge_from, tag_patch, tag_hashtag, tag_into_event}, kind::Kind};

pub fn initialize_merge(
    keys: &Keys,
    repoistory:&String,
    to_branch_id: &String,
    from_branch_id: &String,
    commit_id: &String,
    patch_id: &String,
) -> Event {
    let tags = vec![
        tag_repo(repoistory),
        tag_into_event(tag_repo(repoistory)),
        tag_branch(to_branch_id),
        tag_into_event(tag_branch(to_branch_id)),
        tag_branch_merge_from(from_branch_id),
        tag_into_event(tag_branch_merge_from(from_branch_id)),
        tag_patch(patch_id),
        tag_into_event(tag_patch(patch_id)),
        tag_commit(commit_id),
        tag_hashtag("ngit-event"),
        tag_hashtag("ngit-format-0.0.1"),
];
    EventBuilder::new(
        Kind::Merge.into_sdk_custom_kind(),
        "MERGE",
        &tags,
    )
    .to_unsigned_event(keys.public_key())
    .sign(&keys)
    .unwrap()
}
