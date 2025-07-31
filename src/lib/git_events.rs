use std::{str::FromStr, sync::Arc};

use anyhow::{Context, Result, bail};
use nostr::{
    event::UnsignedEvent,
    nips::{nip01::Coordinate, nip10::Marker, nip19::Nip19},
};
use nostr_sdk::{
    Event, EventBuilder, EventId, FromBech32, Kind, NostrSigner, PublicKey, Tag, TagKind,
    TagStandard, hashes::sha1::Hash as Sha1Hash,
};

use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::sign_event,
    git::{Repo, RepoActions},
    repo_ref::RepoRef,
};

pub fn tag_value(event: &Event, tag_name: &str) -> Result<String> {
    Ok(event
        .tags
        .iter()
        .find(|t| !t.as_slice().is_empty() && t.as_slice()[0].eq(tag_name))
        .context(format!("tag '{tag_name}'not present"))?
        .as_slice()[1]
        .clone())
}

pub fn get_commit_id_from_patch(event: &Event) -> Result<String> {
    let value = tag_value(event, "commit");

    if value.is_ok() {
        value
    } else if event.content.starts_with("From ") && event.content.len().gt(&45) {
        Ok(event.content[5..45].to_string())
    } else {
        bail!("event is not a patch")
    }
}

pub fn get_event_root(event: &nostr::Event) -> Result<EventId> {
    Ok(EventId::parse(
        event
            .tags
            .iter()
            .find(|t| t.is_root())
            .context("no thread root in event")?
            .as_slice()
            .get(1)
            .unwrap(),
    )?)
}

pub fn status_kinds() -> Vec<Kind> {
    vec![
        Kind::GitStatusOpen,
        Kind::GitStatusApplied,
        Kind::GitStatusClosed,
        Kind::GitStatusDraft,
    ]
}

pub const KIND_PULL_REQUEST: Kind = Kind::Custom(1618);
pub const KIND_PULL_REQUEST_UPDATE: Kind = Kind::Custom(1619);

pub fn event_is_patch_set_root(event: &Event) -> bool {
    event.kind.eq(&Kind::GitPatch)
        && event
            .tags
            .iter()
            .any(|t| t.as_slice().len() > 1 && t.as_slice()[1].eq("root"))
}

pub fn event_is_revision_root(event: &Event) -> bool {
    (event.kind.eq(&Kind::GitPatch)
        && event
            .tags
            .iter()
            .any(|t| t.as_slice().len() > 1 && t.as_slice()[1].eq("revision-root")))
        || (event.kind.eq(&KIND_PULL_REQUEST)
            && event
                .tags
                .iter()
                .any(|t| t.as_slice().len() > 1 && t.as_slice()[0].eq("e")))
}

pub fn patch_supports_commit_ids(event: &Event) -> bool {
    event.kind.eq(&Kind::GitPatch)
        && event
            .tags
            .iter()
            .any(|t| !t.as_slice().is_empty() && t.as_slice()[0].eq("commit-pgp-sig"))
}

