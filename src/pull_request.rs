use nostr::{Event, EventBuilder, Keys };


use crate::{ngit_tag::{tag_repo, tag_branch, tag_branch_merge_from, tag_hashtag, tag_into_event}, kind::Kind};

pub fn initialize_pull_request(
    keys: &Keys,
    repoistory:&String,
    to_branch_id: &String,
    from_branch_id: &String,
    title: &String,
    description: &String,
    hashtags: Vec<String>,

) -> Event {
    let mut tags = vec![
        tag_repo(repoistory),
        tag_into_event(tag_repo(repoistory)),
        tag_branch(to_branch_id),
        tag_into_event(tag_branch(to_branch_id)),
        tag_branch_merge_from(from_branch_id),
        tag_into_event(tag_branch_merge_from(from_branch_id)),
        tag_hashtag("ngit-event"),
        tag_hashtag("ngit-format-0.0.1"),
];
    for t in hashtags.iter() {
        tags.push(
            nostr::Tag::Hashtag(t.to_string())
        )
    }
    EventBuilder::new(
        Kind::PullRequest.into_sdk_custom_kind(),
        format!("PullRequest\n{title}\n\n{description}"),
        &tags,
    )
    .to_unsigned_event(keys.public_key())
    .sign(&keys)
    .unwrap()
}
