pub mod accept;

use anyhow::{Context, Result};
use ngit::{
    client::{Params, fetching_with_report, get_repo_ref_from_cache},
    repo_ref::{RepoRef, extract_npub, is_grasp_server_clone_url},
};
use nostr::{PublicKey, TagStandard, ToBech32, nips::nip19::Nip19Coordinate};

use crate::{
    cli::{Cli, RepoCommands, extract_signer_cli_arguments},
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::try_and_get_repo_coordinates_when_remote_unknown,
    sub_commands::init,
};

pub async fn launch(cli_args: &Cli, repo_command: Option<&RepoCommands>) -> Result<()> {
    match repo_command {
        Some(RepoCommands::Init(args) | RepoCommands::Edit(args)) => {
            init::launch(cli_args, args).await
        }
        Some(RepoCommands::Accept(args)) => accept::launch(cli_args, args).await,
        None => show_info(cli_args).await,
    }
}

// ---------------------------------------------------------------------------
// `ngit repo` (no subcommand) — show repository info
// ---------------------------------------------------------------------------

async fn show_info(cli_args: &Cli) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (_, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        Some(&client),
        false,
    )
    .await?;

    let repo_coordinate = (try_and_get_repo_coordinates_when_remote_unknown(&git_repo).await).ok();

    let Some(repo_coordinate) = repo_coordinate else {
        println!("no nostr repository found");
        println!();
        println!("use `ngit repo init` to publish this repository to nostr");
        return Ok(());
    };

    // Fetch latest data from relays
    fetching_with_report(git_repo_path, &client, &repo_coordinate).await?;

    let Some(repo_ref) =
        (get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await).ok()
    else {
        println!(
            "coordinate found ({}) but no announcement on relays",
            repo_coordinate.identifier
        );
        println!();
        println!("if you created this repository, run `ngit repo init` to publish an announcement");
        println!("if you are a co-maintainer, run `ngit repo accept` to publish your announcement");
        return Ok(());
    };

    print_repo_info(&repo_ref, &user_ref.public_key, &repo_coordinate);
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn print_repo_info(repo_ref: &RepoRef, my_pubkey: &PublicKey, coordinate: &Nip19Coordinate) {
    // --- Basic metadata ---
    println!("Repository:  {}", repo_ref.name);
    if !repo_ref.description.is_empty() {
        println!("Description: {}", repo_ref.description);
    }
    if !repo_ref.web.is_empty() {
        for url in &repo_ref.web {
            println!("Web:         {url}");
        }
    }
    if !repo_ref.hashtags.is_empty() {
        println!("Hashtags:    {}", repo_ref.hashtags.join(", "));
    }
    println!();

    // --- Maintainers ---
    let trusted = &repo_ref.trusted_maintainer;
    let trusted_npub = trusted.to_bech32().unwrap_or_else(|_| trusted.to_hex());
    println!("Trusted maintainer: {}", short_npub(&trusted_npub));

    // Build a map: pubkey → who listed them (for recursive display)
    // We walk the events map to find each maintainer's "lister"
    let co_maintainers: Vec<&PublicKey> = repo_ref
        .maintainers
        .iter()
        .filter(|m| *m != trusted)
        .collect();

    if !co_maintainers.is_empty() {
        // For each co-maintainer, find who listed them by inspecting events
        let mut listed_by: Vec<(String, Option<String>)> = Vec::new();
        for co in &co_maintainers {
            let co_npub = co.to_bech32().unwrap_or_else(|_| co.to_hex());
            // Find which maintainer's event lists this co-maintainer
            let lister = find_lister(repo_ref, co, trusted);
            listed_by.push((co_npub, lister));
        }

        // Print directly-listed co-maintainers first, then indirectly-listed
        let direct: Vec<_> = listed_by
            .iter()
            .filter(|(_, lister)| lister.is_none())
            .collect();
        let indirect: Vec<_> = listed_by
            .iter()
            .filter(|(_, lister)| lister.is_some())
            .collect();

        if !direct.is_empty() {
            let names: Vec<String> = direct.iter().map(|(npub, _)| short_npub(npub)).collect();
            println!("Co-maintainers: {}", names.join(", "));
        }
        for (npub, lister) in &indirect {
            if let Some(lister_npub) = lister {
                println!(
                    "  └─ {} is listed by {}, not directly by the trusted maintainer",
                    short_npub(npub),
                    short_npub(lister_npub)
                );
            }
        }
    }

    // Maintainers without announcements
    if let Some(without) = &repo_ref.maintainers_without_annoucnement {
        if !without.is_empty() {
            let names: Vec<String> = without
                .iter()
                .map(|pk| {
                    let npub = pk.to_bech32().unwrap_or_else(|_| pk.to_hex());
                    short_npub(&npub)
                })
                .collect();
            println!("  (invited, no announcement yet: {})", names.join(", "));
        }
    }

    // --- My status ---
    let my_status = if my_pubkey == trusted {
        let has_announcement = repo_ref
            .events
            .keys()
            .any(|c| c.coordinate.public_key == *my_pubkey);
        if has_announcement {
            "trusted maintainer [announcement published ✓]"
        } else {
            "trusted maintainer [no announcement — run `ngit repo init`]"
        }
    } else if repo_ref.maintainers.contains(my_pubkey) {
        let has_announcement = repo_ref
            .events
            .keys()
            .any(|c| c.coordinate.public_key == *my_pubkey);
        if has_announcement {
            "co-maintainer [announcement published ✓]"
        } else {
            "co-maintainer [no announcement — run `ngit repo accept`]"
        }
    } else {
        "not a maintainer"
    };
    println!("Your status: {my_status}");
    println!();

    // --- Infrastructure (with per-maintainer attribution) ---
    println!("Git servers (union across all maintainers — any maintainer can add a mirror):");
    for server in &repo_ref.git_server {
        let attribution = attribute_server_to_maintainer(repo_ref, server, coordinate);
        println!("  {server}  {attribution}");
    }
    println!();

    println!("Relays (union across all maintainers — any maintainer can add a relay):");
    for relay in &repo_ref.relays {
        let attribution = attribute_relay_to_maintainer(repo_ref, relay.as_str(), coordinate);
        println!("  {relay}  {attribution}");
    }
    println!();

    // --- Maintainer model note ---
    println!("Note: git servers and relays are pooled from all maintainers' announcements.");
    println!(
        "      Name, description, web, and hashtags come from the most recently updated announcement."
    );
    println!("      Each maintainer independently decides who they list as co-maintainers;");
    println!("      if Alice lists Bob and Bob lists Carol, all three are in the maintainer set.");
}

