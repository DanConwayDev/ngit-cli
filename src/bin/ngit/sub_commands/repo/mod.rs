pub mod accept;

use std::path::Path;

use anyhow::{Context, Result};
use console::Style;
use ngit::{
    client::{Params, fetching_quietly, get_repo_ref_from_cache},
    login::{existing::load_existing_login, user::get_user_ref_from_cache},
    repo_ref::{
        RepoRef, extract_npub, format_grasp_server_url_as_relay_url, is_grasp_server_clone_url,
        normalize_grasp_server_url,
    },
    utils::get_short_git_server_name,
};
use nostr::{FromBech32, PublicKey, TagStandard, ToBech32, nips::nip19::Nip19Coordinate};

use crate::{
    cli::{Cli, RepoCommands, extract_signer_cli_arguments},
    client::{Client, Connect},
    git::{Repo, RepoActions},
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

    // Attempt a silent login — don't prompt if not logged in.
    let my_pubkey: Option<PublicKey> = load_existing_login(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        &None,
        Some(&client),
        true,  // silent
        false, // don't prompt for password
        false, // don't fetch profile updates
    )
    .await
    .ok()
    .map(|(_, user_ref, _)| user_ref.public_key);

    let repo_coordinate = (try_and_get_repo_coordinates_when_remote_unknown(&git_repo).await).ok();

    let Some(repo_coordinate) = repo_coordinate else {
        println!("no nostr repository found");
        println!();
        println!("use `ngit repo init` to publish this repository to nostr");
        return Ok(());
    };

    // Fetch latest data from relays — suppress the summary line.
    // fetching_quietly writes a blank line to stderr after errors so there
    // is clear separation before the repo info below.
    let _ = fetching_quietly(git_repo_path, &client, &repo_coordinate).await;

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

    print_repo_info(&repo_ref, my_pubkey.as_ref(), &repo_coordinate, git_repo_path).await;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn print_repo_info(
    repo_ref: &RepoRef,
    my_pubkey: Option<&PublicKey>,
    coordinate: &Nip19Coordinate,
    git_repo_path: &Path,
) {
    let heading = Style::new().bold();
    let dim = Style::new().dim();

    let multi_maintainer = repo_ref.maintainers.len() > 1
        || repo_ref
            .maintainers_without_annoucnement
            .as_ref()
            .is_some_and(|v| !v.is_empty());

    // --- Basic metadata ---
    println!("{}", heading.apply_to(&repo_ref.name));

    // Show identifier only when it differs from the name
    let identifier_slug = repo_ref.identifier.to_lowercase().replace(' ', "-");
    let name_slug = repo_ref.name.to_lowercase().replace(' ', "-");
    if identifier_slug != name_slug {
        println!(
            "{}",
            dim.apply_to(format!("identifier: {}", repo_ref.identifier))
        );
    }

    if !repo_ref.description.is_empty() {
        println!("{}", repo_ref.description);
    }
    if !repo_ref.web.is_empty() {
        for url in &repo_ref.web {
            println!("{}", dim.apply_to(url));
        }
    }
    if !repo_ref.hashtags.is_empty() {
        println!("{}", dim.apply_to(repo_ref.hashtags.join("  ")));
    }
    if !repo_ref.root_commit.is_empty() {
        println!(
            "{}",
            dim.apply_to(format!(
                "earliest unique commit: {}",
                &repo_ref.root_commit[..7.min(repo_ref.root_commit.len())]
            ))
        );
    }
    println!();

    // --- Maintainers ---
    println!("{}", heading.apply_to("Maintainers"));
    let trusted = &repo_ref.trusted_maintainer;
    let trusted_name = display_name_for(trusted, my_pubkey, git_repo_path).await;
    println!("  trusted: {trusted_name}");

    let co_maintainers: Vec<&PublicKey> = repo_ref
        .maintainers
        .iter()
        .filter(|m| *m != trusted)
        .collect();

    if !co_maintainers.is_empty() {
        let mut direct_names: Vec<String> = Vec::new();
        let mut indirect: Vec<(String, String)> = Vec::new(); // (name, lister_name)

        for co in &co_maintainers {
            let co_name = display_name_for(co, my_pubkey, git_repo_path).await;
            match find_lister(repo_ref, co, trusted) {
                None => direct_names.push(co_name),
                Some(lister_hex) => {
                    let lister_name = if let Ok(pk) = PublicKey::from_hex(&lister_hex) {
                        display_name_for(&pk, my_pubkey, git_repo_path).await
                    } else {
                        short_npub(&lister_hex)
                    };
                    indirect.push((co_name, lister_name));
                }
            }
        }

        if !direct_names.is_empty() {
            println!("  co-maintainers: {}", direct_names.join(", "));
        }
        for (name, lister_name) in &indirect {
            println!(
                "  {} {}",
                name,
                dim.apply_to(format!(
                    "(listed by {lister_name}, not directly by trusted maintainer)"
                ))
            );
        }
    }

    if let Some(without) = &repo_ref.maintainers_without_annoucnement {
        if !without.is_empty() {
            let mut names = Vec::new();
            for pk in without {
                names.push(display_name_for(pk, my_pubkey, git_repo_path).await);
            }
            println!(
                "  {}",
                dim.apply_to(format!(
                    "invited, no announcement yet: {}",
                    names.join(", ")
                ))
            );
        }
    }
    println!();

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
                let owners = find_server_owners(repo_ref, server, coordinate, my_pubkey, git_repo_path).await;
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
                let owners =
                    find_relay_owners(repo_ref, relay.as_str(), coordinate, my_pubkey, git_repo_path).await;
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

    // --- Maintainer model note (only relevant when there are multiple maintainers) ---
    if multi_maintainer {
        println!(
            "{}",
            dim.apply_to(
                "Note: git servers and relays are pooled from all maintainers' announcements.\n\
                 Name, description, web, and hashtags come from the most recently updated announcement.\n\
                 Each maintainer independently decides who they list as co-maintainers;\n\
                 if Alice lists Bob and Bob lists Carol, all three are in the maintainer set."
            )
        );
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

/// Find which maintainer's event lists `target` as a maintainer.
/// Returns `None` if listed directly by the trusted maintainer,
/// or `Some(lister_pubkey_hex)` if listed by a co-maintainer.
fn find_lister(repo_ref: &RepoRef, target: &PublicKey, trusted: &PublicKey) -> Option<String> {
    use nostr::nips::nip01::Coordinate;
    use nostr_sdk::Kind;

    let trusted_coord = nostr::nips::nip19::Nip19Coordinate {
        coordinate: Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: *trusted,
            identifier: repo_ref.identifier.clone(),
        },
        relays: vec![],
    };
    if let Some(event) = repo_ref.events.get(&trusted_coord) {
        let listed: Vec<PublicKey> = event
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
        if listed.contains(target) {
            return None;
        }
    }

    for (coord, event) in &repo_ref.events {
        if coord.coordinate.public_key == *trusted {
            continue;
        }
        let lister = coord.coordinate.public_key;
        let lister_listed: Vec<PublicKey> = event
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
        if lister_listed.contains(target) {
            return Some(lister.to_hex());
        }
    }

    None
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
