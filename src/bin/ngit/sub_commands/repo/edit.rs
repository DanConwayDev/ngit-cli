use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use clap::ArgGroup;
use ngit::{
    cli_interactor::{cli_error, cli_error_with_category},
    client::{
        Params, get_event_from_global_cache, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    event_ordering::latest_event,
    repo_ref::{
        LeadSource, MaintainerAcknowledgement, RepoRef, announcement_author_declines_maintainership,
    },
};
use nostr::prelude::{Event, Filter, Kind, PublicKey, ToBech32};

use crate::{
    cli::{Cli, SignerParams},
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::{print_selected_repo, try_resolve_repo_coordinate},
    sub_commands::{init, repository_fetch::prepare_account_for_repo_fetch},
};

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
#[command(group(
    ArgGroup::new("relationship_action")
        .args([
            "add_maintainer",
            "remove_maintainer",
            "acknowledge_maintainer_change",
        ])
        .multiple(false)
))]
pub struct SubCommandArgs {
    #[arg(long, alias = "title")]
    /// name of repository (preferred over --identifier); --title is an alias
    pub(crate) name: Option<String>,
    #[arg(long)]
    /// shortname with no spaces or special characters
    pub(crate) identifier: Option<String>,
    #[arg(long)]
    /// optional description
    pub(crate) description: Option<String>,
    #[arg(short, long, value_parser, num_args = 1..)]
    /// where your git+nostr data is hosted
    pub(crate) grasp_server: Vec<String>,
    #[arg(long, value_parser, num_args = 1..)]
    /// additional relays beyond grasp servers
    pub(crate) relay: Vec<String>,
    #[arg(long)]
    /// additional git server URLs beyond grasp servers
    pub(crate) clone: Vec<String>,
    #[arg(long, value_parser, num_args = 1..)]
    /// homepage
    pub(crate) web: Vec<String>,
    #[arg(short = 'u', long = "u", alias = "upstream", value_parser, num_args = 1..)]
    /// informational NIP-34 subordinate-fork `u` tag fields
    pub(crate) upstream: Vec<String>,
    #[arg(long, value_name = "NPUB")]
    /// invite one maintainer
    pub(crate) add_maintainer: Option<String>,
    #[arg(long, value_name = "NPUB")]
    /// remove one maintainer relationship
    pub(crate) remove_maintainer: Option<String>,
    #[arg(
        long,
        value_name = "NPUB",
        conflicts_with_all = [
            "lead_maintainer",
            "no_lead_maintainer",
            "name",
            "identifier",
            "description",
            "grasp_server",
            "relay",
            "clone",
            "web",
            "upstream",
            "hashtag",
            "earliest_unique_commit",
            "clean",
            "private",
            "public",
        ]
    )]
    /// record one maintainer's signed acceptance or departure boundary
    pub(crate) acknowledge_maintainer_change: Option<String>,
    #[arg(long, value_name = "NPUB", conflicts_with = "no_lead_maintainer")]
    /// assign the lead maintainer
    pub(crate) lead_maintainer: Option<String>,
    #[arg(long, conflicts_with = "lead_maintainer")]
    /// affirm deliberately leadless governance for this add or remove
    pub(crate) no_lead_maintainer: bool,
    #[arg(long, value_parser, num_args = 1..)]
    /// hashtags for repository discovery
    pub(crate) hashtag: Vec<String>,
    #[arg(long)]
    /// usually root commit but will be more recent commit for forks
    pub(crate) earliest_unique_commit: Option<String>,
    #[arg(long)]
    /// drop unknown tags from the existing announcement when republishing
    pub(crate) clean: bool,
    #[arg(long, conflicts_with = "public")]
    /// mark the repository private
    pub(crate) private: bool,
    #[arg(long, conflicts_with = "private")]
    /// remove the private marker
    pub(crate) public: bool,
    #[arg(long)]
    /// reserved for future state-only replacement; collisions still fail
    pub(crate) force: bool,
}

impl SubCommandArgs {
    fn has_relationship_mutation(&self) -> bool {
        self.add_maintainer.is_some() || self.remove_maintainer.is_some()
    }
}

fn parse_pubkey(flag: &str, value: &str) -> Result<PublicKey> {
    PublicKey::parse(value).with_context(|| format!("{flag} '{value}' is not a valid npub"))
}

