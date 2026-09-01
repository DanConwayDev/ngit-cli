pub mod accept;
pub mod edit;
pub mod follow_lead;
pub mod leave;
mod preflight;

use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use console::Style;
use ngit::{
    client::{Params, fetching_quietly, get_repo_ref_from_cache, warn_if_invited_as_maintainer},
    login::{existing::load_existing_login, user::get_user_ref_from_cache},
    repo_ref::{
        RepoRef, RoleSource, extract_npub, format_grasp_server_url_as_relay_url,
        is_grasp_server_clone_url, normalize_grasp_server_url,
    },
    utils::get_short_git_server_name,
};
use nostr::prelude::{FromBech32, PublicKey, ToBech32, nip19::Nip19Coordinate};
use serde::Serialize;

use crate::{
    cli::{Cli, RepoCommands, SignerParams},
    client::{Client, Connect},
    git::{Repo, RepoActions},
    repo_ref::{get_nostr_remote_for_resolved_coordinate, try_resolve_repo_coordinate},
    sub_commands::{init, repository_fetch::prepare_account_for_repo_fetch},
};

pub async fn launch(
    cli_args: &Cli,
    repo_command: Option<&RepoCommands>,
    offline: bool,
    json: bool,
    signer: SignerParams<'_>,
) -> Result<()> {
    match repo_command {
        Some(RepoCommands::Init(args)) => init::launch(cli_args, args, signer).await,
        Some(RepoCommands::Edit(args)) => edit::launch(cli_args, args, signer).await,
        Some(RepoCommands::Accept(args)) => accept::launch(args, signer).await,
        Some(RepoCommands::Leave(args)) => leave::launch(args, signer).await,
        Some(RepoCommands::FollowLead(args)) => follow_lead::launch(args, signer).await,
        None => show_info(offline, json, signer).await,
    }
}

// ---------------------------------------------------------------------------
// JSON output types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RepoInfoJson {
    is_nostr_repo: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nostr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordinate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    web: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<Vec<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainers: Option<Vec<String>>,
    selected_maintainer: Option<String>,
    confirmed_maintainers: Option<Vec<String>>,
    invited_maintainers: Option<Vec<String>>,
    lead_maintainer: Option<String>,
    lead_source: Option<String>,
    lead_path: Option<Vec<String>>,
    recommended_coordinate: Option<String>,
    follow_lead_command: Option<String>,
    pending_actions: Option<Vec<PendingActionJson>>,
    health: Option<RepoHealthJson>,
    maintainer_edges: Option<Vec<MaintainerEdgeJson>>,
    moderators: Option<Vec<String>>,
    confirmed_moderators: Option<Vec<String>>,
    members: Option<Vec<MemberJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grasp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relays: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hashtags: Option<Vec<String>>,
}

#[derive(Serialize)]
struct PendingActionJson {
    code: &'static str,
    command: &'static str,
}

#[derive(Serialize)]
struct RepoHealthJson {
    status: &'static str,
    problems: Vec<HealthProblemJson>,
}

#[derive(Serialize)]
struct HealthProblemJson {
    code: &'static str,
    message: &'static str,
}

#[derive(Serialize)]
struct MaintainerEdgeJson {
    from: String,
    to: String,
}