pub fn event_is_valid_pr_or_pr_update(event: &Event) -> bool {
    [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&event.kind)
        && event.tags.iter().any(|t| {
            t.as_slice().len().gt(&1)
                && t.as_slice()[0].eq("c")
                && git2::Oid::from_str(&t.as_slice()[1]).is_ok()
        })
        && event
            .tags
            .iter()
            .any(|t| t.as_slice().len().gt(&1) && t.as_slice()[0].eq("clone"))
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub async fn generate_patch_event(
    git_repo: &Repo,
    root_commit: &Sha1Hash,
    commit: &Sha1Hash,
    thread_event_id: Option<nostr::EventId>,
    signer: &Arc<dyn NostrSigner>,
    repo_ref: &RepoRef,
    parent_patch_event_id: Option<nostr::EventId>,
    series_count: Option<(u64, u64)>,
    branch_name: Option<String>,
    root_proposal_id: &Option<String>,
    mentions: &[nostr::Tag],
) -> Result<nostr::Event> {
    let commit_parent = git_repo
        .get_commit_parent(commit)
        .context("failed to get parent commit")?;
    let relay_hint = repo_ref.relays.first().cloned();

    sign_event(
        EventBuilder::new(
            nostr::event::Kind::GitPatch,
            git_repo
                .make_patch_from_commit(commit, &series_count)
                .context(format!("failed to make patch for commit {commit}"))?,
        )
        .tags(
            [
                repo_ref
                    .maintainers
                    .iter()
                    .map(|m| {
                        Tag::from_standardized(TagStandard::Coordinate {
                            coordinate: Coordinate {
                                kind: nostr::Kind::GitRepoAnnouncement,
                                public_key: *m,
                                identifier: repo_ref.identifier.to_string(),
                            },
                            relay_url: repo_ref.relays.first().cloned(),
                            uppercase: false,
                        })
                    })
                    .collect::<Vec<Tag>>(),
                vec![
                    Tag::from_standardized(TagStandard::Reference(root_commit.to_string())),
                    // commit id reference is a trade-off. its now
                    // unclear which one is the root commit id but it
                    // enables easier location of code comments againt
                    // code that makes it into the main branch, assuming
                    // the commit id is correct
                    Tag::from_standardized(TagStandard::Reference(commit.to_string())),
                    Tag::custom(
                        TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                        vec![format!(
                            "git patch: {}",
                            git_repo
                                .get_commit_message_summary(commit)
                                .unwrap_or_default()
                        )],
                    ),
                ],
                if let Some(thread_event_id) = thread_event_id {
                    vec![Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: thread_event_id,
                        relay_url: relay_hint.clone(),
                        marker: Some(Marker::Root),
                        public_key: None,
                        uppercase: false,
                    })]
                } else if let Some(event_ref) = root_proposal_id.clone() {
                    vec![
                        Tag::hashtag("root"),
                        Tag::hashtag("revision-root"),
                        // TODO check if id is for a root proposal (perhaps its for an issue?)
                        event_tag_from_nip19_or_hex(
                            &event_ref,
                            "proposal",
                            EventRefType::Reply,
                            false,
                            false,
                        )?,
                    ]
                } else {
                    vec![Tag::hashtag("root")]
                },
                mentions.to_vec(),
                if let Some(id) = parent_patch_event_id {
                    vec![Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: id,
                        relay_url: relay_hint.clone(),
                        marker: Some(Marker::Reply),
                        public_key: None,
                        uppercase: false,
                    })]
                } else {
                    vec![]
                },
                // see comment on branch names in cover letter event creation
                if let Some(branch_name) = branch_name {
                    if thread_event_id.is_none() {
                        vec![Tag::custom(
                            TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
                            vec![branch_name.chars().take(60).collect::<String>()],
                        )]
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                },
                // whilst it is in nip34 draft to tag the maintainers
                // I'm not sure it is a good idea because if they are
                // interested in all patches then their specialised
                // client should subscribe to patches tagged with the
                // repo reference. maintainers of large repos will not
                // be interested in every patch.
                repo_ref
                    .maintainers
                    .iter()
                    .map(|pk| Tag::public_key(*pk))
                    .collect(),
                vec![
                    // a fallback is now in place to extract this from the patch
                    Tag::custom(
                        TagKind::Custom(std::borrow::Cow::Borrowed("commit")),
                        vec![commit.to_string()],
                    ),
                    // this is required as patches cannot be relied upon to include the 'base
                    // commit'
                    Tag::custom(
                        TagKind::Custom(std::borrow::Cow::Borrowed("parent-commit")),
                        vec![commit_parent.to_string()],
                    ),
                    // this is required to ensure the commit id matches
                    Tag::custom(
                        TagKind::Custom(std::borrow::Cow::Borrowed("commit-pgp-sig")),
                        vec![
                            git_repo
                                .extract_commit_pgp_signature(commit)
                                .unwrap_or_default(),
                        ],
                    ),
                    // removing description tag will not cause anything to break
                    Tag::from_standardized(nostr_sdk::TagStandard::Description(
                        git_repo.get_commit_message(commit)?.to_string(),
                    )),
                    Tag::custom(
                        TagKind::Custom(std::borrow::Cow::Borrowed("author")),
                        git_repo.get_commit_author(commit)?,
                    ),
                    // this is required to ensure the commit id matches
                    Tag::custom(
                        TagKind::Custom(std::borrow::Cow::Borrowed("committer")),
                        git_repo.get_commit_comitter(commit)?,
                    ),
                ],
            ]
            .concat(),
        ),
        signer,
        if let Some((n, total)) = series_count {
            format!("commit {n}/{total}")
        } else {
            "commit 1/1".to_string()
        },
    )
    .await
    .context("failed to sign event")
}

