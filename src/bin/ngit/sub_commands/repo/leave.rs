use std::path::Path;

use anyhow::{Context, Result};
use console::Style;
use ngit::{
    cli_interactor::cli_error,
    client::{
        Params, get_event_from_global_cache, get_events_from_local_cache, get_repo_ref_from_cache,
        send_events,
    },
    event_ordering::latest_event,
    repo_ref::RepoRef,
};
use nostr::prelude::{Event, Filter, Kind, PublicKey, Timestamp, ToBech32};

use crate::{
    cli::SignerParams,
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::{print_selected_repo, try_resolve_repo_coordinate},
    sub_commands::repository_fetch::prepare_account_for_repo_fetch,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {}

/// Why there is nothing to leave.
#[derive(Debug, PartialEq, Eq)]
enum LeaveRefusal {
    /// invited as a maintainer but never accepted; an unaccepted
    /// invitation grants no role
    UnacceptedInvitation,
    /// assigned a moderator role by another member but never acknowledged
    /// it with an own announcement
    UnacknowledgedModeratorAssignment,
    /// not referenced by the repository at all
    NotAMember,
    /// their own announcement records every self-role as ended
    RoleAlreadyEnded,
}

/// Membership is declared by your own announcement (`my_ref`); leaving
/// means ending the self-role it records. The consolidated `repo_ref`
/// supplies the assignment context distinguishing an unaccepted invitation
/// or unacknowledged moderator assignment from a stranger. `None` means
/// leaving can proceed, which requires `my_ref` to exist.
fn leave_refusal(
    repo_ref: &RepoRef,
    my_ref: Option<&RepoRef>,
    my_pubkey: &PublicKey,
) -> Option<LeaveRefusal> {
    let Some(my_ref) = my_ref else {
        return Some(if repo_ref.maintainers.contains(my_pubkey) {
            LeaveRefusal::UnacceptedInvitation
        } else if repo_ref.moderators.contains(my_pubkey) {
            LeaveRefusal::UnacknowledgedModeratorAssignment
        } else {
            LeaveRefusal::NotAMember
        });
    };
    if !my_ref.maintainers.contains(my_pubkey) && !my_ref.moderators.contains(my_pubkey) {
        return Some(LeaveRefusal::RoleAlreadyEnded);
    }
    None
}

fn refusal_error(refusal: &LeaveRefusal) -> anyhow::Error {
    match refusal {
        LeaveRefusal::UnacceptedInvitation => cli_error(
            "you have no announcement for this repository",
            &[],
            &[
                "you have been invited but never accepted with `ngit repo accept`; an unaccepted invitation grants no role, so there is nothing to leave",
            ],
        ),
        LeaveRefusal::UnacknowledgedModeratorAssignment => cli_error(
            "you have no announcement for this repository",
            &[],
            &[
                "another member assigns you a moderator role but you never acknowledged it; an unacknowledged assignment grants no role, so there is nothing to leave",
            ],
        ),
        LeaveRefusal::NotAMember => cli_error(
            "you have no announcement for this repository",
            &[],
            &["you are not a member of this repository"],
        ),
        LeaveRefusal::RoleAlreadyEnded => cli_error(
            "you are not a member of this repository",
            &[],
            &["your announcement already records your role as ended"],
        ),
    }
}

/// The latest announcement `my_pubkey` published for this repository. The
/// consolidated `repo_ref.events` carries only current members'
/// announcements, so a member who already left — their announcement
/// declines every role — is looked up in the event caches, which the
/// repository fetch has just mirrored relay results into. This
/// distinguishes "already left" from "never a member".
async fn my_announcement_event(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    my_pubkey: &PublicKey,
) -> Option<Event> {
    if let Some(event) = repo_ref
        .events
        .values()
        .find(|event| event.pubkey == *my_pubkey)
    {
        return Some(event.clone());
    }
    let filter = Filter::default()
        .kind(Kind::GitRepoAnnouncement)
        .author(*my_pubkey)
        .identifier(repo_ref.identifier.clone());
    let mut candidates = get_event_from_global_cache(Some(git_repo_path), vec![filter.clone()])
        .await
        .unwrap_or_default();
    candidates.extend(
        get_events_from_local_cache(git_repo_path, vec![filter])
            .await
            .unwrap_or_default(),
    );
    latest_event(&candidates).cloned()
}

pub async fn launch(_args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer.info,
        signer.password,
        Some(&client),
        false,
    )
    .await?;

    let my_pubkey = user_ref.public_key;

    let Some(resolved_repo_coordinate) = try_resolve_repo_coordinate(&git_repo).await? else {
        return Err(cli_error(
            "no nostr repository found",
            &[],
            &["there is no nostr repository here to leave"],
        ));
    };
    print_selected_repo(&resolved_repo_coordinate);
    let mut repo_coordinate = resolved_repo_coordinate.coordinate;

    // Fetch latest data from relays
    let private_discovery =
        prepare_account_for_repo_fetch(&mut client, &mut repo_coordinate, &signer, &user_ref).await;
    ngit::client::fetching_with_private_discovery(
        git_repo_path,
        &client,
        &mut repo_coordinate,
        &private_discovery,
    )
    .await?;

    let Some(repo_ref) =
        (get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await).ok()
    else {
        return Err(cli_error(
            "no announcement found on relays for this repository",
            &[],
            &["if this is a relay or network issue, try again later"],
        ));
    };

    let my_event = my_announcement_event(git_repo_path, &repo_ref, &my_pubkey).await;
    let my_ref = my_event
        .map(|event| RepoRef::try_from((event, None)))
        .transpose()
        .context("failed to parse your existing announcement")?;
    if let Some(refusal) = leave_refusal(&repo_ref, my_ref.as_ref(), &my_pubkey) {
        return Err(refusal_error(&refusal));
    }
    let Some(mut my_ref) = my_ref else {
        // unreachable: leave_refusal reports every no-announcement case
        return Err(refusal_error(&LeaveRefusal::NotAMember));
    };

    // Leaving as the lead is allowed but may leave the repository leadless:
    // co-maintainers under a lead SHOULD list only themselves and the lead,
    // so nobody else's announcement may assert a replacement yet.
    if repo_ref.lead_maintainer() == Some(my_pubkey) {
        let warn_style = Style::new().yellow();
        eprintln!(
            "{}",
            warn_style.apply_to(
                "warning: you are the lead maintainer; leaving may leave the repository without a lead"
            ),
        );
    }

    let repo_name = my_ref.name.clone();
    println!("leaving '{repo_name}'");

    let ended = my_ref.end_self_role(&my_pubkey, Timestamp::now().as_secs());
    // defensive: the membership checks above guarantee an active self-role
    if !ended {
        return Err(refusal_error(&LeaveRefusal::RoleAlreadyEnded));
    }

    // Order the republished announcement after every announcement seen for
    // the coordinate, like `ngit init` does.
    my_ref.events = repo_ref.events.clone();

    println!("publishing your updated announcement to nostr...");
    let repo_event = my_ref.to_event(&signer).await?;

    client.set_signer(signer.clone()).await;
    if repo_ref.private {
        client.nip42_register_private_repo_relays(repo_ref.relays.clone());
    }

    // Publish to the union of members' relays, not just my own: other
    // members and consumers must observe the ended self-role, which per
    // NIP-34 takes precedence over their assignments.
    let _ = send_events(
        &client,
        Some(git_repo_path),
        vec![repo_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await
    .context("failed to publish the announcement ending your role")?;

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "action": "left",
            "entity": "repository",
            "name": repo_name,
            "coordinate": repo_coordinate.to_bech32()?,
        }));
    }
    println!("membership ended. your announcement now records your role as ended.");
    println!(
        "other members' announcements may still list you; per NIP-34 your own record takes precedence."
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Tag, event::FinalizeEvent};

    use super::*;

    fn tag(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    fn role_event(keys: &Keys, tags: Vec<Vec<String>>) -> Event {
        let mut event_tags = vec![Tag::identifier("test-repo")];
        for t in tags {
            event_tags.push(Tag::parse(t).unwrap());
        }
        EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(event_tags)
            .finalize(keys)
            .unwrap()
    }

    /// [`leave_refusal`]: selects the refusal from my own announcement when
    /// one exists, otherwise from the consolidated assignment context.
    mod leave_refusal {
        use super::*;

        #[test]
        fn maintainer_invitation_without_announcement_is_unaccepted() {
            let owner_keys = Keys::generate();
            let me = Keys::generate().public_key();
            let repo_ref = RepoRef::try_from((
                role_event(
                    &owner_keys,
                    vec![
                        tag(&["m", &owner_keys.public_key().to_string()]),
                        tag(&["m", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            assert_eq!(
                leave_refusal(&repo_ref, None, &me),
                Some(LeaveRefusal::UnacceptedInvitation),
            );
        }

        #[test]
        fn moderator_assignment_without_announcement_is_unacknowledged() {
            let owner_keys = Keys::generate();
            let me = Keys::generate().public_key();
            let repo_ref = RepoRef::try_from((
                role_event(
                    &owner_keys,
                    vec![
                        tag(&["M", &owner_keys.public_key().to_string()]),
                        tag(&["o", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            assert_eq!(
                leave_refusal(&repo_ref, None, &me),
                Some(LeaveRefusal::UnacknowledgedModeratorAssignment),
            );
        }

        #[test]
        fn stranger_without_announcement_is_not_a_member() {
            let owner_keys = Keys::generate();
            let me = Keys::generate().public_key();
            let repo_ref = RepoRef::try_from((
                role_event(
                    &owner_keys,
                    vec![tag(&["m", &owner_keys.public_key().to_string()])],
                ),
                None,
            ))
            .unwrap();
            assert_eq!(
                leave_refusal(&repo_ref, None, &me),
                Some(LeaveRefusal::NotAMember),
            );
        }

        #[test]
        fn announcement_with_only_ended_self_entries_already_left() {
            // the consolidated view still lists me (the owner's assignment
            // is active) but my own announcement records the leave, which
            // takes precedence
            let owner_keys = Keys::generate();
            let owner = owner_keys.public_key();
            let my_keys = Keys::generate();
            let me = my_keys.public_key();
            let repo_ref = RepoRef::try_from((
                role_event(
                    &owner_keys,
                    vec![
                        tag(&["m", &owner.to_string()]),
                        tag(&["m", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            let my_ref = RepoRef::try_from((
                role_event(
                    &my_keys,
                    vec![
                        tag(&["m", &owner.to_string()]),
                        tag(&["m", &me.to_string(), "0", "100"]),
                    ],
                ),
                None,
            ))
            .unwrap();
            assert_eq!(
                leave_refusal(&repo_ref, Some(&my_ref), &me),
                Some(LeaveRefusal::RoleAlreadyEnded),
            );
        }

        #[test]
        fn active_self_role_yields_no_refusal() {
            let owner_keys = Keys::generate();
            let owner = owner_keys.public_key();
            let my_keys = Keys::generate();
            let me = my_keys.public_key();
            let repo_ref = RepoRef::try_from((
                role_event(
                    &owner_keys,
                    vec![
                        tag(&["m", &owner.to_string()]),
                        tag(&["m", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            let my_ref = RepoRef::try_from((
                role_event(
                    &my_keys,
                    vec![
                        tag(&["m", &owner.to_string()]),
                        tag(&["m", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            assert_eq!(leave_refusal(&repo_ref, Some(&my_ref), &me), None);
        }

        #[test]
        fn acknowledged_moderatorship_yields_no_refusal() {
            let owner_keys = Keys::generate();
            let owner = owner_keys.public_key();
            let my_keys = Keys::generate();
            let me = my_keys.public_key();
            let repo_ref = RepoRef::try_from((
                role_event(
                    &owner_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            let my_ref = RepoRef::try_from((
                role_event(
                    &my_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &me.to_string()]),
                    ],
                ),
                None,
            ))
            .unwrap();
            assert_eq!(leave_refusal(&repo_ref, Some(&my_ref), &me), None);
        }
    }
}