/// One member in `ngit repo --json`: stable string values documented in
/// `docs/architecture/maintainer-model.md`. `role` is `"lead"`,
/// `"co-maintainer"` or `"moderator"`; `status` is `"confirmed"` or
/// `"invited"` (an assigned-but-unacknowledged moderator is invited, like an
/// unaccepted maintainer); `source` is `"role_tag"`, `"maintainers_tag"` or
/// `"implicit"`.
#[derive(Serialize)]
struct MemberJson {
    pubkey: String,
    role: &'static str,
    status: &'static str,
    source: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemberRole {
    Lead,
    CoMaintainer,
    Moderator,
}

impl MemberRole {
    fn label(self) -> &'static str {
        match self {
            MemberRole::Lead => "lead",
            MemberRole::CoMaintainer => "co-maintainer",
            MemberRole::Moderator => "moderator",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemberStatus {
    Confirmed,
    Invited,
}

impl MemberStatus {
    fn label(self) -> &'static str {
        match self {
            MemberStatus::Confirmed => "confirmed",
            MemberStatus::Invited => "invited",
        }
    }
}

fn source_label(source: RoleSource) -> &'static str {
    match source {
        RoleSource::RoleTag => "role_tag",
        RoleSource::MaintainersTag => "maintainers_tag",
        RoleSource::Implicit => "implicit",
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MemberEntry {
    pubkey: PublicKey,
    role: MemberRole,
    status: MemberStatus,
    source: RoleSource,
}

/// One entry per member, purely re-arranging what `RepoRef` already
/// computes: confirmed maintainers, then invited maintainers, then
/// moderators (confirmed before assigned-but-unacknowledged), each group in
/// listing order. The unique wire-asserted lead gets the `lead` role — it
/// may be an invited maintainer, since a freshly designated lead who has not
/// accepted is still the lead. A pubkey holding both a maintainer listing
/// and a moderator assignment appears once, as a maintainer, mirroring
/// `RepoRef::members_for_announcement_tags`.
fn member_entries(repo_ref: &RepoRef) -> Vec<MemberEntry> {
    let lead = repo_ref.lead_maintainer();
    let confirmed_maintainers: HashSet<PublicKey> =
        repo_ref.confirmed_maintainers().into_iter().collect();
    let confirmed_moderators: HashSet<PublicKey> =
        repo_ref.confirmed_moderators().into_iter().collect();
    let maintainer_role = |pk: &PublicKey| {
        if Some(*pk) == lead {
            MemberRole::Lead
        } else {
            MemberRole::CoMaintainer
        }
    };
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for status in [MemberStatus::Confirmed, MemberStatus::Invited] {
        for pk in &repo_ref.maintainers {
            if (confirmed_maintainers.contains(pk) == (status == MemberStatus::Confirmed))
                && seen.insert(*pk)
            {
                entries.push(MemberEntry {
                    pubkey: *pk,
                    role: maintainer_role(pk),
                    status,
                    source: repo_ref.member_role_source(pk),
                });
            }
        }
    }
    for status in [MemberStatus::Confirmed, MemberStatus::Invited] {
        for pk in &repo_ref.moderators {
            if (confirmed_moderators.contains(pk) == (status == MemberStatus::Confirmed))
                && seen.insert(*pk)
            {
                entries.push(MemberEntry {
                    pubkey: *pk,
                    role: MemberRole::Moderator,
                    status,
                    source: repo_ref.member_role_source(pk),
                });
            }
        }
    }
    entries
}

type MaintainerJsonFields = (
    Vec<String>,
    Vec<String>,
    Option<String>,
    Vec<MaintainerEdgeJson>,
);

fn maintainer_json_fields(repo_ref: &RepoRef) -> MaintainerJsonFields {
    let encode = |pk: &PublicKey| pk.to_bech32().unwrap_or_else(|_| pk.to_hex());
    let confirmed = repo_ref
        .confirmed_maintainers()
        .iter()
        .map(&encode)
        .collect();
    let invited = repo_ref.invited_maintainers().iter().map(&encode).collect();
    let lead = repo_ref.lead_maintainer().as_ref().map(&encode);
    let edges = repo_ref
        .maintainer_edges()
        .iter()
        .map(|edge| MaintainerEdgeJson {
            from: encode(&edge.from),
            to: encode(&edge.to),
        })
        .collect();
    (confirmed, invited, lead, edges)
}

/// The moderator lists and per-member `members` entries for
/// `ngit repo --json`, encoded for output.
fn membership_json_fields(repo_ref: &RepoRef) -> (Vec<String>, Vec<String>, Vec<MemberJson>) {
    let encode = |pk: &PublicKey| pk.to_bech32().unwrap_or_else(|_| pk.to_hex());
    let moderators = repo_ref.moderators.iter().map(&encode).collect();
    let confirmed_moderators = repo_ref
        .confirmed_moderators()
        .iter()
        .map(&encode)
        .collect();
    let members = member_entries(repo_ref)
        .iter()
        .map(|entry| MemberJson {
            pubkey: encode(&entry.pubkey),
            role: entry.role.label(),
            status: entry.status.label(),
            source: source_label(entry.source),
        })
        .collect();
    (moderators, confirmed_moderators, members)
}

// ---------------------------------------------------------------------------
// `ngit repo` (no subcommand) — show repository info
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn show_info(offline: bool, json: bool, signer: SignerParams<'_>) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    // Attempt a silent login — don't prompt if not logged in.
    let active_login = load_existing_login(
        &Some(&git_repo),
        signer.info,
        signer.password,
        &None,
        Some(&client),
        true,  // silent
        false, // don't prompt for password
        false, // don't fetch profile updates
    )
    .await;
    let active_login = match active_login {
        Ok(login) => Some(login),
        Err(error) if signer.info.is_some() => return Err(error),
        Err(_) => None,
    };
    let my_pubkey = active_login
        .as_ref()
        .map(|(_, user_ref, _)| user_ref.public_key);

    let Some(resolved_repo) = try_resolve_repo_coordinate(&git_repo).await? else {
        if json {
            crate::output::set(RepoInfoJson {
                is_nostr_repo: false,
                name: None,
                identifier: None,
                description: None,
                nostr_url: None,
                coordinate: None,
                web: None,
                upstream: None,
                maintainers: None,
                selected_maintainer: None,
                confirmed_maintainers: None,
                invited_maintainers: None,
                lead_maintainer: None,
                lead_source: None,
                lead_path: None,
                recommended_coordinate: None,
                follow_lead_command: None,
                pending_actions: None,
                health: None,
                maintainer_edges: None,
                moderators: None,
                confirmed_moderators: None,
                members: None,
                grasp_servers: None,
                git_servers: None,
                relays: None,
                hashtags: None,
            })?;
        } else {
            println!(
                "subcommands: init, edit, accept, leave  (run `ngit repo --help` for details)"
            );
            println!();
            println!("no nostr repository found");
            println!();
            println!("use `ngit repo init` to publish this repository to nostr");
        }
        return Ok(());
    };
    let selected_remote =
        get_nostr_remote_for_resolved_coordinate(&git_repo, &resolved_repo).await?;
    let repo_coordinate = resolved_repo.coordinate;
    // Fetch latest data from relays — suppress the summary line.
    // fetching_quietly writes a blank line to stderr after errors so there
    // is clear separation before the repo info below.
    if !offline {
        let private_discovery = if let Some((signer, user_ref, _)) = active_login.as_ref() {
            prepare_account_for_repo_fetch(
                &git_repo,
                &mut client,
                &repo_coordinate,
                signer,
                user_ref,
            )
            .await
        } else {
            ngit::login::user::PrivateGitRelayDiscovery::Absent
        };
        let _ =
            fetching_quietly(git_repo_path, &client, &repo_coordinate, &private_discovery).await;
    }

    let Some(repo_ref) =
        (get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await).ok()
    else {
        if json {
            // Coordinate found but no announcement yet — still a nostr repo
            let nostr_url = selected_remote.map(|remote| remote.decoded_url.original_string);
            crate::output::set(RepoInfoJson {
                is_nostr_repo: true,
                name: None,
                identifier: Some(repo_coordinate.identifier.clone()),
                description: None,
                nostr_url,
                coordinate: repo_coordinate.to_bech32().ok(),
                web: None,
                upstream: None,
                maintainers: None,
                selected_maintainer: Some(
                    repo_coordinate
                        .public_key
                        .to_bech32()
                        .unwrap_or_else(|_| repo_coordinate.public_key.to_hex()),
                ),
                confirmed_maintainers: None,
                invited_maintainers: None,
                lead_maintainer: None,
                lead_source: None,
                lead_path: None,
                recommended_coordinate: None,
                follow_lead_command: None,
                pending_actions: None,
                health: None,
                maintainer_edges: None,
                moderators: None,
                confirmed_moderators: None,
                members: None,
                grasp_servers: None,
                git_servers: None,
                relays: None,
                hashtags: None,
            })?;
        } else {
            println!(
                "subcommands: init, edit, accept, leave  (run `ngit repo --help` for details)"
            );
            println!();
            println!(
                "coordinate found ({}) but no announcement on relays",
                repo_coordinate.identifier
            );
            println!();
            println!(
                "if you created this repository, run `ngit repo init` to publish an announcement"
            );
            println!(
                "if you are a co-maintainer, run `ngit repo accept` to publish your announcement"
            );
        }
        return Ok(());
    };

    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    if json {
        print_repo_info_json(&repo_ref, &repo_coordinate, &git_repo)?;
    } else {
        println!("subcommands: init, edit, accept, leave  (run `ngit repo --help` for details)");
        println!();
        print_repo_info(
            &repo_ref,
            my_pubkey.as_ref(),
            &repo_coordinate,
            git_repo_path,
        )
        .await;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn print_repo_info_json(
    repo_ref: &RepoRef,
    coordinate: &Nip19Coordinate,
    git_repo: &Repo,
) -> Result<()> {
    let nostr_url = git_repo
        .git_repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().ok().map(std::string::ToString::to_string))
        .filter(|u| u.starts_with("nostr://"));

    let grasp_servers: Vec<String> = repo_ref
        .git_server
        .iter()
        .filter(|s| is_grasp_server_clone_url(s))
        .filter_map(|s| normalize_grasp_server_url(s).ok())
        .collect();

    let git_servers: Vec<String> = repo_ref
        .git_server
        .iter()
        .filter(|s| !is_grasp_server_clone_url(s))
        .cloned()
        .collect();

    let grasp_relay_urls: Vec<String> = repo_ref
        .git_server
        .iter()
        .filter(|s| is_grasp_server_clone_url(s))
        .filter_map(|s| format_grasp_server_url_as_relay_url(s).ok())
        .collect();

    let relays: Vec<String> = repo_ref
        .relays
        .iter()
        .filter(|r| {
            let r_str = r.as_str().trim_end_matches('/');
            !grasp_relay_urls
                .iter()
                .any(|g| g.trim_end_matches('/') == r_str)
        })
        .map(std::string::ToString::to_string)
        .collect();

    let maintainers: Vec<String> = repo_ref
        .maintainers
        .iter()
        .filter_map(|pk| pk.to_bech32().ok())
        .collect();
    let encode = |pk: &PublicKey| pk.to_bech32().unwrap_or_else(|_| pk.to_hex());
    let (confirmed_maintainers, invited_maintainers, lead_maintainer, maintainer_edges) =
        maintainer_json_fields(repo_ref);
    let (moderators, confirmed_moderators, members) = membership_json_fields(repo_ref);
    let resolution = repo_ref.lead_resolution();
    let lead_path = resolution.path.iter().map(&encode).collect();
    let forward_available = resolution.source == ngit::repo_ref::LeadSource::Explicit
        && resolution
            .lead
            .is_some_and(|lead| lead != repo_ref.selected_maintainer);
    let recommended_coordinate = resolution.lead.and_then(|lead| {
        repo_ref
            .events
            .values()
            .find(|event| event.pubkey == lead)
            .cloned()
            .and_then(|event| RepoRef::try_from((event, None)).ok())
            .and_then(|lead_ref| lead_ref.coordinate_with_hint().to_bech32().ok())
    });
    let mut problems = Vec::new();
    match resolution.source {
        ngit::repo_ref::LeadSource::Pending => problems.push(HealthProblemJson {
            code: "lead_pending",
            message: "the selected lead path is incomplete",
        }),
        ngit::repo_ref::LeadSource::Conflict => problems.push(HealthProblemJson {
            code: "lead_conflict",
            message: "the selected lead path is conflicting",
        }),
        _ => {}
    }
    if forward_available {
        problems.push(HealthProblemJson {
            code: "follow_lead_available",
            message: "the selected coordinate forwards to another lead",
        });
    }
    let health_status = if problems
        .iter()
        .any(|problem| matches!(problem.code, "lead_pending" | "lead_conflict"))
    {
        "error"
    } else if problems.is_empty() {
        "ok"
    } else {
        "warning"
    };

    let info = RepoInfoJson {
        is_nostr_repo: true,
        name: Some(repo_ref.name.clone()),
        identifier: Some(repo_ref.identifier.clone()),
        description: if repo_ref.description.is_empty() {
            None
        } else {
            Some(repo_ref.description.clone())
        },
        nostr_url,
        coordinate: coordinate.to_bech32().ok(),
        web: if repo_ref.web.is_empty() {
            None
        } else {
            Some(repo_ref.web.clone())
        },
        upstream: if repo_ref.upstream.is_empty() {
            None
        } else {
            Some(repo_ref.upstream.clone())
        },
        maintainers: Some(maintainers),
        selected_maintainer: Some(encode(&repo_ref.selected_maintainer)),
        confirmed_maintainers: Some(confirmed_maintainers),
        invited_maintainers: Some(invited_maintainers),
        lead_maintainer,
        lead_source: Some(resolution.source.label().to_string()),
        lead_path: Some(lead_path),
        recommended_coordinate,
        follow_lead_command: forward_available.then(|| "ngit repo follow-lead".to_string()),
        pending_actions: Some(if forward_available {
            vec![PendingActionJson {
                code: "follow_lead",
                command: "ngit repo follow-lead",
            }]
        } else {
            Vec::new()
        }),
        health: Some(RepoHealthJson {
            status: health_status,
            problems,
        }),
        maintainer_edges: Some(maintainer_edges),
        moderators: Some(moderators),
        confirmed_moderators: Some(confirmed_moderators),
        members: Some(members),
        grasp_servers: if grasp_servers.is_empty() {
            None
        } else {
            Some(grasp_servers)
        },
        git_servers: if git_servers.is_empty() {
            None
        } else {
            Some(git_servers)
        },
        relays: if relays.is_empty() {
            None
        } else {
            Some(relays)
        },
        hashtags: if repo_ref.hashtags.is_empty() {
            None
        } else {
            Some(repo_ref.hashtags.clone())
        },
    };

    crate::output::set(info)
}

#[allow(clippy::too_many_lines)]
async fn print_repo_info(
    repo_ref: &RepoRef,
    my_pubkey: Option<&PublicKey>,
    coordinate: &Nip19Coordinate,
    git_repo_path: &Path,
) {
    let title = Style::new().bold().yellow();
    let heading = Style::new().bold().dim();
    let dim = Style::new().dim();

    let term_width = console::Term::stdout().size().1 as usize;
    let rule_width = term_width.clamp(20, 60);
    let rule = dim.apply_to("─".repeat(rule_width));

    let multi_maintainer = repo_ref.maintainers.len() > 1
        || repo_ref
            .maintainers_without_annoucnement
            .as_ref()
            .is_some_and(|v| !v.is_empty());

    // --- Basic metadata ---
    println!("{rule}");
    println!(" {}", title.apply_to(&repo_ref.name));

    // Show identifier only when it differs from the name
    let identifier_slug = repo_ref.identifier.to_lowercase().replace(' ', "-");
    let name_slug = repo_ref.name.to_lowercase().replace(' ', "-");
    if identifier_slug != name_slug {
        println!(
            " {}",
            dim.apply_to(format!("identifier: {}", repo_ref.identifier))
        );
    }

    if !repo_ref.description.is_empty() {
        println!(" {}", repo_ref.description);
    }
    if !repo_ref.web.is_empty() {
        for url in &repo_ref.web {
            println!(" {}", dim.apply_to(url));
        }
    }
    if !repo_ref.upstream.is_empty() {
        for upstream in &repo_ref.upstream {
            println!(
                " {}",
                dim.apply_to(format!("upstream: {}", upstream.join(" ")))
            );
        }
    }
    if !repo_ref.hashtags.is_empty() {
        println!(" {}", dim.apply_to(repo_ref.hashtags.join("  ")));
    }
    println!("{rule}");
    println!();

    // --- Maintainers ---
    println!("{}", heading.apply_to("Maintainers"));
    let selected = &repo_ref.selected_maintainer;
    let selected_name = display_name_for(selected, my_pubkey, git_repo_path).await;
    let confirmed = repo_ref.confirmed_maintainers();
    let edges = repo_ref.maintainer_edges();
    let lead = repo_ref.lead_maintainer();
    let members = member_entries(repo_ref);
    // a lone maintainer without a lead assertion needs no role badge; once
    // there is a second member or a lead the roles disambiguate. Counting
    // deduplicated member entries keeps a pubkey holding both a maintainer
    // listing and a moderator assignment from faking a second member.
    let show_role_badges = lead.is_some()
        || members.len() > 1
        || repo_ref
            .maintainers_without_annoucnement
            .as_ref()
            .is_some_and(|v| !v.is_empty());
    for maintainer in &confirmed {
        let name = if maintainer == selected {
            selected_name.clone()
        } else {
            display_name_for(maintainer, my_pubkey, git_repo_path).await
        };
        let mut roles = Vec::new();
        if maintainer == selected && confirmed.len() > 1 {
            roles.push("selected");
        }
        if show_role_badges {
            roles.push(if Some(*maintainer) == lead {
                "lead"
            } else {
                "co-maintainer"
            });
        }
        let role_suffix = (!roles.is_empty()).then(|| format!(" [{}]", roles.join(", ")));
        let listed = related_maintainers(*maintainer, &confirmed, &edges, EdgeDirection::Outgoing);
        let listing = if listed.is_empty() {
            "lists none".to_string()
        } else {
            let mut names = Vec::new();
            for listed_maintainer in listed {
                names.push(display_name_for(&listed_maintainer, my_pubkey, git_repo_path).await);
            }
            format!("lists {}", names.join(", "))
        };
        let annotation = match role_source_note(repo_ref, maintainer) {
            Some(note) => format!("· {listing} · {note}"),
            None => format!("· {listing}"),
        };
        println!(
            "  {name}{} {}",
            role_suffix.unwrap_or_default(),
            dim.apply_to(annotation)
        );
    }

    let invited = repo_ref.invited_maintainers();
    if !invited.is_empty() {
        println!("  {}", dim.apply_to("Invited maintainers"));
        for pk in invited {
            let name = display_name_for(&pk, my_pubkey, git_repo_path).await;
            // a freshly designated lead who has not accepted is still the lead
            let role_suffix = (Some(pk) == lead).then_some(" [lead]");
            let inviters = related_maintainers(pk, &confirmed, &edges, EdgeDirection::Incoming);
            let mut notes = Vec::new();
            if !inviters.is_empty() && inviters.len() < confirmed.len() {
                let mut inviter_names = Vec::new();
                for inviter in inviters {
                    inviter_names.push(display_name_for(&inviter, my_pubkey, git_repo_path).await);
                }
                notes.push(format!("invited by {}", inviter_names.join(", ")));
            }
            if let Some(note) = role_source_note(repo_ref, &pk) {
                notes.push(note.to_string());
            }
            if notes.is_empty() {
                println!("  {name}{}", role_suffix.unwrap_or_default());
            } else {
                println!(
                    "  {name}{} {}",
                    role_suffix.unwrap_or_default(),
                    dim.apply_to(format!("· {}", notes.join(" · ")))
                );
            }
        }
        println!(
            "  {}",
            dim.apply_to("invited maintainers have no authority until they accept")
        );
    }
    println!();

    // --- Moderators ---
    // derived from member_entries so this section agrees with the --json
    // members field: a pubkey also holding a maintainer listing already
    // appeared above as a maintainer and is not repeated here
    let moderator_members: Vec<&MemberEntry> = members
        .iter()
        .filter(|entry| entry.role == MemberRole::Moderator)
        .collect();
    if !moderator_members.is_empty() {
        println!("{}", heading.apply_to("Moderators"));
        for entry in moderator_members {
            let name = display_name_for(&entry.pubkey, my_pubkey, git_repo_path).await;
            if entry.status == MemberStatus::Confirmed {
                println!("  {name} [moderator]");
            } else {
                println!(
                    "  {name} [moderator] {}",
                    dim.apply_to("· assigned, not yet acknowledged")
                );
            }
        }
        println!(
            "  {}",
            dim.apply_to("moderators can manage issues and PRs but never publish repository state")
        );
        println!();
    }

    // --- Infrastructure ---
    // Split into three groups:
    //   1. Grasp servers (each bundles a git server + relay)
    //   2. Additional git servers (non-grasp)
    //   3. Additional relays (not covered by a grasp server)

    // Relay URLs that grasp servers already cover (for deduplication)
    let grasp_relay_urls: Vec<String> = repo_ref
        .git_server
        .iter()
        .filter(|s| is_grasp_server_clone_url(s))
        .filter_map(|s| format_grasp_server_url_as_relay_url(s).ok())
        .collect();

    let grasp_servers: Vec<&String> = repo_ref
        .git_server
        .iter()
        .filter(|s| is_grasp_server_clone_url(s))
        .collect();

    let extra_git_servers: Vec<&String> = repo_ref
        .git_server
        .iter()
        .filter(|s| !is_grasp_server_clone_url(s))
        .collect();

    let extra_relays: Vec<_> = repo_ref
        .relays
        .iter()
        .filter(|r| {
            let r_str = r.as_str().trim_end_matches('/');
            !grasp_relay_urls
                .iter()
                .any(|g| g.trim_end_matches('/') == r_str)
        })
        .collect();

    if !grasp_servers.is_empty() {
        println!("{}", heading.apply_to("Grasp servers"));
        for server in &grasp_servers {
            // Display just the domain (strip scheme, npub path, and repo path)
            let short = normalize_grasp_server_url(server)
                .unwrap_or_else(|_| get_short_git_server_name(server));

            if multi_maintainer {
                // Owner is encoded in the URL path (the npub)
                let owner_label = if let Ok(npub) = extract_npub(server) {
                    if let Ok(pk) = PublicKey::from_bech32(npub) {
                        let name = display_name_for(&pk, my_pubkey, git_repo_path).await;
                        format!("[{name}]")
                    } else {
                        format!("[{}]", short_npub(npub))
                    }
                } else {
                    String::new()
                };
                if owner_label.is_empty() {
                    println!("  {short}");
                } else {
                    println!("  {short}  {}", dim.apply_to(&owner_label));
                }
            } else {
                println!("  {short}");
            }
        }
        println!();
    }

    if !extra_git_servers.is_empty() {
        println!("{}", heading.apply_to("Additional git servers"));
        for server in &extra_git_servers {
            let short = get_short_git_server_name(server);
            if multi_maintainer {
                let owners =
                    find_server_owners(repo_ref, server, coordinate, my_pubkey, git_repo_path)
                        .await;
                if owners.is_empty() {
                    println!("  {short}");
                } else {
                    println!(
                        "  {short}  {}",
                        dim.apply_to(format!("[{}]", owners.join(", ")))
                    );
                }
            } else {
                println!("  {short}");
            }
        }
        println!();
    }

    if !extra_relays.is_empty() {
        println!("{}", heading.apply_to("Additional relays"));
        for relay in &extra_relays {
            // Strip the wss:// / ws:// prefix for display
            let display = relay
                .as_str()
                .trim_start_matches("wss://")
                .trim_start_matches("ws://")
                .trim_end_matches('/');
            if multi_maintainer {
                let owners = find_relay_owners(
                    repo_ref,
                    relay.as_str(),
                    coordinate,
                    my_pubkey,
                    git_repo_path,
                )
                .await;
                if owners.is_empty() {
                    println!("  {display}");
                } else {
                    println!(
                        "  {display}  {}",
                        dim.apply_to(format!("[{}]", owners.join(", ")))
                    );
                }
            } else {
                println!("  {display}");
            }
        }
        println!();
    }

    if !repo_ref.root_commit.is_empty() {
        println!(
            "{}",
            dim.apply_to(format!(
                "earliest unique commit: {}",
                &repo_ref.root_commit[..7.min(repo_ref.root_commit.len())]
            ))
        );
        println!();
    }

    // --- Maintainer model note (only relevant when there are multiple maintainers)
    // ---
    if multi_maintainer {
        println!(
            "{}",
            dim.apply_to(
                "Note: git servers and relays are pooled from all maintainers' announcements.\n\
                 Name, description, web, upstream, and hashtags come from the most recently updated announcement.\n\
                 Reciprocal links confirm co-maintainership; only confirmed maintainers' state and status events are authoritative.\n\
                 Invited maintainers gain authority by accepting; a unique lead coordinates but has no extra rights."
            )
        );
    }
    let resolution = repo_ref.lead_resolution();
    if resolution.source == ngit::repo_ref::LeadSource::Explicit
        && resolution
            .lead
            .is_some_and(|lead| lead != repo_ref.selected_maintainer)
    {
        eprintln!("warning: this checkout's selected coordinate forwards to the resolved lead");
        eprintln!("switch to the lead with: ngit repo follow-lead");
    }
}

/// A provenance note for listings that predate NIP-34 indexed role tags.
/// `None` for role-tag listings — the new normal needs no callout.
fn role_source_note(repo_ref: &RepoRef, pk: &PublicKey) -> Option<&'static str> {
    match repo_ref.member_role_source(pk) {
        RoleSource::RoleTag => None,
        RoleSource::MaintainersTag => Some("listed via deprecated maintainers tag"),
        RoleSource::Implicit => Some("implicit listing (no role tag)"),
    }
}

/// Resolve a display name for a public key from the local metadata cache.
/// Appends " (you)" when `pk` matches `my_pubkey`.
/// Falls back to a short npub if no metadata is cached.
async fn display_name_for(
    pk: &PublicKey,
    my_pubkey: Option<&PublicKey>,
    git_repo_path: &Path,
) -> String {
    let name = if let Ok(user_ref) = get_user_ref_from_cache(Some(git_repo_path), pk).await {
        user_ref.metadata.name
    } else {
        let npub = pk.to_bech32().unwrap_or_else(|_| pk.to_hex());
        short_npub(&npub)
    };
    if my_pubkey == Some(pk) {
        format!("{name} (you)")
    } else {
        name
    }
}

#[derive(Clone, Copy)]
enum EdgeDirection {
    Incoming,
    Outgoing,
}

fn related_maintainers(
    maintainer: PublicKey,
    ordered_candidates: &[PublicKey],
    edges: &[ngit::repo_ref::MaintainerEdge],
    direction: EdgeDirection,
) -> Vec<PublicKey> {
    ordered_candidates
        .iter()
        .copied()
        .filter(|candidate| {
            edges.iter().any(|edge| match direction {
                EdgeDirection::Incoming => edge.from == *candidate && edge.to == maintainer,
                EdgeDirection::Outgoing => edge.from == maintainer && edge.to == *candidate,
            })
        })
        .collect()
}

async fn find_server_owners(
    repo_ref: &RepoRef,
    server_url: &str,
    _coordinate: &Nip19Coordinate,
    my_pubkey: Option<&PublicKey>,
    git_repo_path: &Path,
) -> Vec<String> {
    let mut owners = Vec::new();
    for (coord, event) in &repo_ref.events {
        if let Ok(event_ref) = RepoRef::try_from((event.clone(), None)) {
            if event_ref
                .git_server
                .iter()
                .any(|s| s.trim_end_matches('/') == server_url.trim_end_matches('/'))
            {
                let pk = coord.coordinate.public_key;
                owners.push(display_name_for(&pk, my_pubkey, git_repo_path).await);
            }
        }
    }
    owners
}

async fn find_relay_owners(
    repo_ref: &RepoRef,
    relay_url: &str,
    _coordinate: &Nip19Coordinate,
    my_pubkey: Option<&PublicKey>,
    git_repo_path: &Path,
) -> Vec<String> {
    let mut owners = Vec::new();
    for (coord, event) in &repo_ref.events {
        if let Ok(event_ref) = RepoRef::try_from((event.clone(), None)) {
            if event_ref
                .relays
                .iter()
                .any(|r| r.as_str().trim_end_matches('/') == relay_url.trim_end_matches('/'))
            {
                let pk = coord.coordinate.public_key;
                owners.push(display_name_for(&pk, my_pubkey, git_repo_path).await);
            }
        }
    }
    owners
}

/// Shorten an npub for display: show first 12 + "..." + last 4 chars.
fn short_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}...{}", &npub[..12], &npub[npub.len() - 4..])
}

#[cfg(test)]
mod tests {
    use ngit::repo_ref::MaintainerEdge;
    use nostr::prelude::{Coordinate, Event, EventBuilder, Keys, Kind, Tag, event::FinalizeEvent};