#[derive(Debug, PartialEq)]
pub enum EventRefType {
    Root,
    Reply,
    Quote,
}

pub fn event_tag_from_nip19_or_hex(
    reference: &str,
    reference_name: &str,
    ref_type: EventRefType,
    allow_npub_reference: bool,
    prompt_for_correction: bool,
) -> Result<nostr::Tag> {
    let mut bech32 = reference.to_string();
    loop {
        if bech32.is_empty() {
            bech32 = Interactor::default().input(
                PromptInputParms::default().with_prompt(format!("{reference_name} reference")),
            )?;
        }
        let marker = match ref_type {
            EventRefType::Root => Some(Marker::Root),
            EventRefType::Reply => Some(Marker::Reply),
            EventRefType::Quote => None,
        };
        if let Ok(nip19) = Nip19::from_bech32(&bech32) {
            match nip19 {
                Nip19::Event(n) => {
                    if ref_type == EventRefType::Quote {
                        break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Quote {
                            event_id: n.event_id,
                            relay_url: n.relays.first().cloned(),
                            public_key: None,
                        }));
                    }
                    break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: n.event_id,
                        relay_url: n.relays.first().cloned(),
                        marker,
                        public_key: None,
                        uppercase: false,
                    }));
                }
                Nip19::EventId(id) => {
                    if ref_type == EventRefType::Quote {
                        break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Quote {
                            event_id: id,
                            relay_url: None,
                            public_key: None,
                        }));
                    }
                    break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: id,
                        relay_url: None,
                        marker,
                        public_key: None,
                        uppercase: false,
                    }));
                }
                Nip19::Coordinate(coordinate) => {
                    break Ok(Tag::from_standardized(TagStandard::Coordinate {
                        coordinate: coordinate.coordinate,
                        relay_url: coordinate.relays.first().cloned(),
                        uppercase: false,
                    }));
                }
                Nip19::Profile(profile) => {
                    if allow_npub_reference {
                        break Ok(Tag::public_key(profile.public_key));
                    }
                }
                Nip19::Pubkey(public_key) => {
                    if allow_npub_reference {
                        break Ok(Tag::public_key(public_key));
                    }
                }
                _ => {}
            }
        }
        if let Ok(id) = nostr::EventId::from_str(&bech32) {
            break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                event_id: id,
                relay_url: None,
                marker,
                public_key: None,
                uppercase: false,
            }));
        }
        if prompt_for_correction {
            println!("not a valid {reference_name} event reference");
        } else {
            bail!(format!("not a valid {reference_name} event reference"));
        }

        bech32 = String::new();
    }
}