fn own_announcement(repo_ref: &RepoRef, my_pubkey: PublicKey) -> Result<RepoRef> {
    repo_ref
        .events
        .values()
        .find(|event| event.pubkey == my_pubkey)
        .cloned()
        .map(|event| RepoRef::try_from((event, None)))
        .transpose()
        .context("failed to parse your repository announcement")?
        .ok_or_else(|| {
            cli_error(
                "you have not published an announcement for this repository",
                &[],
                &["if you are invited, run `ngit repo accept` first"],
            )
        })
}

fn announcement_by(repo_ref: &RepoRef, pubkey: PublicKey) -> Option<RepoRef> {
    repo_ref
        .events
        .values()
        .find(|event| event.pubkey == pubkey)
        .cloned()
        .and_then(|event| RepoRef::try_from((event, None)).ok())
}

fn npubs(pubkeys: impl IntoIterator<Item = PublicKey>) -> Vec<String> {
    pubkeys
        .into_iter()
        .map(|pubkey| pubkey.to_bech32().unwrap_or_else(|_| pubkey.to_hex()))
        .collect()
}

fn require_safe_named_removal(
    repo_ref: &RepoRef,
    author: PublicKey,
    target: PublicKey,
    replacement_maintainers: &[PublicKey],
) -> Result<()> {
    let before_maintainers: HashSet<PublicKey> =
        repo_ref.confirmed_maintainers().into_iter().collect();
    let before_moderators: HashSet<PublicKey> =
        repo_ref.confirmed_moderators().into_iter().collect();
    let (after_maintainers, after_moderators) =
        repo_ref.membership_after_author_change(author, replacement_maintainers, true, None);
    let after_maintainers: HashSet<PublicKey> = after_maintainers.into_iter().collect();
    let after_moderators: HashSet<PublicKey> = after_moderators.into_iter().collect();
    let target_npub = target.to_bech32().unwrap_or_else(|_| target.to_hex());

    if before_maintainers.contains(&target) && after_maintainers.contains(&target) {
        let retaining_assigners = npubs(
            repo_ref
                .maintainer_edges()
                .into_iter()
                .filter(|edge| {
                    edge.from != author
                        && edge.to == target
                        && after_maintainers.contains(&edge.from)
                })
                .map(|edge| edge.from),
        );
        let source = if target == repo_ref.selected_maintainer {
            "the currently selected repository coordinate still seeds their authority".to_string()
        } else if retaining_assigners.is_empty() {
            "another active reciprocal path still confirms them".to_string()
        } else {
            format!(
                "they are still assigned by {}",
                retaining_assigners.join(", ")
            )
        };
        return Err(cli_error_with_category(
            "membership_removal_ineffective",
            &format!(
                "removing {target_npub} from your announcement would not remove their maintainer authority"
            ),
            &[("remaining authority", &source)],
            &[
                "ask each retaining co-maintainer to run `ngit repo follow-lead` first",
                "then retry this named removal",
            ],
        ));
    }

    let unexpected_maintainer_removals = npubs(
        before_maintainers
            .difference(&after_maintainers)
            .copied()
            .filter(|pubkey| *pubkey != target),
    );
    let unexpected_moderator_removals =
        npubs(before_moderators.difference(&after_moderators).copied());
    let unexpected_maintainer_additions =
        npubs(after_maintainers.difference(&before_maintainers).copied());
    let unexpected_moderator_additions =
        npubs(after_moderators.difference(&before_moderators).copied());
    if unexpected_maintainer_removals.is_empty()
        && unexpected_moderator_removals.is_empty()
        && unexpected_maintainer_additions.is_empty()
        && unexpected_moderator_additions.is_empty()
    {
        return Ok(());
    }

    let mut details = Vec::new();
    if !unexpected_maintainer_removals.is_empty() {
        details.push((
            "other maintainers removed",
            unexpected_maintainer_removals.join(", "),
        ));
    }
    if !unexpected_moderator_removals.is_empty() {
        details.push((
            "moderators removed",
            unexpected_moderator_removals.join(", "),
        ));
    }
    if !unexpected_maintainer_additions.is_empty() {
        details.push((
            "maintainers added",
            unexpected_maintainer_additions.join(", "),
        ));
    }
    if !unexpected_moderator_additions.is_empty() {
        details.push((
            "moderators added",
            unexpected_moderator_additions.join(", "),
        ));
    }
    let detail_refs: Vec<(&str, &str)> = details
        .iter()
        .map(|(label, value)| (*label, value.as_str()))
        .collect();
    Err(cli_error_with_category(
        "membership_removal_consequences",
        &format!("removing {target_npub} would also change other confirmed repository members"),
        &detail_refs,
        &[
            "establish replacement graph paths for every affected member first",
            "retry only when this named removal has no other membership consequences",
        ],
    ))
}