    use super::*;

    fn tag(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    fn announcement(keys: &Keys, tags: Vec<Vec<String>>) -> Event {
        let mut event_tags = vec![Tag::identifier("test-repo")];
        for t in tags {
            event_tags.push(Tag::parse(t).unwrap());
        }
        EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(event_tags)
            .finalize(keys)
            .unwrap()
    }

    /// Consolidate announcements the way `get_repo_ref_from_cache` does for
    /// the pieces `member_entries` reads: the first event's author is the
    /// selected maintainer, maintainer listings are unioned, further events
    /// join the events map and moderators are the confirmed group's active
    /// `o` assignments.
    fn consolidated(events: Vec<Event>) -> RepoRef {
        let mut iter = events.into_iter();
        let mut repo_ref = RepoRef::try_from((iter.next().unwrap(), None)).unwrap();
        for event in iter {
            let parsed = RepoRef::try_from((event.clone(), None)).unwrap();
            for pk in parsed.maintainers {
                if !repo_ref.maintainers.contains(&pk) {
                    repo_ref.maintainers.push(pk);
                }
            }
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: Kind::GitRepoAnnouncement,
                        public_key: event.pubkey,
                        identifier: "test-repo".to_string(),
                    },
                    relays: vec![],
                },
                event,
            );
        }
        repo_ref.moderators = repo_ref.assigned_moderators();
        repo_ref
    }

    #[test]
    fn member_entries_report_role_status_and_source_per_member() {
        let owner_keys = Keys::generate();
        let owner = owner_keys.public_key();
        let co_keys = Keys::generate();
        let co = co_keys.public_key();
        let invited = Keys::generate().public_key();
        let moderator_keys = Keys::generate();
        let moderator = moderator_keys.public_key();
        let assigned = Keys::generate().public_key();

        let repo_ref = consolidated(vec![
            announcement(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["m", &co.to_string()]),
                    tag(&["m", &invited.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                    tag(&["o", &assigned.to_string()]),
                ],
            ),
            // reciprocal acceptance confirms the co-maintainer
            announcement(
                &co_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["m", &co.to_string()]),
                ],
            ),
            // the moderator acknowledges the role and a confirmed member
            announcement(
                &moderator_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                ],
            ),
        ]);

        assert_eq!(
            member_entries(&repo_ref),
            vec![
                MemberEntry {
                    pubkey: owner,
                    role: MemberRole::Lead,
                    status: MemberStatus::Confirmed,
                    source: RoleSource::RoleTag,
                },
                MemberEntry {
                    pubkey: co,
                    role: MemberRole::CoMaintainer,
                    status: MemberStatus::Confirmed,
                    source: RoleSource::RoleTag,
                },
                MemberEntry {
                    pubkey: invited,
                    role: MemberRole::CoMaintainer,
                    status: MemberStatus::Invited,
                    source: RoleSource::RoleTag,
                },
                MemberEntry {
                    pubkey: moderator,
                    role: MemberRole::Moderator,
                    status: MemberStatus::Confirmed,
                    source: RoleSource::RoleTag,
                },
                MemberEntry {
                    pubkey: assigned,
                    role: MemberRole::Moderator,
                    status: MemberStatus::Invited,
                    source: RoleSource::RoleTag,
                },
            ]
        );
    }

    #[test]
    fn member_entries_report_the_deprecated_fallback_without_a_lead() {
        let owner_keys = Keys::generate();
        let owner = owner_keys.public_key();
        let other_keys = Keys::generate();
        let other = other_keys.public_key();

        let listing = vec![tag(&[
            "maintainers",
            &owner.to_string(),
            &other.to_string(),
        ])];
        let repo_ref = consolidated(vec![
            announcement(&owner_keys, listing.clone()),
            announcement(&other_keys, listing),
        ]);

        assert_eq!(
            member_entries(&repo_ref),
            vec![
                MemberEntry {
                    pubkey: owner,
                    role: MemberRole::CoMaintainer,
                    status: MemberStatus::Confirmed,
                    source: RoleSource::MaintainersTag,
                },
                MemberEntry {
                    pubkey: other,
                    role: MemberRole::CoMaintainer,
                    status: MemberStatus::Confirmed,
                    source: RoleSource::MaintainersTag,
                },
            ]
        );
    }

    #[test]
    fn member_entries_list_a_dual_role_pubkey_once_as_a_maintainer() {
        let owner_keys = Keys::generate();
        let owner = owner_keys.public_key();
        let dual = Keys::generate().public_key();

        let repo_ref = consolidated(vec![announcement(
            &owner_keys,
            vec![
                tag(&["M", &owner.to_string()]),
                tag(&["m", &dual.to_string()]),
                tag(&["o", &dual.to_string()]),
            ],
        )]);

        let entries = member_entries(&repo_ref);
        assert_eq!(
            entries.iter().filter(|e| e.pubkey == dual).count(),
            1,
            "a pubkey listed as maintainer and moderator must appear once"
        );
        assert_eq!(
            entries
                .iter()
                .find(|e| e.pubkey == dual)
                .map(|e| (e.role, e.status)),
            Some((MemberRole::CoMaintainer, MemberStatus::Invited)),
        );
    }

    #[test]
    fn related_maintainers_preserves_display_order_in_both_directions() {
        let alice = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let ordered = vec![alice, bob, carol];
        let edges = vec![
            MaintainerEdge {
                from: alice,
                to: carol,
            },
            MaintainerEdge {
                from: bob,
                to: carol,
            },
            MaintainerEdge {
                from: alice,
                to: bob,
            },
        ];

        assert_eq!(
            related_maintainers(alice, &ordered, &edges, EdgeDirection::Outgoing),
            vec![bob, carol]
        );
        assert_eq!(
            related_maintainers(carol, &ordered, &edges, EdgeDirection::Incoming),
            vec![alice, bob]
        );
    }
}