/// Find which maintainer's event lists `target` as a maintainer.
/// Returns `None` if listed directly by the trusted maintainer,
/// or `Some(lister_npub)` if listed by a co-maintainer.
fn find_lister(repo_ref: &RepoRef, target: &PublicKey, trusted: &PublicKey) -> Option<String> {
    use nostr::nips::nip01::Coordinate;
    use nostr_sdk::Kind;

    // Check if the trusted maintainer's event lists this target directly
    let trusted_coord = nostr::nips::nip19::Nip19Coordinate {
        coordinate: Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: *trusted,
            identifier: repo_ref.identifier.clone(),
        },
        relays: vec![],
    };
    if let Some(event) = repo_ref.events.get(&trusted_coord) {
        // Parse the event's maintainers tag
        let listed_in_trusted: Vec<PublicKey> = event
            .tags
            .iter()
            .filter_map(|t| {
                if let Some(TagStandard::PublicKey { public_key, .. }) = t.as_standardized() {
                    Some(*public_key)
                } else {
                    None
                }
            })
            .collect();
        if listed_in_trusted.contains(target) {
            return None; // directly listed by trusted maintainer
        }
    }

    // Otherwise find which co-maintainer lists them
    for (coord, event) in &repo_ref.events {
        if coord.coordinate.public_key == *trusted {
            continue;
        }
        let lister = coord.coordinate.public_key;
        let maintainers_listed: Vec<PublicKey> = event
            .tags
            .iter()
            .filter_map(|t| {
                if let Some(TagStandard::PublicKey { public_key, .. }) = t.as_standardized() {
                    Some(*public_key)
                } else {
                    None
                }
            })
            .collect();
        if maintainers_listed.contains(target) {
            let lister_npub = lister.to_bech32().unwrap_or_else(|_| lister.to_hex());
            return Some(lister_npub);
        }
    }

    None
}

/// Find which maintainer(s) contribute a given git server URL.
fn attribute_server_to_maintainer(
    repo_ref: &RepoRef,
    server_url: &str,
    coordinate: &Nip19Coordinate,
) -> String {
    // For grasp-format URLs, the npub in the path tells us the owner
    if is_grasp_server_clone_url(server_url) {
        if let Ok(npub) = extract_npub(server_url) {
            return format!("[{}]", short_npub(npub));
        }
    }

    // For non-grasp URLs, find which maintainer's event lists it
    let owners = find_server_owners(repo_ref, server_url, coordinate);
    if owners.is_empty() {
        String::new()
    } else {
        format!("[{}]", owners.join(", "))
    }
}

/// Find which maintainer(s) contribute a given relay URL.
fn attribute_relay_to_maintainer(
    repo_ref: &RepoRef,
    relay_url: &str,
    coordinate: &Nip19Coordinate,
) -> String {
    let owners = find_relay_owners(repo_ref, relay_url, coordinate);
    if owners.is_empty() {
        String::new()
    } else {
        format!("[{}]", owners.join(", "))
    }
}

fn find_server_owners(
    repo_ref: &RepoRef,
    server_url: &str,
    _coordinate: &Nip19Coordinate,
) -> Vec<String> {
    let mut owners = Vec::new();
    for (coord, event) in &repo_ref.events {
        if let Ok(event_ref) = RepoRef::try_from((event.clone(), None)) {
            if event_ref
                .git_server
                .iter()
                .any(|s| s.trim_end_matches('/') == server_url.trim_end_matches('/'))
            {
                let npub = coord
                    .coordinate
                    .public_key
                    .to_bech32()
                    .unwrap_or_else(|_| coord.coordinate.public_key.to_hex());
                owners.push(short_npub(&npub));
            }
        }
    }
    owners
}

fn find_relay_owners(
    repo_ref: &RepoRef,
    relay_url: &str,
    _coordinate: &Nip19Coordinate,
) -> Vec<String> {
    let mut owners = Vec::new();
    for (coord, event) in &repo_ref.events {
        if let Ok(event_ref) = RepoRef::try_from((event.clone(), None)) {
            if event_ref
                .relays
                .iter()
                .any(|r| r.as_str().trim_end_matches('/') == relay_url.trim_end_matches('/'))
            {
                let npub = coord
                    .coordinate
                    .public_key
                    .to_bech32()
                    .unwrap_or_else(|_| coord.coordinate.public_key.to_hex());
                owners.push(short_npub(&npub));
            }
        }
    }
    owners
}

/// Shorten an npub for display: show first 8 + "..." + last 4 chars.
fn short_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}...{}", &npub[..12], &npub[npub.len() - 4..])
}