fn require_prepared_lead(
    current_roster: &[PublicKey],
    proposed_lead: PublicKey,
    proposed_ref: Option<&RepoRef>,
    author: PublicKey,
) -> Result<()> {
    let current: HashSet<PublicKey> = current_roster.iter().copied().collect();
    let proposed: HashSet<PublicKey> = proposed_ref
        .map(|repo_ref| repo_ref.maintainers.iter().copied().collect())
        .unwrap_or_default();
    let retained = HashSet::from([author, proposed_lead]);
    let covered: HashSet<PublicKey> = proposed.union(&retained).copied().collect();
    let removed = npubs(current.difference(&covered).copied());
    if !removed.is_empty() {
        let lead = proposed_lead
            .to_bech32()
            .unwrap_or_else(|_| proposed_lead.to_hex());
        return Err(cli_error(
            &format!(
                "setting {lead} as lead would remove specific maintainers from your active graph: {}",
                removed.join(", ")
            ),
            &[],
            &[
                &format!(
                    "ask {lead} to add these maintainers first: {}",
                    removed.join(", ")
                ),
                "or remove each named maintainer first with `ngit repo edit --remove-maintainer <npub>`",
            ],
        ));
    }
    let extra = npubs(proposed.difference(&current).copied());
    if !extra.is_empty() {
        return Err(cli_error(
            "the proposed lead announcement contains additional active maintainers",
            &[("additional maintainers", &extra.join(", "))],
            &["reconcile the proposed lead's roster before retrying the handover"],
        ));
    }
    let prepared = proposed_ref.is_some_and(|repo_ref| {
        repo_ref.lead == Some(proposed_lead) && repo_ref.maintainers.contains(&proposed_lead)
    });
    if !prepared || proposed != current {
        let lead = proposed_lead
            .to_bech32()
            .unwrap_or_else(|_| proposed_lead.to_hex());
        return Err(cli_error(
            "the proposed lead has not published a complete self-lead roster",
            &[],
            &[
                &format!("ask {lead} to run `ngit repo edit --lead-maintainer {lead}` first"),
                "retry after their announcement lists the complete current roster",
            ],
        ));
    }
    Ok(())
}

