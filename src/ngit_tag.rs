use core::fmt;
use std::str::FromStr;

use nostr::{Tag, prelude::{self, UncheckedUrl}, EventId};

/// Tag kind
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum TagKind {
    /// Group
    Group,
    /// Admin group
    AdminGroup,
    /// Repository
    Repo,
    /// Branch
    Branch,
    /// Branch merged from
    BranchMergeFrom,
    /// Patch
    Patch,
    /// Patch Parent
    PatchParent,
    /// Commit
    Commit,
    /// Commit Parent
    CommitParent,
    /// Commit Message
    CommitMessage,
    /// Initial Commit
    InitialCommit,
    /// Relays
    Relays,
    /// Custom tag kind
    Custom(String),
}

impl fmt::Display for TagKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Group => write!(f, "group"),
            Self::AdminGroup => write!(f, "admin-group"),
            Self::Repo => write!(f, "repo"),
            Self::Branch => write!(f, "branch"),
            Self::BranchMergeFrom => write!(f, "from-branch"),
            Self::Patch => write!(f, "patch"),
            Self::PatchParent => write!(f, "parent-patch"),
            Self::Commit => write!(f, "commit"),
            Self::CommitParent => write!(f, "parent-commit"),
            Self::CommitMessage => write!(f, "commit-message"),
            Self::InitialCommit => write!(f, "initial-commit"),
            Self::Relays => write!(f, "relays"),
            Self::Custom(tag) => write!(f, "{tag}"),
        }
    }
}

impl<S> From<S> for TagKind
where
    S: Into<String>,
{
    fn from(s: S) -> Self {
        let s: String = s.into();
        match s.as_str() {
            "group" => Self::Group,
            "admin-group" => Self::AdminGroup,
            "repo" => Self::Repo, // single letter tags are searchable under NIP-12
            "branch" => Self::Branch, // single letter tags are searchable under NIP-12
            "from-branch" => Self::BranchMergeFrom,
            "patch" => Self::Patch,
            "parent-patch" => Self::PatchParent,
            "commit" => Self::Commit,
            "parent-commit" => Self::CommitParent,
            "commit-message" => Self::CommitMessage,
            "initial-commit" => Self::InitialCommit,
            "relays" => Self::Relays,
            tag => Self::Custom(tag.to_string()),
        }
    }
}

fn tag(label:TagKind,value:&String) -> Tag {
    Tag::Generic(
        prelude::TagKind::Custom(label.to_string()),
        vec![value.clone()],
    )
}
fn tag_multi_value(label:TagKind,value:&Vec<String>) -> Tag {
    Tag::Generic(
        prelude::TagKind::Custom(label.to_string()),
        value.clone(),
    )
}

pub fn tag_group(event_id: &String) -> Tag { tag(TagKind::Group, event_id) }
pub fn tag_group_with_relays(group_id: &String, vec_relays: &Vec<String>) -> Tag {
    let mut combined = vec![group_id.clone()];
    for r in vec_relays {
        combined.push(r.to_string());
    }
    tag_multi_value(
        TagKind::Group,
        &combined,
    )
}
pub fn tag_admin_group(event_id: &String) -> Tag { tag(TagKind::AdminGroup, event_id) }
pub fn tag_admin_group_with_relays(group_id: &String, vec_relays: &Vec<UncheckedUrl>) -> Tag {
    let mut combined = vec![group_id.clone()];
    for r in vec_relays {
        combined.push(r.to_string());
    }
    tag_multi_value(
        TagKind::AdminGroup,
        &combined,
    )
}
// takes a tag referencing an event with optional relays and turns it into an e tag which is fiterable. perfect for tag_repo, tag_branch, etc.
pub fn tag_into_event(tag:Tag) -> Tag {
    Tag::Event(
        tag_extract_value_as_event_id(&tag),
        tag_extract_relays(&tag).get(0).cloned(),
        None,
    )
}
pub fn tag_repo(event_id: &String) -> Tag { tag(TagKind::Repo, event_id) }
pub fn tag_branch(event_id: &String) -> Tag { tag(TagKind::Branch, event_id) }
pub fn tag_branch_merge_from(event_id: &String) -> Tag { tag(TagKind::BranchMergeFrom, event_id) }
pub fn tag_patch(event_id: &String) -> Tag { tag(TagKind::Patch, event_id) }
pub fn tag_patch_parent(event_id: &String) -> Tag { tag(TagKind::PatchParent, event_id) }
pub fn tag_commit(commit_id: &String) -> Tag { tag(TagKind::Commit, commit_id) }
pub fn tag_commit_parent(commit_id: &String) -> Tag { tag(TagKind::CommitParent, commit_id) }
pub fn tag_commit_message(message: &String) -> Tag { tag(TagKind::CommitMessage, message) }
pub fn tag_initial_commit() -> Tag { Tag::Hashtag(TagKind::InitialCommit.to_string()) }
pub fn tag_relays(relays:&Vec<String>) -> Tag { 
    let mut relays_unchecked_url = vec![];
    for r in relays {
        relays_unchecked_url.push(
            UncheckedUrl::from_str(r)
                .expect("relay in string to not produce error on uncheckedUrl"),
        )
    }
    Tag::Relays(relays_unchecked_url)
}
pub fn tag_hashtag(hashtag:&str) -> Tag { Tag::Hashtag(hashtag.to_string()) }
pub fn tag_extract_value(tag:&Tag) -> String { tag.as_vec()[1].clone() }
pub fn tag_extract_value_as_event_id(tag:&Tag) -> EventId {
    EventId::from_str(tag.as_vec()[1].clone().as_str())
        .expect("first tag value is a event id")
}
pub fn tag_extract_relays(tag:&Tag) -> Vec<UncheckedUrl> {
    let mut relays = vec![];
    let tag_vec = tag.as_vec();
    for (i, s) in tag_vec.iter().enumerate() {
        if i > 1 || (
            i == 1 &&  tag_vec[0] == TagKind::Relays.to_string()
        )
         {
            relays.push(
                UncheckedUrl::from_str(s)
                .expect("relay strings to not produce error on uncheckedUrl"),
            );
        }
    }
    relays
}

pub fn tag_is_group(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::Group.to_string() }
pub fn tag_is_admin_group(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::AdminGroup.to_string() }
pub fn tag_is_repo(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::Repo.to_string() }
pub fn tag_is_branch(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::Branch.to_string() }
pub fn tag_is_branch_merged_from(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::BranchMergeFrom.to_string() }
pub fn tag_is_patch(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::Patch.to_string() }
pub fn tag_is_patch_parent(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::PatchParent.to_string() }
pub fn tag_is_commit(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::Commit.to_string() }
pub fn tag_is_commit_parent(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::CommitParent.to_string() }
pub fn tag_is_commit_message(tag:&Tag) -> bool { tag.kind().to_string() == TagKind::CommitMessage.to_string() }
pub fn tag_is_initial_commit(tag:&Tag) -> bool {
    tag.kind().to_string() == prelude::TagKind::T.to_string()
    && tag.as_vec()[1] ==  TagKind::InitialCommit.to_string()
}
pub fn tag_is_relays(tag:Tag) -> bool { tag.kind().to_string() == TagKind::Relays.to_string() }
