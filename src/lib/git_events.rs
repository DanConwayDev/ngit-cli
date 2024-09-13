use std::str::FromStr;

use anyhow::{bail, Context, Result};
use nostr::nips::{nip01::Coordinate, nip10::Marker, nip19::Nip19};
use nostr_sdk::{
    hashes::sha1::Hash as Sha1Hash, Event, EventBuilder, EventId, FromBech32, Kind, PublicKey, Tag,
    TagKind, TagStandard, UncheckedUrl,
};
use nostr_signer::NostrSigner;

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
        .find(|t| t.as_vec()[0].eq(tag_name))
        .context(format!("tag '{tag_name}'not present"))?
        .as_vec()[1]
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
            .tags()
            .iter()
            .find(|t| t.is_root())
            .context("no thread root in event")?
            .as_vec()
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

pub fn event_is_patch_set_root(event: &Event) -> bool {
    event.kind.eq(&Kind::GitPatch) && event.tags().iter().any(|t| t.as_vec()[1].eq("root"))
}

pub fn event_is_revision_root(event: &Event) -> bool {
    event.kind.eq(&Kind::GitPatch)
        && event
            .tags()
            .iter()
            .any(|t| t.as_vec()[1].eq("revision-root"))
}

pub fn patch_supports_commit_ids(event: &Event) -> bool {
    event.kind.eq(&Kind::GitPatch)
        && event
            .tags()
            .iter()
            .any(|t| t.as_vec()[0].eq("commit-pgp-sig"))
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub async fn generate_patch_event(
    git_repo: &Repo,
    root_commit: &Sha1Hash,
    commit: &Sha1Hash,
    thread_event_id: Option<nostr::EventId>,
    signer: &nostr_sdk::NostrSigner,
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
    let relay_hint = repo_ref.relays.first().map(nostr::UncheckedUrl::from);

    sign_event(
        EventBuilder::new(
            nostr::event::Kind::GitPatch,
            git_repo
                .make_patch_from_commit(commit, &series_count)
                .context(format!("cannot make patch for commit {commit}"))?,
            [
                repo_ref
                    .maintainers
                    .iter()
                    .map(|m| {
                        Tag::coordinate(Coordinate {
                            kind: nostr::Kind::GitRepoAnnouncement,
                            public_key: *m,
                            identifier: repo_ref.identifier.to_string(),
                            relays: repo_ref.relays.clone(),
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
                    })]
                } else if let Some(event_ref) = root_proposal_id.clone() {
                    vec![
                        Tag::hashtag("root"),
                        Tag::hashtag("revision-root"),
                        // TODO check if id is for a root proposal (perhaps its for an issue?)
                        event_tag_from_nip19_or_hex(
                            &event_ref,
                            "proposal",
                            Marker::Reply,
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
                    })]
                } else {
                    vec![]
                },
                // see comment on branch names in cover letter event creation
                if let Some(branch_name) = branch_name {
                    if thread_event_id.is_none() {
                        vec![Tag::custom(
                            TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
                            vec![branch_name.to_string()],
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
    )
    .await
    .context("failed to sign event")
}

pub fn event_tag_from_nip19_or_hex(
    reference: &str,
    reference_name: &str,
    marker: Marker,
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
        if let Ok(nip19) = Nip19::from_bech32(bech32.clone()) {
            match nip19 {
                Nip19::Event(n) => {
                    break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: n.event_id,
                        relay_url: n.relays.first().map(UncheckedUrl::new),
                        marker: Some(marker),
                        public_key: None,
                    }));
                }
                Nip19::EventId(id) => {
                    break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: id,
                        relay_url: None,
                        marker: Some(marker),
                        public_key: None,
                    }));
                }
                Nip19::Coordinate(coordinate) => {
                    break Ok(Tag::coordinate(coordinate));
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
                marker: Some(marker),
                public_key: None,
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

#[allow(clippy::too_many_lines)]
pub async fn generate_cover_letter_and_patch_events(
    cover_letter_title_description: Option<(String, String)>,
    git_repo: &Repo,
    commits: &[Sha1Hash],
    signer: &NostrSigner,
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
        ),
        [
            repo_ref.maintainers.iter().map(|m| Tag::coordinate(Coordinate {
                kind: nostr::Kind::GitRepoAnnouncement,
                public_key: *m,
                identifier: repo_ref.identifier.to_string(),
                relays: repo_ref.relays.clone(),
            })).collect::<Vec<Tag>>(),
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
                    event_tag_from_nip19_or_hex(&event_ref,"proposal",Marker::Reply, false, false)?,
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
            if let Ok(branch_name) = git_repo.get_checked_out_branch_name() {
                if !branch_name.eq("main")
                    && !branch_name.eq("master")
                    && !branch_name.eq("origin/main")
                    && !branch_name.eq("origin/master")
                {
                    vec![
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
                            vec![if let Some(branch_name) = branch_name.strip_prefix("pr/") {
                                branch_name.to_string()
                            } else {
                                branch_name
                            }],
                        ),
                    ]
                }
                else { vec![] }
            } else {
                vec![]
            },
            repo_ref.maintainers
                .iter()
                .map(|pk| Tag::public_key(*pk))
                .collect(),
        ].concat(),
    ), signer).await
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
                events.last().map(nostr::Event::id),
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
                            Some(if let Some(branch_name) = branch_name.strip_prefix("pr/") {
                                branch_name.to_string()
                            } else {
                                branch_name
                            })
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
    pub branch_name: String,
    pub event_id: Option<nostr::EventId>,
}