async fn latest_maintainer_announcement(
    git_repo_path: &Path,
    identifier: &str,
    pubkey: PublicKey,
) -> Option<Event> {
    let filter = Filter::new()
        .kind(Kind::GitRepoAnnouncement)
        .author(pubkey)
        .identifier(identifier.to_string());
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

fn relationship_governance(
    args: &SubCommandArgs,
    repo_ref: &RepoRef,
    my_pubkey: PublicKey,
) -> Result<Option<PublicKey>> {
    let requested_lead = args
        .lead_maintainer
        .as_deref()
        .map(|value| parse_pubkey("--lead-maintainer", value))
        .transpose()?;
    if !args.has_relationship_mutation() {
        if args.no_lead_maintainer {
            return Err(cli_error(
                "--no-lead-maintainer is only valid with --add-maintainer or --remove-maintainer",
                &[],
                &[],
            ));
        }
        if requested_lead.is_none() {
            return Ok(None);
        }
    }

    let resolution = repo_ref.lead_resolution();
    match resolution.source {
        LeadSource::ImplicitSole => Ok(requested_lead.or(Some(my_pubkey))),
        LeadSource::Explicit => {
            if args.no_lead_maintainer {
                return Err(cli_error(
                    "--no-lead-maintainer cannot remove an existing lead declaration",
                    &[],
                    &["use the existing lead-shaped workflow"],
                ));
            }
            let preparing_to_lead =
                !args.has_relationship_mutation() && requested_lead == Some(my_pubkey);
            if resolution.lead != Some(my_pubkey) && !preparing_to_lead {
                let lead = resolution
                    .lead
                    .and_then(|pk| pk.to_bech32().ok())
                    .unwrap_or_else(|| "the resolved lead".to_string());
                return Err(cli_error(
                    &format!(
                        "only the resolved lead should change this repository's maintainer roster; the lead is {lead}"
                    ),
                    &[],
                    &[&format!("ask {lead} to make this change")],
                ));
            }
            Ok(requested_lead)
        }
        LeadSource::LegacyInferred | LeadSource::ExplicitNone | LeadSource::None => {
            if requested_lead.is_none() && !args.no_lead_maintainer {
                return Err(cli_error(
                    "this multi-maintainer repository has no explicit lead decision",
                    &[],
                    &[
                        "add --lead-maintainer <npub> to make the lead explicit",
                        "or add --no-lead-maintainer for this deliberately leadless change",
                    ],
                ));
            }
            Ok(requested_lead)
        }
        LeadSource::Pending => Err(cli_error_with_category(
            "lead_pending",
            "the repository lead transition is pending",
            &[],
            &["complete or reconcile the lead transition before changing the roster"],
        )),
        LeadSource::Conflict => Err(cli_error_with_category(
            "lead_conflict",
            "the repository has conflicting lead declarations",
            &[],
            &["reconcile the lead declarations before changing the roster"],
        )),
    }
}

#[allow(clippy::too_many_lines)]
pub async fn launch(
    cli: &Cli,
    args: &SubCommandArgs,
    signer_params: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer_params.info,
        signer_params.password,
        Some(&client),
        false,
    )
    .await?;
    let Some(resolved) = try_resolve_repo_coordinate(&git_repo).await? else {
        return Err(cli_error(
            "no nostr repository found",
            &[],
            &["use `ngit init` to publish a new repository"],
        ));
    };
    print_selected_repo(&resolved);
    let mut coordinate = resolved.coordinate;
    let private_discovery =
        prepare_account_for_repo_fetch(&mut client, &mut coordinate, &signer, &user_ref).await;
    ngit::client::fetching_with_private_discovery(
        git_repo_path,
        &client,
        &mut coordinate,
        &private_discovery,
    )
    .await?;
    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &coordinate)
        .await
        .context("no repository announcement found on relays")?;
    let my_pubkey = user_ref.public_key;
    let mut my_ref = own_announcement(&repo_ref, my_pubkey)?;

    let acknowledgement = if let Some(value) = &args.acknowledge_maintainer_change {
        let target = parse_pubkey("--acknowledge-maintainer-change", value)?;
        if target == my_pubkey {
            return Err(cli_error(
                "you cannot acknowledge your own maintainer history",
                &[],
                &[],
            ));
        }
        let target_event =
            latest_maintainer_announcement(git_repo_path, &repo_ref.identifier, target)
                .await
                .ok_or_else(|| {
                    cli_error(
                        "no signed maintainer change was found for that pubkey",
                        &[],
                        &["fetch again after that maintainer publishes their announcement"],
                    )
                })?;
        let departed = announcement_author_declines_maintainership(&target_event);
        if !departed && !repo_ref.confirmed_maintainers().contains(&target) {
            return Err(cli_error(
                "that pubkey has not published a confirmed maintainer acceptance",
                &[],
                &["ask them to run `ngit repo accept` first"],
            ));
        }
        if departed && my_ref.lead == Some(target) {
            return Err(cli_error(
                "the active lead has ended their own role",
                &[],
                &["resolve the lead transition before acknowledging other history"],
            ));
        }
        let acknowledgement = my_ref
            .acknowledge_maintainer_event(&target_event)
            .with_context(|| format!("cannot acknowledge maintainer change for {value}"))?;
        let changed = match acknowledgement {
            MaintainerAcknowledgement::Accepted { changed, .. }
            | MaintainerAcknowledgement::Departed { changed, .. } => changed,
        };
        if !changed {
            if crate::output::is_json() {
                crate::output::set_value(serde_json::json!({
                    "status": "ok",
                    "action": "maintainer_change_already_acknowledged",
                    "pubkey": target.to_string(),
                }));
            }
            println!("that maintainer change is already recorded.");
            return Ok(());
        }
        Some(acknowledgement)
    } else {
        None
    };

    let requested_lead = relationship_governance(args, &repo_ref, my_pubkey)?;
    let resolution = repo_ref.lead_resolution();
    let preparing_to_lead = args.lead_maintainer.is_some()
        && requested_lead == Some(my_pubkey)
        && resolution.source == LeadSource::Explicit
        && resolution.lead != Some(my_pubkey);
    let mut maintainers = if preparing_to_lead {
        if !repo_ref.confirmed_maintainers().contains(&my_pubkey) {
            return Err(cli_error(
                "only a confirmed maintainer can prepare to receive the lead",
                &[],
                &["accept the maintainer invitation first"],
            ));
        }
        let current_lead = resolution.lead.context("the resolved lead is missing")?;
        let lead_ref = announcement_by(&repo_ref, current_lead)
            .context("the resolved lead announcement is missing")?;
        let canonical: HashSet<PublicKey> = lead_ref.maintainers.iter().copied().collect();
        let discovered: HashSet<PublicKey> = repo_ref.maintainers.iter().copied().collect();
        if canonical != discovered {
            return Err(cli_error(
                "the discovered maintainer graph does not match the lead's active roster",
                &[],
                &["run `ngit repo follow-lead` before preparing a handover"],
            ));
        }
        lead_ref.maintainers
    } else {
        my_ref.maintainers.clone()
    };
    if !maintainers.contains(&my_pubkey) {
        maintainers.insert(0, my_pubkey);
    }
    if let Some(value) = &args.add_maintainer {
        let target = parse_pubkey("--add-maintainer", value)?;
        if target == my_pubkey || maintainers.contains(&target) {
            return Err(cli_error_with_category(
                "maintainer_already_listed",
                "that pubkey is already in your active maintainer roster",
                &[],
                &[],
            ));
        }
        let discovered =
            super::preflight::discover_candidate_events(&client, &repo_ref, target).await?;
        if let Some(event) = super::preflight::latest_announcement(
            git_repo_path,
            &repo_ref.identifier,
            target,
            &discovered,
        )
        .await
        {
            let candidate = RepoRef::try_from((event, None))
                .context("failed to parse the invitee's same-identifier announcement")?;
            if candidate.maintainers.contains(&my_pubkey) {
                super::preflight::require_no_joined_component(
                    &candidate,
                    &my_ref.maintainers,
                    my_pubkey,
                    target,
                )?;
                super::preflight::require_equivalent_activating_state(
                    git_repo_path,
                    &repo_ref,
                    target,
                    false,
                    args.force,
                    &discovered,
                )
                .await?;
            }
        }
        maintainers.push(target);
    }
    let removed_target = if let Some(value) = &args.remove_maintainer {
        let target = parse_pubkey("--remove-maintainer", value)?;
        if target == my_pubkey {
            return Err(cli_error(
                "--remove-maintainer cannot remove your own role",
                &[],
                &["use `ngit repo leave` to end your own role"],
            ));
        }
        if !maintainers.contains(&target) {
            return Err(cli_error_with_category(
                "maintainer_not_listed",
                "that pubkey is not in your active maintainer roster",
                &[],
                &[],
            ));
        }
        maintainers.retain(|pubkey| *pubkey != target);
        Some(target)
    } else {
        None
    };

    let mut role_tags = acknowledgement.map(|_| my_ref.role_tags.clone());
    if args.lead_maintainer.is_some() && requested_lead.is_some_and(|lead| lead != my_pubkey) {
        let lead = requested_lead.unwrap();
        let proposed_ref = announcement_by(&repo_ref, lead);
        require_prepared_lead(&maintainers, lead, proposed_ref.as_ref(), my_pubkey)?;
        my_ref.defer_third_party_roles(my_pubkey, lead);
        role_tags = Some(my_ref.role_tags.clone());
    }

    if let Some(target) = removed_target {
        let active_projection = requested_lead
            .filter(|lead| *lead != my_pubkey)
            .map_or_else(|| maintainers.clone(), |lead| vec![my_pubkey, lead]);
        require_safe_named_removal(&repo_ref, my_pubkey, target, &active_projection)?;
    }

    let relationship_action = args.has_relationship_mutation()
        || args.lead_maintainer.is_some()
        || args.no_lead_maintainer
        || acknowledgement.is_some();
    if let Some(lead) = requested_lead {
        if !maintainers.contains(&lead) {
            let lead = lead.to_bech32().unwrap_or_else(|_| lead.to_hex());
            return Err(cli_error(
                &format!("lead maintainer {lead} is not in the resulting roster"),
                &[],
                &[&format!(
                    "add them first with `ngit repo edit --add-maintainer {lead}`"
                )],
            ));
        }
    }

    let internal_args = init::SubCommandArgs {
        name: args.name.clone(),
        identifier: args.identifier.clone(),
        description: args.description.clone(),
        grasp_server: args.grasp_server.clone(),
        relay: args.relay.clone(),
        clone: args.clone.clone(),
        web: args.web.clone(),
        upstream: args.upstream.clone(),
        other_maintainers: maintainers
            .iter()
            .filter(|pubkey| **pubkey != my_pubkey)
            .filter_map(|pubkey| pubkey.to_bech32().ok())
            .collect(),
        lead_maintainer: requested_lead.and_then(|pubkey| pubkey.to_bech32().ok()),
        replace_maintainers: relationship_action,
        clear_lead: args.no_lead_maintainer,
        role_tags,
        preserve_selected_coordinate: true,
        hashtag: args.hashtag.clone(),
        earliest_unique_commit: args.earliest_unique_commit.clone(),
        clean: args.clean,
        private: args.private,
        public: args.public,
    };
    init::launch_repo_edit(cli, &internal_args, signer_params).await
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{EventBuilder, Keys, Tag, event::FinalizeEvent, nip01::Coordinate};

    use super::*;

    fn role_event(keys: &Keys, roles: Vec<Vec<String>>) -> Event {
        let mut tags = vec![Tag::identifier("repo")];
        tags.extend(roles.into_iter().map(|role| Tag::parse(role).unwrap()));
        EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .finalize(keys)
            .unwrap()
    }

    fn insert(repo_ref: &mut RepoRef, event: Event) {
        repo_ref.events.insert(
            nostr::prelude::Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: event.pubkey,
                    identifier: "repo".to_string(),
                },
                relays: vec![],
            },
            event,
        );
    }

    fn tag(letter: &str, pubkey: PublicKey) -> Vec<String> {
        vec![letter.to_string(), pubkey.to_string()]
    }

    #[test]
    fn named_removal_refuses_when_another_co_maintainer_retains_the_target() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let carol_keys = Keys::generate();
        let alice = alice_keys.public_key();
        let bob = bob_keys.public_key();
        let carol = carol_keys.public_key();
        let mut repo_ref = RepoRef::try_from((
            role_event(
                &alice_keys,
                vec![tag("M", alice), tag("m", bob), tag("m", carol)],
            ),
            None,
        ))
        .unwrap();
        insert(
            &mut repo_ref,
            role_event(
                &bob_keys,
                vec![tag("M", alice), tag("m", bob), tag("m", carol)],
            ),
        );
        insert(
            &mut repo_ref,
            role_event(&carol_keys, vec![tag("M", alice), tag("m", carol)]),
        );
        repo_ref.maintainers = vec![alice, bob, carol];

        let error = require_safe_named_removal(&repo_ref, alice, carol, &[alice, bob])
            .unwrap_err()
            .to_string();
        assert!(error.contains(&carol.to_bech32().unwrap()));
        assert!(error.contains("would not remove their maintainer authority"));
    }

    #[test]
    fn named_removal_names_dependent_maintainers_and_moderators() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let dave_keys = Keys::generate();
        let moderator_keys = Keys::generate();
        let alice = alice_keys.public_key();
        let bob = bob_keys.public_key();
        let dave = dave_keys.public_key();
        let moderator = moderator_keys.public_key();
        let mut repo_ref = RepoRef::try_from((
            role_event(&alice_keys, vec![tag("M", alice), tag("m", bob)]),
            None,
        ))
        .unwrap();
        insert(
            &mut repo_ref,
            role_event(
                &bob_keys,
                vec![
                    tag("M", alice),
                    tag("m", bob),
                    tag("m", dave),
                    tag("o", moderator),
                ],
            ),
        );
        insert(
            &mut repo_ref,
            role_event(&dave_keys, vec![tag("M", bob), tag("m", dave)]),
        );
        insert(
            &mut repo_ref,
            role_event(&moderator_keys, vec![tag("M", bob), tag("o", moderator)]),
        );
        repo_ref.maintainers = vec![alice, bob, dave];
        repo_ref.moderators = vec![moderator];

        let error = require_safe_named_removal(&repo_ref, alice, bob, &[alice])
            .unwrap_err()
            .to_string();
        assert!(error.contains("would also change other confirmed repository members"));
    }
}