pub fn generate_unsigned_pr_or_update_event(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    signing_public_key: &PublicKey,
    root_proposal: Option<&Event>,
    commit: &Sha1Hash,
    clone_url_hint: &[&str],
    mentions: &[nostr::Tag],
) -> Result<UnsignedEvent> {
    let root_patch_cover_letter = if let Some(root_proposal) = root_proposal {
        if root_proposal.kind.eq(&Kind::GitPatch) {
            Some(event_to_cover_letter(root_proposal)?)
        } else {
            None
        }
    } else {
        None
    };

    let title = if let Some(cl) = &root_patch_cover_letter {
        cl.title.clone()
    } else {
        git_repo.get_commit_message_summary(commit)?
    };

    let description = if let Some(cl) = &root_patch_cover_letter {
        cl.description.clone()
    } else {
        let mut description = git_repo.get_commit_message(commit)?.trim().to_string();
        if let Some(remaining_description) = description.strip_prefix(&title) {
            description = remaining_description.trim().to_string();
        }
        description
    };

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    let pr_update_specific_tags = |root_proposal: &Event| {
        vec![
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                vec![format!("git Pull Request Update")],
            ),
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("E")),
                vec![root_proposal.id],
            ),
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("P")),
                vec![root_proposal.pubkey],
            ),
        ]
    };
    let pr_specific_tags = || {
        [
            vec![
                Tag::from_standardized(TagStandard::Subject(title.clone())),
                Tag::custom(
                    nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                    vec![format!("git Pull Request: {}", title.clone())],
                ),
            ],
            if let Some(cl) = &root_patch_cover_letter {
                vec![
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("e")),
                        vec![root_proposal.unwrap().id],
                    ),
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
                        vec![cl.branch_name_without_id_or_prefix.clone()],
                    ),
                    Tag::public_key(root_proposal.unwrap().pubkey),
                ]
            } else if let Some(branch_name_tag) =
                make_branch_name_tag_from_check_out_branch(git_repo)
            {
                vec![branch_name_tag]
            } else {
                vec![]
            },
        ]
        .concat()
    };

    Ok(
        if root_proposal.is_some() && root_patch_cover_letter.is_none() {
            EventBuilder::new(KIND_PULL_REQUEST_UPDATE, "")
        } else {
            EventBuilder::new(KIND_PULL_REQUEST, description)
        }
        .tags(
            [
                repo_ref
                    .maintainers
                    .iter()
                    .map(|m| {
                        Tag::from_standardized(TagStandard::Coordinate {
                            coordinate: Coordinate {
                                kind: nostr::Kind::GitRepoAnnouncement,
                                public_key: *m,
                                identifier: repo_ref.identifier.to_string(),
                            },
                            relay_url: repo_ref.relays.first().cloned(),
                            uppercase: false,
                        })
                    })
                    .collect::<Vec<Tag>>(),
                mentions.to_vec(),
                if let Some(root_proposal) = root_proposal {
                    if root_patch_cover_letter.is_none() {
                        pr_update_specific_tags(root_proposal)
                    } else {
                        pr_specific_tags()
                    }
                } else {
                    pr_specific_tags()
                },
                vec![
                    Tag::from_standardized(TagStandard::Reference(format!("{root_commit}"))),
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("c")),
                        vec![format!("{commit}")],
                    ),
                    Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("clone")),
                        clone_url_hint
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>(),
                    ),
                ],
                repo_ref
                    .maintainers
                    .iter()
                    .map(|pk| Tag::public_key(*pk))
                    .collect(),
            ]
            .concat(),
        )
        .build(*signing_public_key),
    )
}

fn make_branch_name_tag_from_check_out_branch(git_repo: &Repo) -> Option<Tag> {
    if let Ok(branch_name) = git_repo.get_checked_out_branch_name() {
        if !branch_name.eq("main")
            && !branch_name.eq("master")
            && !branch_name.eq("origin/main")
            && !branch_name.eq("origin/master")
        {
            Some(Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
                vec![
                    if let Some(branch_name) = branch_name.strip_prefix("pr/") {
                        branch_name.to_string()
                    } else {
                        branch_name
                    }
                    .chars()
                    .take(60)
                    .collect::<String>(),
                ],
            ))
        } else {
            None
        }
    } else {
        None
    }
}