impl CoverLetter {
    pub fn get_branch_name(&self) -> Result<String> {
        Ok(format!(
            "pr/{}({})",
            self.branch_name,
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
        && event.tags().iter().any(|t| t.as_vec()[1].eq("root"))
        && event
            .tags()
            .iter()
            .any(|t| t.as_vec()[1].eq("cover-letter"))
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
    if !event_is_patch_set_root(event) {
        bail!("event is not a patch set root event (root patch or cover letter)")
    }

    let title = commit_msg_from_patch_oneliner(event)?;
    let full = commit_msg_from_patch(event)?;
    let description = full[title.len()..].trim().to_string();

    Ok(CoverLetter {
        title: title.clone(),
        description,
        // TODO should this be prefixed by format!("{}-"e.id.to_string()[..5]?)
        branch_name: if let Ok(name) = match tag_value(event, "branch-name") {
            Ok(name) => {
                if !name.eq("main") && !name.eq("master") {
                    Ok(name)
                } else {
                    Err(())
                }
            }
            _ => Err(()),
        } {
            name
        } else {
            let s = title
                .replace(' ', "-")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c.eq(&'/') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            s
        },
        event_id: Some(event.id()),
    })
}

pub fn get_most_recent_patch_with_ancestors(
    mut patches: Vec<nostr::Event>,
) -> Result<Vec<nostr::Event>> {
    patches.sort_by_key(|e| e.created_at);

    let youngest_patch = patches.last().context("no patches found")?;

    let patches_with_youngest_created_at: Vec<&nostr::Event> = patches
        .iter()
        .filter(|p| p.created_at.eq(&youngest_patch.created_at))
        .collect();

    let mut res = vec![];

    let mut event_id_to_search = patches_with_youngest_created_at
        .clone()
        .iter()
        .find(|p| {
            !patches_with_youngest_created_at.iter().any(|p2| {
                if let Ok(reply_to) = get_event_parent_id(p2) {
                    reply_to.eq(&p.id.to_string())
                } else {
                    false
                }
            })
        })
        .context("cannot find patches_with_youngest_created_at")?
        .id
        .to_string();

    while let Some(event) = patches
        .iter()
        .find(|e| e.id.to_string().eq(&event_id_to_search))
    {
        res.push(event.clone());
        if event_is_patch_set_root(event) {
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
        .find(|t| t.as_vec().len().gt(&3) && t.as_vec()[3].eq("reply"))
    {
        reply_tag
    } else {
        event
            .tags
            .iter()
            .find(|t| t.as_vec().len().gt(&3) && t.as_vec()[3].eq("root"))
            .context("no reply or root e tag present".to_string())?
    }
    .as_vec()[1]
        .clone())
}

pub fn is_event_proposal_root_for_branch(
    e: &Event,
    branch_name_or_refstr: &str,
    logged_in_user: &Option<PublicKey>,
) -> Result<bool> {
    let branch_name = branch_name_or_refstr.replace("refs/heads/", "");
    Ok(event_to_cover_letter(e).is_ok_and(|cl| {
        (logged_in_user.is_some_and(|public_key| e.author().eq(&public_key))
            && (branch_name.eq(&format!("pr/{}", cl.branch_name))
                || cl.branch_name.eq(&branch_name)))
            || cl.get_branch_name().is_ok_and(|s| s.eq(&branch_name))
    }) && !event_is_revision_root(e))
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
                [
                    Tag::hashtag("cover-letter"),
                    Tag::hashtag("root"),
                ],
            )
            .to_event(&nostr::Keys::generate())?)
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