#[allow(clippy::too_many_lines)]
pub async fn generate_cover_letter_and_patch_events(
    cover_letter_title_description: Option<(String, String)>,
    git_repo: &Repo,
    commits: &[Sha1Hash],
    signer: &Arc<dyn NostrSigner>,
    repo_ref: &RepoRef,
    root_proposal_id: &Option<String>,
    mentions: &[nostr::Tag],
) -> Result<Vec<nostr::Event>> {
    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    let mut events = vec![];

    if let Some((title, description)) = cover_letter_title_description {
        events.push(sign_event(EventBuilder::new(
        nostr::event::Kind::GitPatch,
        format!(
            "From {} Mon Sep 17 00:00:00 2001\nSubject: [PATCH 0/{}] {title}\n\n{description}",
            commits.last().unwrap(),
            commits.len()
        ))
        .tags(
        [
            repo_ref.maintainers.iter().map(|m|
                Tag::from_standardized(TagStandard::Coordinate {
                    coordinate: Coordinate {
                        kind: nostr::Kind::GitRepoAnnouncement,
                        public_key: *m,
                        identifier: repo_ref.identifier.to_string(),
                    },
                    relay_url: repo_ref.relays.first().cloned(),
                    uppercase: false,
                })
            ).collect::<Vec<Tag>>(),
            vec![
                Tag::from_standardized(TagStandard::Reference(format!("{root_commit}"))),
                Tag::hashtag("cover-letter"),
                Tag::custom(
                    nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                    vec![format!("git patch cover letter: {}", title.clone())],
                ),
            ],
            if let Some(event_ref) = root_proposal_id.clone() {
                vec![
                    Tag::hashtag("root"),
                    Tag::hashtag("revision-root"),
                    // TODO check if id is for a root proposal (perhaps its for an issue?)
                    event_tag_from_nip19_or_hex(&event_ref,"proposal",EventRefType::Reply, false, false)?,
                ]
            } else {
                vec![
                    Tag::hashtag("root"),
                ]
            },
            mentions.to_vec(),
            // this is not strictly needed but makes for prettier branch names
            // eventually a prefix will be needed of the event id to stop 2 proposals with the same name colliding
            // a change like this, or the removal of this tag will require the actual branch name to be tracked
            // so pulling and pushing still work
            if let Some(branch_name_tag) = make_branch_name_tag_from_check_out_branch(git_repo) {
                vec![branch_name_tag]
            } else {
                vec![]
            },
            repo_ref.maintainers
                .iter()
                .map(|pk| Tag::public_key(*pk))
                .collect(),
        ].concat(),
    ),
    signer,
    format!("commit 0/{}",commits.len()),
).await
    .context("failed to create cover-letter event")?);
    }

    for (i, commit) in commits.iter().enumerate() {
        events.push(
            generate_patch_event(
                git_repo,
                &root_commit,
                commit,
                events.first().map(|event| event.id),
                signer,
                repo_ref,
                events.last().map(|e| e.id),
                if events.is_empty() && commits.len().eq(&1) {
                    None
                } else {
                    Some(((i + 1).try_into()?, commits.len().try_into()?))
                },
                if events.is_empty() {
                    if let Ok(branch_name) = git_repo.get_checked_out_branch_name() {
                        if !branch_name.eq("main")
                            && !branch_name.eq("master")
                            && !branch_name.eq("origin/main")
                            && !branch_name.eq("origin/master")
                        {
                            Some(
                                if let Some(branch_name) = branch_name.strip_prefix("pr/") {
                                    branch_name.to_string()
                                } else {
                                    branch_name
                                }
                                .chars()
                                .take(60)
                                .collect::<String>(),
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                },
                root_proposal_id,
                if events.is_empty() { mentions } else { &[] },
            )
            .await
            .context("failed to generate patch event")?,
        );
    }
    Ok(events)
}

pub struct CoverLetter {
    pub title: String,
    pub description: String,
    pub branch_name_without_id_or_prefix: String,
    pub event_id: Option<nostr::EventId>,
}

impl CoverLetter {
    pub fn get_branch_name_with_pr_prefix_and_shorthand_id(&self) -> Result<String> {
        Ok(format!(
            "pr/{}({})",
            self.branch_name_without_id_or_prefix,
            &self
                .event_id
                .context("proposal root event_id must be know to get it's branch name")?
                .to_hex()
                .as_str()[..8],
        ))
    }
}
pub fn event_is_cover_letter(event: &nostr::Event) -> bool {
    // TODO: look for Subject:[ PATCH 0/n ] but watch out for:
    //   [PATCH v1 0/n ] or
    //   [PATCH subsystem v2 0/n ]
    event.kind.eq(&Kind::GitPatch)
        && event
            .tags
            .iter()
            .any(|t| t.as_slice().len() > 1 && t.as_slice()[1].eq("root"))
        && event
            .tags
            .iter()
            .any(|t| t.as_slice().len() > 1 && t.as_slice()[1].eq("cover-letter"))
}

pub fn commit_msg_from_patch(patch: &nostr::Event) -> Result<String> {
    if let Ok(msg) = tag_value(patch, "description") {
        Ok(msg)
    } else {
        let start_index = patch
            .content
            .find("] ")
            .context("event is not formatted as a patch or cover letter")?
            + 2;
        let end_index = patch.content[start_index..]
            .find("\ndiff --git")
            .unwrap_or(patch.content.len());
        Ok(patch.content[start_index..end_index].to_string())
    }
}

pub fn commit_msg_from_patch_oneliner(patch: &nostr::Event) -> Result<String> {
    Ok(commit_msg_from_patch(patch)?
        .split('\n')
        .collect::<Vec<&str>>()[0]
        .to_string())
}

pub fn event_to_cover_letter(event: &nostr::Event) -> Result<CoverLetter> {
    if !event.kind.eq(&KIND_PULL_REQUEST) && !event_is_patch_set_root(event) {
        bail!("event is not a patch set root event (root patch or cover letter)")
    }

    let title = if event.kind.eq(&KIND_PULL_REQUEST) {
        tag_value(event, "subject").unwrap_or("untitled".to_owned())
    } else {
        commit_msg_from_patch_oneliner(event)?
    };
    let description = if event.kind.eq(&KIND_PULL_REQUEST) {
        event.content.clone()
    } else {
        commit_msg_from_patch(event)?[title.len()..]
            .trim()
            .to_string()
    };

    Ok(CoverLetter {
        title: title.clone(),
        description,
        branch_name_without_id_or_prefix: if let Ok(name) = tag_value(event, "branch-name") {
            if !name.eq("main") && !name.eq("master") {
                safe_branch_name_for_pr(&name)
            } else {
                safe_branch_name_for_pr(&title)
            }
        } else {
            safe_branch_name_for_pr(&title)
        },
        event_id: Some(event.id),
    })
}

fn safe_branch_name_for_pr(s: &str) -> String {
    s.replace(' ', "-")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.eq(&'/') {
                c
            } else {
                '-'
            }
        })
        .take(60)
        .collect()
}

pub fn get_pr_tip_event_or_most_recent_patch_with_ancestors(
    mut proposal_events: Vec<nostr::Event>,
) -> Result<Vec<nostr::Event>> {
    proposal_events.sort_by_key(|e| e.created_at);

    let youngest = proposal_events.last().context("no proposal events found")?;

    let events_with_youngest_created_at: Vec<&nostr::Event> = proposal_events
        .iter()
        .filter(|p| p.created_at.eq(&youngest.created_at))
        .collect();

    let mut res = vec![];

    let mut event_id_to_search = events_with_youngest_created_at
        .clone()
        .iter()
        .find(|p| {
            !events_with_youngest_created_at.iter().any(|p2| {
                if let Ok(reply_to) = get_event_parent_id(p2) {
                    reply_to.eq(&p.id.to_string())
                } else {
                    false
                }
            })
        })
        .context("failed to find events_with_youngest_created_at")?
        .id
        .to_string();

    while let Some(event) = proposal_events
        .iter()
        .find(|e| e.id.to_string().eq(&event_id_to_search))
    {
        res.push(event.clone());
        if [KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE].contains(&event.kind)
            || event_is_patch_set_root(event)
        {
            break;
        }
        event_id_to_search = get_event_parent_id(event).unwrap_or_default();
    }
    Ok(res)
}

fn get_event_parent_id(event: &nostr::Event) -> Result<String> {
    Ok(if let Some(reply_tag) = event
        .tags
        .iter()
        .find(|t| t.as_slice().len().gt(&3) && t.as_slice()[3].eq("reply"))
    {
        reply_tag
    } else {
        event
            .tags
            .iter()
            .find(|t| t.as_slice().len().gt(&3) && t.as_slice()[3].eq("root"))
            .context("no reply or root e tag present".to_string())?
    }
    .as_slice()[1]
        .clone())
}

pub fn is_event_proposal_root_for_branch(
    e: &Event,
    branch_name_or_refstr: &str,
    logged_in_user: Option<&PublicKey>,
) -> Result<bool> {
    let branch_name = branch_name_or_refstr.replace("refs/heads/", "");
    Ok(event_to_cover_letter(e).is_ok_and(|cl| {
        (logged_in_user.is_some_and(|public_key| e.pubkey.eq(public_key))
            && branch_name.eq(&format!("pr/{}", cl.branch_name_without_id_or_prefix)))
            || cl
                .get_branch_name_with_pr_prefix_and_shorthand_id()
                .is_ok_and(|s| s.eq(&branch_name))
    }) && (
        // If we wanted to treat to list Pull Requests that revise a Patch we would do this:
        // Note: whilst this the the case elsewhere event_is_revision_root is used, there is more to
        //       think about here?
        // e.kind.eq(&KIND_PULL_REQUEST) ||
        !event_is_revision_root(e)
    ))
}

pub fn get_status(
    proposal: &Event,
    repo_ref: &RepoRef,
    all_status_in_repo: &[Event],
    all_pr_roots_in_repo: &[Event],
) -> Kind {
    let get_direct_status = |proposal: &Event| {
        if let Some(e) = all_status_in_repo
            .iter()
            .filter(|e| {
                status_kinds().contains(&e.kind)
                    && e.tags.iter().any(|t| {
                        t.as_slice().len() > 1 && t.as_slice()[1].eq(&proposal.id.to_string())
                    })
                    && (proposal.pubkey.eq(&e.pubkey) || repo_ref.maintainers.contains(&e.pubkey))
            })
            .collect::<Vec<&nostr::Event>>()
            .first()
        {
            e.kind
        } else {
            Kind::GitStatusOpen
        }
    };
    let is_proposal_pr_revision_of_patch = |proposal: &Event, patch: &Event| {
        proposal.kind.eq(&KIND_PULL_REQUEST)
            && proposal.tags.clone().into_iter().any(|t| {
                t.as_slice().len() > 1
                    && t.as_slice()[0].eq("e")
                    && t.as_slice()[1].eq(&patch.id.to_string())
            })
    };

    let direct_status = get_direct_status(proposal);
    if direct_status.eq(&Kind::GitStatusClosed) && proposal.kind.eq(&Kind::GitPatch) {
        if let Some(pr_revision) = all_pr_roots_in_repo
            .iter()
            .find(|p| is_proposal_pr_revision_of_patch(p, proposal))
        {
            get_direct_status(pr_revision)
        } else {
            direct_status
        }
    } else {
        direct_status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod event_to_cover_letter {
        use super::*;

        fn generate_cover_letter(title: &str, description: &str) -> Result<nostr::Event> {
            Ok(nostr::event::EventBuilder::new(
                nostr::event::Kind::GitPatch,
                format!("From ea897e987ea9a7a98e7a987e97987ea98e7a3334 Mon Sep 17 00:00:00 2001\nSubject: [PATCH 0/2] {title}\n\n{description}"),
                )
            .tags([
                    Tag::hashtag("cover-letter"),
                    Tag::hashtag("root"),
                ],
            )
            .sign_with_keys(&nostr::Keys::generate())?)
        }

        #[test]
        fn basic_title() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter("the title", "description here")?)?
                    .title,
                "the title",
            );
            Ok(())
        }

        #[test]
        fn basic_description() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter("the title", "description here")?)?
                    .description,
                "description here",
            );
            Ok(())
        }

        #[test]
        fn description_trimmed() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title",
                    " \n \ndescription here\n\n "
                )?)?
                .description,
                "description here",
            );
            Ok(())
        }

        #[test]
        fn multi_line_description() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title",
                    "description here\n\nmore here\nmore"
                )?)?
                .description,
                "description here\n\nmore here\nmore",
            );
            Ok(())
        }

        #[test]
        fn new_lines_in_title_forms_part_of_description() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title\nwith new line",
                    "description here\n\nmore here\nmore"
                )?)?
                .title,
                "the title",
            );
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title\nwith new line",
                    "description here\n\nmore here\nmore"
                )?)?
                .description,
                "with new line\n\ndescription here\n\nmore here\nmore",
            );
            Ok(())
        }

        mod blank_description {
            use super::*;

            #[test]
            fn title_correct() -> Result<()> {
                assert_eq!(
                    event_to_cover_letter(&generate_cover_letter("the title", "")?)?.title,
                    "the title",
                );
                Ok(())
            }

            #[test]
            fn description_is_empty_string() -> Result<()> {
                assert_eq!(
                    event_to_cover_letter(&generate_cover_letter("the title", "")?)?.description,
                    "",
                );
                Ok(())
            }
        }
    }
}
