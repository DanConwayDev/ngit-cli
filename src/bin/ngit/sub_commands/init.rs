use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use console::{Style, Term};
use ngit::{
    accept_maintainership::{grasp_servers_from_user_or_fallback, wait_for_grasp_servers},
    agent_guidance,
    cli_interactor::{
        PromptChoiceParms, PromptConfirmParms, cli_error, multi_select_with_custom_value,
        show_multi_input_prompt_success,
    },
    client::{
        Params, get_events_from_local_cache, get_filter_state_events, get_state_from_cache,
        send_events,
    },
    event_ordering,
    git::{
        is_git_remote_helper_url,
        nostr_url::{CloneUrl, NostrUrlDecoded},
        validate_git_server_clone_url,
    },
    list::{list_from_remote, list_from_remotes},
    repo_ref::{
        apply_grasp_infrastructure, detect_existing_grasp_servers, extract_npub, extract_pks,
        format_grasp_server_url_as_relay_url, is_grasp_server_clone_url, latest_event_repo_ref,
        normalize_grasp_server_url, save_repo_config_to_yaml,
    },
    repo_state::RepoState,
    utils::join_with_and,
};
use nostr::{
    FromBech32, Kind, PublicKey, RelayUrl, ToBech32, Url,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};

use crate::{
    cli::{Cli, extract_signer_cli_arguments},
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions, nostr_url::convert_clone_url_to_https},
    git_remote_helper::push::{
        create_rejected_refspecs_and_remotes_refspecs, generate_updated_state,
    },
    login,
    push_bookkeeping::{record_accepted_push_refspecs, set_branch_upstream},
    repo_ref::{
        RepoCoordinateSource, RepoRef, ResolvedRepoCoordinate, get_repo_config_from_yaml,
        print_selected_repo, try_resolve_repo_coordinate,
    },
    state_transaction::{
        GitStatePushOutcome, LiveOps, ServerForcePolicy, StateTransaction, StateTransactionFailure,
    },
};

// ---------------------------------------------------------------------------
// InitState: determines what scenario we're in
// ---------------------------------------------------------------------------

enum InitState {
    /// No coordinate found anywhere (State A)
    Fresh,
    /// Coordinate found but no announcement event on relays (State B)
    CoordinateOnly { coordinate: Nip19Coordinate },
    /// Announcement exists, I am the selected maintainer (State C)
    MyAnnouncement {
        coordinate: Nip19Coordinate,
        repo_ref: RepoRef,
    },
    /// Announcement exists, I'm in the maintainer set (State D)
    CoMaintainer {
        coordinate: Nip19Coordinate,
        repo_ref: RepoRef,
    },
    /// Announcement exists, I'm not in the maintainer set (State E)
    NotListed {
        coordinate: Nip19Coordinate,
        repo_ref: RepoRef,
    },
}

fn may_suggest_skill(state: &InitState) -> bool {
    matches!(
        state,
        InitState::Fresh
            | InitState::CoordinateOnly { .. }
            | InitState::MyAnnouncement { .. }
            | InitState::CoMaintainer { .. }
    )
}

impl InitState {
    fn coordinate(&self) -> Option<&Nip19Coordinate> {
        match self {
            Self::Fresh => None,
            Self::CoordinateOnly { coordinate }
            | Self::MyAnnouncement { coordinate, .. }
            | Self::CoMaintainer { coordinate, .. }
            | Self::NotListed { coordinate, .. } => Some(coordinate),
        }
    }

    fn repo_ref(&self) -> Option<&RepoRef> {
        match self {
            Self::Fresh | Self::CoordinateOnly { .. } => None,
            Self::MyAnnouncement { repo_ref, .. }
            | Self::CoMaintainer { repo_ref, .. }
            | Self::NotListed { repo_ref, .. } => Some(repo_ref),
        }
    }

    /// Extract my own announcement's `RepoRef` from the events map.
    /// Returns `None` if no coordinate, no announcement, or I have no event.
    fn my_repo_ref(&self, my_pubkey: &PublicKey) -> Option<RepoRef> {
        self.repo_ref()
            .and_then(|rr| my_event_repo_ref(rr, my_pubkey))
    }

    fn has_coordinate(&self) -> bool {
        !matches!(self, Self::Fresh)
    }
}

struct ResolvedFields {
    identifier: String,
    name: String,
    description: String,
    git_servers: Vec<String>,
    relays: Vec<RelayUrl>,
    web: Vec<String>,
    upstream: Vec<Vec<String>>,
    maintainers: Vec<PublicKey>,
    earliest_unique_commit: String,
    blossoms: Vec<Url>,
    hashtags: Vec<String>,
    selected_grasp_servers: Vec<String>,
    /// Existing announcements for this coordinate, retained so a republish can
    /// order itself after the current NIP-01 winner.
    announcement_events: HashMap<Nip19Coordinate, nostr::Event>,
    /// Tags from the source announcement that aren't in ngit's known
    /// allowlist ([`is_known_tag_name`]), preserved verbatim on
    /// republish so that tags added by a future ngit version or a
    /// third-party tool aren't silently dropped. Cleared when
    /// `--clean` is passed. See [`SubCommandArgs::clean`].
    extra_tags: Vec<nostr::Tag>,
}

/// Extract my own announcement's `RepoRef` from the events map.
fn my_event_repo_ref(repo_ref: &RepoRef, my_pubkey: &PublicKey) -> Option<RepoRef> {
    repo_ref
        .events
        .values()
        .find(|e| e.pubkey == *my_pubkey)
        .and_then(|e| RepoRef::try_from((e.clone(), None)).ok())
}

/// Check if a grasp-format clone URL belongs to the given public key.
fn is_my_grasp_clone_url(url: &str, my_pubkey: &PublicKey) -> bool {
    if !is_grasp_server_clone_url(url) {
        return false;
    }
    if let Ok(npub) = extract_npub(url) {
        if let Ok(url_pk) = PublicKey::from_bech32(npub) {
            return url_pk == *my_pubkey;
        }
    }
    false
}

/// Check if a relay URL corresponds to one of the given grasp servers.
fn is_grasp_derived_relay(relay: &str, grasp_servers: &[String]) -> bool {
    let Ok(relay_normalized) = normalize_grasp_server_url(relay) else {
        return false;
    };
    grasp_servers.iter().any(|gs| {
        normalize_grasp_server_url(gs).is_ok_and(|gs_normalized| gs_normalized == relay_normalized)
    })
}

fn dir_name_fallback() -> String {
    env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default()
}

fn identifier_from_name(name: &str) -> String {
    name.replace(' ', "-")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.eq(&'/') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn build_gitworkshop_url(
    public_key: &PublicKey,
    identifier: &str,
    first_relay: Option<&RelayUrl>,
) -> String {
    NostrUrlDecoded {
        original_string: String::new(),
        coordinate: Nip19Coordinate {
            coordinate: Coordinate {
                public_key: *public_key,
                kind: Kind::GitRepoAnnouncement,
                identifier: identifier.to_string(),
            },
            relays: first_relay.into_iter().cloned().collect(),
        },
        protocol: None,
        ssh_key_file: None,
        nip05: None,
    }
    .to_string()
    .replace("nostr://", "https://gitworkshop.dev/")
}

/// Resolve the `web` field from args, existing announcement, or gitworkshop
/// default.
fn resolve_web(
    args_web: &[String],
    state: &InitState,
    identifier: &str,
    gitworkshop_url: &str,
) -> Vec<String> {
    if !args_web.is_empty() {
        return args_web.to_vec();
    }
    if let Some(rr) = state.repo_ref() {
        let latest_web = latest_event_repo_ref(rr).map_or_else(|| rr.web.clone(), |lr| lr.web);
        let joined = latest_web.join(" ");
        // replace legacy gitworkshop.dev url format
        if joined.contains(&format!("https://gitworkshop.dev/repo/{identifier}")) {
            return vec![gitworkshop_url.to_string()];
        }
        return latest_web;
    }
    vec![gitworkshop_url.to_string()]
}

/// Normalize and validate a hashtag: lowercase, strip leading `#`, allow only
/// `a-z`, `0-9`, and `-` (no leading/trailing/consecutive hyphens).
fn validate_hashtag(s: &str) -> Result<String> {
    let trimmed = s.trim().trim_start_matches('#').to_lowercase();
    if trimmed.is_empty() {
        bail!("hashtag cannot be empty");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("hashtag can only contain lowercase letters (a-z), digits (0-9), and hyphens (-)");
    }
    if trimmed.starts_with('-') || trimmed.ends_with('-') {
        bail!("hashtag cannot start or end with a hyphen");
    }
    if trimmed.contains("--") {
        bail!("hashtag cannot contain consecutive hyphens");
    }
    Ok(trimmed)
}

/// Resolve the `hashtags` field from args or existing announcement.
fn resolve_hashtags(args_hashtag: &[String], state: &InitState) -> Result<Vec<String>> {
    if !args_hashtag.is_empty() {
        return args_hashtag.iter().map(|h| validate_hashtag(h)).collect();
    }
    if let Some(rr) = state.repo_ref() {
        return Ok(latest_event_repo_ref(rr).map_or_else(|| rr.hashtags.clone(), |lr| lr.hashtags));
    }
    Ok(vec![])
}

/// Resolve which grasp servers to use. Handles flag overrides, detection from
/// existing URLs, user grasp list / system fallbacks, and interactive
/// prompting.
fn resolve_grasp_servers(
    args: &SubCommandArgs,
    cli: &Cli,
    state: &InitState,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    identifier: &str,
    interactive: bool,
) -> Result<Vec<String>> {
    if !args.grasp_server.is_empty() {
        return Ok(args.grasp_server.clone());
    }

    let has_both_relays_and_clone_url = !args.relay.is_empty() && !args.clone.is_empty();
    if has_both_relays_and_clone_url {
        return Ok(vec![]);
    }

    // Use my own announcement (not the consolidated union) for grasp detection.
    // Infrastructure is personal — each maintainer has their own servers.
    let my_ref = state.my_repo_ref(&user_ref.public_key);

    if !args.clone.is_empty() {
        return Ok(detect_existing_grasp_servers(
            my_ref.as_ref(),
            &args.relay,
            &args.clone,
            identifier,
        ));
    }

    if !interactive || cli.defaults || state.has_coordinate() || cli.force {
        // Prefer grasp servers from my existing announcement, then user's grasp
        // list (or selected maintainer's servers as fallback), then system defaults
        let existing = detect_existing_grasp_servers(my_ref.as_ref(), &args.relay, &[], identifier);
        if !existing.is_empty() {
            return Ok(existing);
        }
        // For co-maintainer state, pass the repo_ref so the selected
        // maintainer's grasp servers can be used as a fallback.
        let selected_maintainer_repo_ref = if matches!(state, InitState::CoMaintainer { .. }) {
            state.repo_ref()
        } else {
            None
        };
        return Ok(grasp_servers_from_user_or_fallback(
            user_ref,
            selected_maintainer_repo_ref,
            client,
        ));
    }

    // Interactive prompt
    let mut options: Vec<String> =
        detect_existing_grasp_servers(my_ref.as_ref(), &args.relay, &args.clone, identifier);
    let mut selections: Vec<bool> = vec![true; options.len()];
    let empty = options.is_empty();
    for user_grasp_option in &user_ref.grasp_list.urls {
        if !options
            .iter()
            .any(|option| option.contains(user_grasp_option.as_str()))
        {
            options.push(user_grasp_option.to_string());
            selections.push(empty);
        }
    }
    let empty = options.is_empty();
    let fallback_grasp_servers = client.get_grasp_default_set();
    for fallback in fallback_grasp_servers {
        if !options.iter().any(|option| option.contains(fallback)) {
            options.push(fallback.clone());
            selections.push(empty);
        }
    }
    let selected = multi_select_with_custom_value(
        "grasp servers (ideally use between 2-4)",
        "grasp server",
        options,
        selections,
        normalize_grasp_server_url,
    )?;
    show_multi_input_prompt_success("grasp servers", &selected);
    Ok(selected)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validation for State A (Fresh): no existing coordinate.
fn validate_fresh(cli: &Cli, args: &SubCommandArgs, user_has_grasp_list: bool) -> Result<()> {
    // -d or -f with no substantive flags: proceed with all defaults
    if !args.has_substantive_flags(cli.repo_relay_only) && (cli.defaults || cli.force) {
        return Ok(());
    }

    // Substantive flags provided: -d fills any gaps
    if cli.defaults {
        return Ok(());
    }

    // Validate essential fields
    let mut missing: Vec<(&str, &str)> = Vec::new();

    let missing_name = args.identifier.is_none() && args.name.is_none();
    if missing_name {
        missing.push(("--name <NAME>", "repository name or identifier"));
    }

    let has_grasp_servers = !args.grasp_server.is_empty();
    let has_both_relays_and_clone_url = !args.relay.is_empty() && !args.clone.is_empty();
    let missing_servers =
        !has_grasp_servers && !user_has_grasp_list && !has_both_relays_and_clone_url;
    if missing_servers {
        missing.push((
            "--grasp-server <URL>...",
            "where your git+nostr data is hosted",
        ));
    }

    if missing.is_empty() {
        return Ok(());
    }

    let message = if missing.len() == 1 {
        let (flag, desc) = missing[0];
        format!("missing {flag} ({desc})")
    } else {
        "missing required fields".to_string()
    };

    let mut details: Vec<(&str, &str)> = if missing.len() > 1 {
        missing.clone()
    } else {
        vec![]
    };

    details.push(("-d, --defaults", "or just use sensible defaults"));
    let name_part = if missing_name {
        " --name \"My Project\""
    } else {
        ""
    };
    let suggestion =
        format!("ngit init{name_part} --description \"my project description\" --defaults");

    Err(cli_error(&message, &details, &[&suggestion]))
}

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[clap(long, alias = "title")]
    /// name of repository (preferred over --identifier); --title is an alias
    name: Option<String>,
    #[clap(long)]
    /// shortname with no spaces or special characters
    identifier: Option<String>,
    #[clap(long)]
    /// optional description
    description: Option<String>,
    #[clap(short, long, value_parser, num_args = 1..)]
    /// where your git+nostr data is hosted
    grasp_server: Vec<String>,
    #[clap(long, value_parser, num_args = 1..)]
    /// additional relays beyond grasp servers
    relay: Vec<String>,
    #[clap(long)]
    /// additional git server URLs beyond grasp servers
    clone: Vec<String>,
    #[clap(long, value_parser, num_args = 1..)]
    /// homepage
    web: Vec<String>,
    #[clap(short = 'u', long = "u", alias = "upstream", value_parser, num_args = 1..)]
    /// informational NIP-34 subordinate-fork `u` tag fields
    upstream: Vec<String>,
    #[clap(long, value_parser, num_args = 1..)]
    /// npubs of other maintainers
    other_maintainers: Vec<String>,
    #[clap(long, value_parser, num_args = 1..)]
    /// hashtags for repository discovery
    hashtag: Vec<String>,
    #[clap(long)]
    /// usually root commit but will be more recent commit for forks
    earliest_unique_commit: Option<String>,
    #[clap(long)]
    /// drop unknown tags from the existing announcement when republishing
    /// (default is to preserve them so tags added by future ngit versions
    /// or third-party tools aren't silently lost)
    clean: bool,
}

impl SubCommandArgs {
    fn has_substantive_flags(&self, repo_relay_only: bool) -> bool {
        self.name.is_some()
            || self.identifier.is_some()
            || self.description.is_some()
            || !self.clone.is_empty()
            || !self.relay.is_empty()
            || !self.grasp_server.is_empty()
            || !self.web.is_empty()
            || !self.upstream.is_empty()
            || !self.other_maintainers.is_empty()
            || !self.hashtag.is_empty()
            || self.earliest_unique_commit.is_some()
            || repo_relay_only
            || self.clean
    }
}

// ---------------------------------------------------------------------------
// Pre/post-fetch validation
// ---------------------------------------------------------------------------

fn validate_pre_fetch(
    cli: &Cli,
    args: &SubCommandArgs,
    repo_coordinate: Option<&Nip19Coordinate>,
    user_has_grasp_list: bool,
    cached_repo_ref: Option<&RepoRef>,
    my_pubkey: &PublicKey,
) -> Result<()> {
    // Interactive mode bypasses pre-fetch validation
    if cli.interactive {
        return Ok(());
    }

    // If no coordinate exists, we're in State A (Fresh) - validate now
    if repo_coordinate.is_none() {
        return validate_fresh(cli, args, user_has_grasp_list);
    }

    // If we have cached data and it's MyAnnouncement state, validate early
    if let (Some(coord), Some(repo_ref)) = (repo_coordinate, cached_repo_ref) {
        if coord.coordinate.public_key == *my_pubkey {
            // MyAnnouncement state - validate before network fetch
            if let Some(new_id) = &args.identifier {
                if *new_id != repo_ref.identifier && !cli.force {
                    let suggestion = format!("ngit init --identifier {new_id} --force");
                    return Err(cli_error(
                        "changing identifier creates a new repository",
                        &[],
                        &[&suggestion],
                    ));
                }
            }
            if !args.has_substantive_flags(cli.repo_relay_only) && !cli.force {
                return Err(cli_error(
                    "no arguments specified, use --force to publish with new timestamp",
                    &[],
                    &["ngit init --force"],
                ));
            }
        }
    }

    Ok(())
}

fn validate_post_fetch(cli: &Cli, args: &SubCommandArgs, state: &InitState) -> Result<()> {
    // Interactive mode bypasses all validation
    if cli.interactive {
        return Ok(());
    }

    match state {
        InitState::Fresh => {
            // Already validated in pre-fetch
            Ok(())
        }
        InitState::CoordinateOnly { coordinate } => {
            if cli.force {
                Ok(())
            } else {
                let id = &coordinate.identifier;
                Err(cli_error(
                    &format!(
                        "no announcement found for coordinate '{id}'\n\n\
                         \x20 This could be a relay or network issue. Only proceed with --force\n\
                         \x20 if you are sure there isn't an existing announcement event."
                    ),
                    &[],
                    &["ngit init --force"],
                ))
            }
        }
        InitState::MyAnnouncement { repo_ref, .. } => {
            if let Some(new_id) = &args.identifier {
                if *new_id != repo_ref.identifier && !cli.force {
                    let suggestion = format!("ngit init --identifier {new_id} --force");
                    return Err(cli_error(
                        "changing identifier creates a new repository",
                        &[],
                        &[&suggestion],
                    ));
                }
            }
            if !args.has_substantive_flags(cli.repo_relay_only) && !cli.force {
                return Err(cli_error(
                    "no arguments specified, use --force to publish with new timestamp",
                    &[],
                    &["ngit init --force"],
                ));
            }
            Ok(())
        }
        InitState::CoMaintainer { repo_ref, .. } => {
            if let Some(new_id) = &args.identifier {
                if *new_id != repo_ref.identifier && !cli.force {
                    let suggestion = format!("ngit init --identifier {new_id} --force");
                    return Err(cli_error(
                        "changing identifier creates a new repository",
                        &[],
                        &[&suggestion],
                    ));
                }
            }
            Ok(())
        }
        InitState::NotListed { .. } => {
            if cli.force {
                Ok(())
            } else {
                Err(cli_error(
                    "you are not listed as a maintainer",
                    &[],
                    &["ngit init --force"],
                ))
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn resolve_fields(
    state: &InitState,
    user_ref: &ngit::login::user::UserRef,
    args: &SubCommandArgs,
    cli: &Cli,
    git_repo: &Repo,
    root_commit: &str,
    client: &Client,
    repo_config_result: &Result<ngit::repo_ref::RepoConfigYaml>,
    interactive: bool,
) -> Result<ResolvedFields> {
    let my_pubkey = &user_ref.public_key;

    // Shared lookups used by multiple fields below
    let latest = state.repo_ref().and_then(latest_event_repo_ref);
    let my_ref = state.my_repo_ref(my_pubkey);

    // --- Identifier default ---
    let identifier_default = if let Some(coord) = state.coordinate() {
        coord.identifier.clone()
    } else if let Ok(config) = repo_config_result {
        if let Some(id) = &config.identifier {
            id.clone()
        } else {
            dir_name_fallback()
        }
    } else {
        dir_name_fallback()
    };

    // --- Name ---
    let name_default = if let Some(ref lr) = latest {
        lr.name.clone()
    } else if let Some(coord) = state.coordinate() {
        coord.identifier.clone()
    } else {
        dir_name_fallback()
    };

    let name = if let Some(v) = &args.name {
        v.clone()
    } else if interactive {
        Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo name")
                .with_default(name_default.clone())
                .with_flag_name("--name"),
        )?
    } else {
        name_default.clone()
    };

    // --- Description ---
    let description_default = latest
        .as_ref()
        .map_or_else(String::new, |lr| lr.description.clone());

    let description = if let Some(v) = &args.description {
        v.clone()
    } else if interactive {
        Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo description (one sentence)")
                .optional()
                .with_default(description_default.clone())
                .with_flag_name("--description"),
        )?
    } else {
        description_default
    };

    // --- Simple mode (interactive only) ---
    let simple_mode = if !interactive || (!args.clone.is_empty() && !args.relay.is_empty()) {
        false // not used in non-interactive, but avoids Option
    } else {
        Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt("config mode")
                .with_choices(vec![
                    "simple - all you need".to_string(),
                    "advanced - all the dials and switches".to_string(),
                ])
                .with_default(0),
        )? == 0
    };

    // --- Identifier ---
    let identifier = if let Some(id) = &args.identifier {
        id.clone()
    } else if state.has_coordinate() {
        identifier_default.clone()
    } else if !interactive || cli.defaults {
        if args.name.is_some() && !state.has_coordinate() {
            identifier_from_name(&name)
        } else {
            identifier_default.clone()
        }
    } else {
        let id_default = if args.name.is_some() || name != name_default {
            identifier_from_name(&name)
        } else {
            identifier_default.clone()
        };
        Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo identifier")
                .with_default(id_default)
                .with_flag_name("--identifier"),
        )?
    };

    // --- Grasp servers ---
    let selected_grasp_servers =
        resolve_grasp_servers(args, cli, state, user_ref, client, &identifier, interactive)?;

    // --- Base infrastructure (flag > my event > fallback) ---
    // Grasp-derived infrastructure (my clone URLs, relays) is handled
    // by apply_grasp_infrastructure below. Defaults here are *additional*
    // infrastructure only. My own grasp-format clone URLs are filtered out so
    // they get re-derived from the resolved grasp servers. Grasp-format clone
    // URLs belonging to other maintainers are kept as additional git servers.
    let no_state = git_repo
        .get_git_config_item("nostr.nostate", None)
        .ok()
        .flatten()
        .is_some_and(|s| s == "true");

    // Detect my grasp servers from my existing announcement (for filtering)
    let my_existing_grasp_servers: Vec<String> = my_ref
        .as_ref()
        .map(|mr| detect_existing_grasp_servers(Some(mr), &[], &[], &identifier))
        .unwrap_or_default();

    let git_servers_default = if let Some(ref mr) = my_ref {
        // Keep non-grasp URLs and grasp URLs from other maintainers;
        // filter out my own grasp-derived clone URLs (re-derived from grasp servers)
        mr.git_server
            .iter()
            .filter(|url| !is_my_grasp_clone_url(url, my_pubkey))
            .cloned()
            .collect()
    } else if no_state {
        // Only fall back to origin URL when nostate is set (user pushes directly
        // to a traditional git server rather than through grasp servers)
        if let Ok(url) = git_repo.get_origin_url() {
            if let Ok(fetch_url) = convert_clone_url_to_https(&url) {
                vec![fetch_url]
            } else if url.starts_with("nostr://") {
                vec![]
            } else {
                vec![url]
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let relays_default = if let Some(ref mr) = my_ref {
        // Keep relays that don't correspond to my grasp servers
        // (grasp-derived relays are re-added by apply_grasp_infrastructure)
        mr.relays
            .iter()
            .map(std::string::ToString::to_string)
            .filter(|r| !is_grasp_derived_relay(r, &my_existing_grasp_servers))
            .collect()
    } else if let Ok(config) = repo_config_result {
        config.relays.clone()
    } else {
        vec![]
    };

    let mut git_servers = if args.clone.is_empty() {
        git_servers_default
    } else {
        args.clone.clone()
    };
    let mut relay_strings = if args.relay.is_empty() {
        relays_default
    } else {
        args.relay.clone()
    };

    apply_grasp_infrastructure(
        &selected_grasp_servers,
        &mut git_servers,
        &mut relay_strings,
        &user_ref.public_key,
        &identifier,
    )?;

    // --- Interactive: nostr.nostate prompt ---
    if interactive
        && no_state
        && Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("store state on nostr? required for nostr-permissioned git servers")
                .with_default(true),
        )?
    {
        if git_repo
            .get_git_config_item("nostr.nostate", Some(true))
            .unwrap_or(None)
            .is_some()
        {
            git_repo.remove_git_config_item("nostr.nostate", true)?;
        } else {
            git_repo.remove_git_config_item("nostr.nostate", false)?;
        }
    }

    // --- Git servers (interactive prompting) ---
    let git_servers = if !args.clone.is_empty() || !interactive {
        git_servers
    } else {
        prompt_git_servers(git_servers, &selected_grasp_servers, simple_mode)?
    };
    for git_server in &git_servers {
        validate_git_server_url(git_server)?;
    }

    // --- Relays ---
    let relays: Vec<RelayUrl> = if !args.relay.is_empty() || !interactive {
        relay_strings
            .iter()
            .filter_map(|r| parse_relay_url(r).ok())
            .collect()
    } else if simple_mode {
        let grasp_relay_urls: Vec<String> = selected_grasp_servers
            .iter()
            .filter_map(|r| format_grasp_server_url_as_relay_url(r).ok())
            .collect();
        let options: Vec<String> = relay_strings
            .iter()
            .filter(|s| !grasp_relay_urls.iter().any(|r| s.as_str() == r))
            .cloned()
            .collect();
        let selections: Vec<bool> = vec![true; options.len()];
        let selected = multi_select_with_custom_value(
            "extra nostr relays (grasp servers are sufficient; public relays optional)",
            "nostr relay",
            options,
            selections,
            |s| {
                parse_relay_url(s)
                    .map(|_| s.to_string())
                    .context(format!("Invalid relay URL format: {s}"))
            },
        )?;
        show_multi_input_prompt_success("additional nostr relays", &selected);
        [
            grasp_relay_urls
                .iter()
                .filter_map(|r| parse_relay_url(r).ok())
                .collect::<Vec<RelayUrl>>(),
            selected
                .iter()
                .filter_map(|r| parse_relay_url(r).ok())
                .collect::<Vec<RelayUrl>>(),
        ]
        .concat()
    } else {
        // advanced interactive
        let selections: Vec<bool> = vec![true; relay_strings.len()];
        let selected = multi_select_with_custom_value(
            "nostr relays",
            "nostr relay",
            relay_strings,
            selections,
            |s| {
                parse_relay_url(s)
                    .map(|_| s.to_string())
                    .context(format!("Invalid relay URL format: {s}"))
            },
        )?;
        show_multi_input_prompt_success("nostr relays", &selected);
        selected
            .iter()
            .filter_map(|r| parse_relay_url(r).ok())
            .collect()
    };

    // --- Maintainers ---
    let maintainers_default = if let Some(ref mr) = my_ref {
        let mut m = vec![*my_pubkey];
        for pk in &mr.maintainers {
            if !m.contains(pk) {
                m.push(*pk);
            }
        }
        m
    } else if let Some(coord) = state.coordinate() {
        let selected = coord.coordinate.public_key;
        if selected == *my_pubkey {
            vec![*my_pubkey]
        } else {
            vec![*my_pubkey, selected]
        }
    } else {
        vec![*my_pubkey]
    };

    let base_maintainers = if args.other_maintainers.is_empty() {
        maintainers_default
    } else {
        let mut m = vec![user_ref.public_key];
        for npub in &args.other_maintainers {
            if let Ok(pk) = PublicKey::from_bech32(npub) {
                if !m.contains(&pk) {
                    m.push(pk);
                }
            }
        }
        m
    };

    let maintainers = if !args.other_maintainers.is_empty()
        || !interactive
        || (base_maintainers.len() == 1
            && Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_prompt("add other maintainers now?")
                    .dont_report()
                    .with_choices(vec![
                        "maybe later".to_string(),
                        "add maintainers".to_string(),
                    ])
                    .with_default(0),
            )? == 0)
    {
        base_maintainers
    } else {
        let selections: Vec<bool> = vec![true; base_maintainers.len()];
        let selected = multi_select_with_custom_value(
            "maintainers",
            "maintainer npub",
            base_maintainers
                .iter()
                .filter_map(|m| m.to_bech32().ok())
                .collect(),
            selections,
            |s| {
                extract_npub(s)
                    .map(|_| s.to_string())
                    .context(format!("Invalid npub: {s}"))
            },
        )?;
        show_multi_input_prompt_success("maintainers", &selected);
        selected
            .iter()
            .filter_map(|npub| PublicKey::parse(npub).ok())
            .collect()
    };

    // --- Interactive: github/codeberg warning ---
    if interactive
        && selected_grasp_servers.is_empty()
        && git_servers
            .iter()
            .any(|s| s.contains("github.com") || s.contains("codeberg.org"))
        && Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("you have listed github / codeberg. Are you or other maintainers planning on pushing directly to github / codeberg rather than using your shiny new nostr clone url which will do this for you?")
                .with_default(false),
        )?
    {
        println!("This means people using the nostr URL won't get your latest branch updates.");
        if Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("opt-out of storing git state on nostr and relay on github for now? you will still receive PRs and issues via nostr")
                .with_default(true),
        )? {
            git_repo.save_git_config_item("nostr.nostate", "true", false)?;
        }
    }

    // --- Web ---
    let gitworkshop_url = build_gitworkshop_url(&user_ref.public_key, &identifier, relays.first());
    let web_default = resolve_web(&args.web, state, &identifier, &gitworkshop_url);

    let web = if !args.web.is_empty() || !interactive || simple_mode {
        web_default
    } else {
        // advanced interactive
        let web_default_str = web_default.join(" ");
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("repo website")
                    .optional()
                    .with_default(web_default_str)
                    .with_flag_name("--web"),
            )?
            .split(' ')
            .map(std::string::ToString::to_string)
            .collect()
    };

    // --- Informational upstream (`u`) tags ---
    // NIP-34 uses `u` to mark a repository as a subordinate fork. ngit never
    // invents one; it only preserves existing metadata or emits fields
    // explicitly supplied with `--u` / `--upstream`.
    let upstream = if args.upstream.is_empty() {
        latest
            .as_ref()
            .map_or_else(Vec::new, |lr| lr.upstream.clone())
    } else {
        vec![args.upstream.clone()]
    };

    // --- Earliest unique commit ---
    // Cascade: my event -> consolidated RepoRef (selected maintainer's) -> local
    // root commit
    let my_euc = my_ref
        .as_ref()
        .map(|mr| &mr.root_commit)
        .filter(|c| !c.is_empty());
    let repo_euc = state
        .repo_ref()
        .map(|rr| &rr.root_commit)
        .filter(|c| !c.is_empty());
    let euc_default = my_euc
        .or(repo_euc)
        .cloned()
        .unwrap_or_else(|| root_commit.to_string());

    let earliest_unique_commit = if let Some(commit) = &args.earliest_unique_commit {
        if let Ok(exists) = git_repo.does_commit_exist(commit) {
            if !exists {
                bail!("earliest unique commit does not exist on current repository");
            }
        } else {
            bail!("earliest unique commit id not formatted correctly");
        }
        if commit.len() != 40 {
            bail!("earliest unique commit id must be 40 characters long");
        }
        commit.clone()
    } else if interactive && !simple_mode {
        println!(
            "the earliest unique commit helps with discoverability. It defaults to the root commit. Only change this if your repo has completely forked off an has formed its own identity."
        );
        let mut result = euc_default.clone();
        loop {
            result = Interactor::default().input(
                PromptInputParms::default()
                    .with_prompt("earliest unique commit (to help with discoverability)")
                    .with_default(result.clone())
                    .with_flag_name("--earliest-unique-commit"),
            )?;
            if let Ok(exists) = git_repo.does_commit_exist(&result) {
                if exists && result.len() == 40 {
                    break;
                }
                if !exists {
                    println!("commit does not exist on current repository");
                }
            } else {
                println!("commit id not formatted correctly");
            }
            if result.len() != 40 {
                println!("commit id must be 40 characters long");
            }
        }
        result
    } else {
        euc_default
    };

    // --- Blossoms (preserve from latest event) ---
    let blossoms = latest
        .as_ref()
        .map_or_else(Vec::new, |lr| lr.blossoms.clone());

    // --- Hashtags (shared metadata — from latest event, like name/description/web)
    // ---
    let hashtags_default = resolve_hashtags(&args.hashtag, state)?;

    let hashtags = if !args.hashtag.is_empty() || !interactive || simple_mode {
        hashtags_default
    } else {
        // advanced interactive
        let selections: Vec<bool> = vec![true; hashtags_default.len()];
        let selected = multi_select_with_custom_value(
            "hashtags for repository discovery",
            "hashtag",
            hashtags_default,
            selections,
            validate_hashtag,
        )?;
        show_multi_input_prompt_success("hashtags", &selected);
        selected
    };

    // --- Extra (unknown) tags ---
    // Cascade: --clean wipes them, otherwise inherit from the latest
    // event across all maintainers (so a co-maintainer's newer publish
    // propagates new tags, matching how name/description/web cascade).
    // Falls back to my own event for symmetry; in MyAnnouncement state
    // my event *is* the latest so both branches return the same set.
    let extra_tags: Vec<nostr::Tag> = if args.clean {
        vec![]
    } else if let Some(ref lr) = latest {
        lr.extra_tags.clone()
    } else if let Some(ref mr) = my_ref {
        mr.extra_tags.clone()
    } else {
        vec![]
    };

    Ok(ResolvedFields {
        identifier,
        name,
        description,
        git_servers,
        relays,
        web,
        upstream,
        maintainers,
        earliest_unique_commit,
        blossoms,
        hashtags,
        selected_grasp_servers,
        announcement_events: state
            .repo_ref()
            .map(|repo_ref| repo_ref.events.clone())
            .unwrap_or_default(),
        extra_tags,
    })
}

/// Interactive prompt for git server selection with simple/advanced modes.
fn prompt_git_servers(
    git_servers: Vec<String>,
    selected_grasp_servers: &[String],
    simple_mode: bool,
) -> Result<Vec<String>> {
    let grasp_server_git_servers: Vec<String> = git_servers
        .iter()
        .filter(|s| is_grasp_server_clone_url(s))
        .cloned()
        .collect();
    let mut additional_server_options: Vec<String> = git_servers
        .iter()
        .filter(|s| !is_grasp_server_clone_url(s))
        .cloned()
        .collect();

    if simple_mode && !selected_grasp_servers.is_empty() {
        if additional_server_options.is_empty() {
            return Ok(git_servers);
        }
        let selected = loop {
            let selections: Vec<bool> = vec![true; additional_server_options.len()];
            let selected = multi_select_with_custom_value(
                "additional git server(s) on top of grasp servers",
                "git server remote url",
                additional_server_options,
                selections,
                validate_git_server_url,
            )?;

            if selected.is_empty()
                || Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_prompt("if you or another maintainer start pushing directly to these, nostr will be out of date")
                        .dont_report()
                        .with_choices(vec![
                            "I'll always push to the nostr remote".to_string(),
                            "change setup".to_string(),
                        ])
                        .with_default(0),
                )? == 1
            {
                additional_server_options = selected;
                continue;
            }
            break selected;
        };
        show_multi_input_prompt_success("additional git servers", &selected);
        let mut combined = grasp_server_git_servers;
        combined.extend(selected);
        Ok(combined)
    } else {
        let selections: Vec<bool> = vec![true; git_servers.len()];
        let selected = multi_select_with_custom_value(
            "git server remote url(s)",
            "git server remote url",
            git_servers,
            selections,
            validate_git_server_url,
        )?;
        show_multi_input_prompt_success("git servers", &selected);
        Ok(selected)
    }
}

fn validate_git_server_url(url: &str) -> Result<String> {
    validate_git_server_clone_url(url)?;
    if is_git_remote_helper_url(url) {
        Ok(url.to_string())
    } else {
        CloneUrl::from_str(url)
            .map(|_| url.to_string())
            .context(format!("Invalid git server URL format: {url}"))
    }
}

/// How `ngit init` establishes the repository state after publishing
/// the announcement.
enum StateAction {
    /// `nostr.nostate`: no state event is created or synced.
    None,
    /// A canonical state event is already cached: propagate it
    /// in-process through `ngit sync`'s authoritative flow (nothing is
    /// republished or re-signed).
    SyncCachedState,
    /// No canonical state, but the pre-existing `origin` remote was
    /// listable: build a candidate state event from its listing
    /// (carried here) and establish it through a candidate state
    /// transaction.
    PublishOriginState(HashMap<String, String>),
    /// Fresh repository: push the local main/master branch and its
    /// state through the state transaction.
    PushInitialBranch,
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn publish_and_finalize(
    fields: ResolvedFields,
    signer: Arc<ngit::signer::NgitSigner>,
    user_ref: &ngit::login::user::UserRef,
    client: &mut Client,
    cli: &Cli,
    git_repo: &Repo,
    repo_config_result: &Result<ngit::repo_ref::RepoConfigYaml>,
    is_co_maintainer_first_acceptance: bool,
    selected_repo: Option<&ResolvedRepoCoordinate>,
) -> Result<()> {
    let git_repo_path = git_repo.get_path()?;

    // Step 1: Build RepoRef
    //
    // `fields.extra_tags` carries any tags on the source announcement
    // that this ngit doesn't itself emit (see `is_known_tag_name`).
    // They are round-tripped verbatim so a tag added by a future ngit
    // version or third-party tool isn't silently lost; `--clean`
    // suppresses the carry-over upstream in `resolve_fields`.
    if !fields.extra_tags.is_empty() {
        let names: Vec<String> = fields
            .extra_tags
            .iter()
            .filter_map(|t| t.as_slice().first().cloned())
            .collect();
        let warn_style = Style::new().yellow();
        eprintln!(
            "{}",
            warn_style.apply_to(format!(
                "warning: preserving unknown tag(s) from existing announcement: {}",
                names.join(", "),
            )),
        );
        eprintln!(
            "{}",
            warn_style.apply_to("         pass --clean to drop them on republish"),
        );
    }
    let repo_ref = RepoRef {
        identifier: fields.identifier.clone(),
        name: fields.name,
        description: fields.description,
        root_commit: fields.earliest_unique_commit,
        git_server: fields.git_servers,
        web: fields.web,
        upstream: fields.upstream,
        relays: fields.relays.clone(),
        blossoms: fields.blossoms,
        hashtags: fields.hashtags,
        selected_maintainer: user_ref.public_key,
        maintainers_without_annoucnement: None,
        maintainers: fields.maintainers.clone(),
        events: fields.announcement_events,
        nostr_git_url: None,
        extra_tags: fields.extra_tags,
    };

    // Whether the network fetch in `launch` already covered the
    // coordinate being announced. The fetch runs only when a repo
    // coordinate was resolved, and an `--identifier` change announces a
    // coordinate that fetch never saw. (A co-maintainer's resolved
    // coordinate names another maintainer, but the fetch discovers and
    // covers every maintainer coordinate for the identifier, so
    // matching on the identifier is sufficient.)
    let announced_coordinate_fetched =
        selected_repo.is_some_and(|resolved| resolved.coordinate.identifier == repo_ref.identifier);

    let selected_repo = selected_repo
        .cloned()
        .unwrap_or_else(|| ResolvedRepoCoordinate {
            coordinate: Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: user_ref.public_key,
                    identifier: repo_ref.identifier.clone(),
                },
                relays: repo_ref.relays.clone(),
            },
            source: RepoCoordinateSource::NewRepository,
            remote: None,
        });
    print_selected_repo(&selected_repo);

    // Step 2: Create event
    let repo_event = repo_ref.to_event(&signer).await?;

    // Step 3: Build nostr URL
    let nostr_url_decoded = repo_ref.to_nostr_git_url(&Some(git_repo));

    let events = vec![repo_event];

    // Step 4: Handle state events and push/sync logic
    let no_state = if let Ok(Some(s)) = git_repo.get_git_config_item("nostr.nostate", None) {
        s == "true"
    } else {
        false
    };

    // Stale-state protection: a candidate state event must only be
    // built after this invocation has fetched state events for the
    // coordinate being announced, otherwise a fresh init could re-sign
    // ref values older than a state event it never saw. `launch` only
    // fetches when a repo coordinate was already resolvable, so cover
    // the announced coordinate here when it didn't.
    if !no_state && !announced_coordinate_fetched {
        if let Err(error) = fetching_with_report(
            git_repo_path,
            client,
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: user_ref.public_key,
                    identifier: repo_ref.identifier.clone(),
                },
                relays: repo_ref.relays.clone(),
            },
        )
        .await
        {
            eprintln!(
                "WARNING: failed to fetch any existing repository state from relays: {error:#}"
            );
        }
    }

    let state_action = if no_state {
        // user explicitly opted out of state-event creation
        StateAction::None
    } else if get_state_from_cache(Some(git_repo.get_path()?), &repo_ref)
        .await
        .is_ok()
    {
        // A canonical state event is already cached: propagate it
        // in-process via ngit sync's authoritative flow, which seeds
        // only the relays missing it. Nothing is rebuilt or re-signed —
        // re-signing cached ref values as a newer event is how stale
        // state clobbers newer state.
        StateAction::SyncCachedState
    } else if let Ok(remote) = git_repo.git_repo.find_remote("origin") {
        if let Ok(url) = remote.url() {
            // Build a state event from the pre-existing origin's
            // listing. It is a transaction candidate: pushed to the
            // repo's git servers, fanned out to the relays and cached
            // only after acceptance (see publish_origin_state).
            if let Ok(mut origin_state) =
                list_from_remote(&Term::stdout(), git_repo, url, &nostr_url_decoded, false)
            {
                origin_state.retain(|key, _| {
                    key.starts_with("refs/heads/")
                        || key.starts_with("refs/tags/")
                        || key.starts_with("HEAD")
                });
                StateAction::PublishOriginState(origin_state)
            } else {
                // cant reach existing origin so just try push
                StateAction::PushInitialBranch
            }
        } else {
            // origin never connected so just try push
            StateAction::PushInitialBranch
        }
    } else {
        // no origin so we need to just push
        StateAction::PushInitialBranch
    };

    // Step 5: Publish events
    client.set_signer(signer.clone()).await;

    let _ = send_events(
        client,
        Some(git_repo_path),
        events,
        user_ref.relays.write(),
        fields.relays.clone(),
        !cli.disable_cli_spinners,
        false,
    )
    .await?;

    // Step 6: Set git config
    git_repo.save_git_config_item(
        "nostr.repo",
        &Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: user_ref.public_key,
                identifier: fields.identifier.clone(),
            },
            relays: vec![],
        }
        .to_bech32()?,
        false,
    )?;

    // Step 7: Set origin remote
    let nostr_url = nostr_url_decoded.to_string();
    if git_repo.git_repo.find_remote("origin").is_ok() {
        git_repo.git_repo.remote_set_url("origin", &nostr_url)?;
    } else {
        git_repo.git_repo.remote("origin", &nostr_url)?;
    }
    println!("set remote origin to nostr url");

    // Step 8: Push/sync
    match state_action {
        StateAction::None => {}
        StateAction::PushInitialBranch => {
            let branch_name = main_or_master_branch_name(git_repo)?;
            if !fields.selected_grasp_servers.is_empty() {
                wait_for_grasp_servers(
                    git_repo,
                    &fields.selected_grasp_servers,
                    &user_ref.public_key,
                    &fields.identifier,
                )
                .await?;
            }

            println!("pushing your repository data to your git server(s)...");
            push_initial_branch(
                git_repo,
                &repo_ref,
                user_ref,
                client,
                &signer,
                &nostr_url_decoded,
                branch_name,
            )
            .await
            .with_context(|| {
                format!(
                    "your repository announcement was published to nostr but pushing your git data failed. fix the reported issue and run `git push -u origin {branch_name}` to push your git data and publish the repository state"
                )
            })?;
        }
        StateAction::SyncCachedState => {
            if !fields.selected_grasp_servers.is_empty() {
                wait_for_grasp_servers(
                    git_repo,
                    &fields.selected_grasp_servers,
                    &user_ref.public_key,
                    &fields.identifier,
                )
                .await?;
            }

            println!("syncing your repository's git servers with its nostr state...");
            super::sync::sync_with_client(
                &super::sync::SubCommandArgs::default(),
                git_repo,
                client,
                true,
            )
            .await
            .context(
                "your repository announcement was published to nostr but syncing your repository's git servers with its nostr state failed. fix the reported issue and run `ngit sync`",
            )?;
        }
        StateAction::PublishOriginState(origin_state) => {
            if !fields.selected_grasp_servers.is_empty() {
                wait_for_grasp_servers(
                    git_repo,
                    &fields.selected_grasp_servers,
                    &user_ref.public_key,
                    &fields.identifier,
                )
                .await?;
            }

            println!(
                "publishing your repository state and syncing your git server(s) with the existing origin's state..."
            );
            publish_origin_state(
                git_repo,
                &repo_ref,
                user_ref,
                client,
                &signer,
                &nostr_url_decoded,
                origin_state,
            )
            .await
            .context(
                "your repository announcement was published to nostr but publishing its repository state failed. fix the reported issue and run `ngit sync`",
            )?;
        }
    }

    // Step 9: Print share URLs / completion message
    let gitworkshop_url = nostr_url_decoded
        .to_string()
        .replace("nostr://", "https://gitworkshop.dev/");
    if is_co_maintainer_first_acceptance {
        println!("co-maintainership accepted.");
        println!("your announcement was published to nostr. you can now push updates.");
        println!("your repository URL: {gitworkshop_url}");
        println!("your clone URL: {nostr_url}");
        println!(
            "note: run `ngit init` at any time to update your announcement (relays, git servers, etc.)"
        );
    } else {
        println!("share your repository: {gitworkshop_url}");
        println!("clone url: {nostr_url}");
    }

    // Step 10: Update maintainers.yaml if needed
    let relays = fields
        .relays
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<String>>();
    if match repo_config_result {
        Ok(config) => {
            !<std::option::Option<std::string::String> as Clone>::clone(&config.identifier)
                .unwrap_or_default()
                .eq(&fields.identifier)
                || !extract_pks(config.maintainers.clone())?.eq(&fields.maintainers)
                || !config.relays.eq(&relays)
        }
        Err(_) => false,
    } {
        let title_style = Style::new().bold().fg(console::Color::Yellow);
        println!("{}", title_style.apply_to("maintainers.yaml"));
        save_repo_config_to_yaml(
            git_repo,
            fields.identifier.clone(),
            fields.maintainers.clone(),
            relays.clone(),
        )?;
        println!(
            "maintainers.yaml {}. commit and push.",
            if repo_config_result.is_err() {
                "created"
            } else {
                "updated"
            }
        );
        println!(
            "this optional file helps in identifying who the maintainers are over time through the commit history"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    // Phase 1: Local-only setup
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        Some(&client),
        false,
    )
    .await?;

    let resolved_repo_coordinate = try_resolve_repo_coordinate(&git_repo).await?;
    let repo_coordinate = resolved_repo_coordinate
        .as_ref()
        .map(|resolved| resolved.coordinate.clone());

    // Phase 2: Try to get cached repo_ref for early validation
    let cached_repo_ref = if let Some(coord) = &repo_coordinate {
        (get_repo_ref_from_cache(Some(git_repo_path), coord).await).ok()
    } else {
        None
    };

    // Phase 3: Pre-fetch validation (fail fast)
    let user_has_grasp_list = !user_ref.grasp_list.urls.is_empty();
    validate_pre_fetch(
        cli_args,
        args,
        repo_coordinate.as_ref(),
        user_has_grasp_list,
        cached_repo_ref.as_ref(),
        &user_ref.public_key,
    )?;

    // Phase 4: Network fetch (only if coordinate exists)
    let repo_ref = if let Some(repo_coordinate) = &repo_coordinate {
        fetching_with_report(git_repo_path, &client, repo_coordinate).await?;
        (get_repo_ref_from_cache(Some(git_repo_path), repo_coordinate).await).ok()
    } else {
        None
    };

    // Phase 4: Determine state + post-fetch validation
    let state = match (&repo_coordinate, &repo_ref) {
        (None, _) => InitState::Fresh,
        (Some(coord), None) => InitState::CoordinateOnly {
            coordinate: coord.clone(),
        },
        (Some(coord), Some(rr)) => {
            if coord.coordinate.public_key == user_ref.public_key {
                InitState::MyAnnouncement {
                    coordinate: coord.clone(),
                    repo_ref: rr.clone(),
                }
            } else if rr.maintainers.contains(&user_ref.public_key) {
                InitState::CoMaintainer {
                    coordinate: coord.clone(),
                    repo_ref: rr.clone(),
                }
            } else {
                InitState::NotListed {
                    coordinate: coord.clone(),
                    repo_ref: rr.clone(),
                }
            }
        }
    };

    validate_post_fetch(cli_args, args, &state)?;

    // Print CoMaintainer-specific context before proceeding so the user
    // understands they are accepting (or updating) a co-maintainership
    // offer, NOT creating a new repository.
    let is_co_maintainer_first_acceptance =
        if let InitState::CoMaintainer { repo_ref: rr, .. } = &state {
            rr.maintainers_without_annoucnement
                .as_ref()
                .is_some_and(|ms| ms.contains(&user_ref.public_key))
        } else {
            false
        };

    if let InitState::CoMaintainer { repo_ref: rr, .. } = &state {
        if is_co_maintainer_first_acceptance {
            println!(
                "accepting co-maintainership of '{}' (offered by {})",
                rr.name,
                rr.selected_maintainer
                    .to_bech32()
                    .unwrap_or_else(|_| rr.selected_maintainer.to_string()),
            );
            println!(
                "publishing your repository announcement to nostr to confirm your co-maintainership..."
            );
            if cli_args.interactive {
                println!("tip: run `ngit init -d` to accept with defaults and skip all prompts");
            }
        } else {
            println!(
                "updating your co-maintainer announcement for '{}' on nostr...",
                rr.name
            );
        }
    }

    // Phase 5: Resolve all fields
    let repo_config_result = get_repo_config_from_yaml(&git_repo);
    let fields = resolve_fields(
        &state,
        &user_ref,
        args,
        cli_args,
        &git_repo,
        &root_commit.to_string(),
        &client,
        &repo_config_result,
        cli_args.interactive,
    )?;

    // Phase 6: Persist --repo-relay-only flag to local git config if supplied
    if cli_args.repo_relay_only {
        git_repo.save_git_config_item("nostr.repo-relay-only", "true", false)?;
    }

    // Phase 7: Build and publish
    let suggest_skill = may_suggest_skill(&state);
    let result = publish_and_finalize(
        fields,
        signer,
        &user_ref,
        &mut client,
        cli_args,
        &git_repo,
        &repo_config_result,
        is_co_maintainer_first_acceptance,
        resolved_repo_coordinate.as_ref(),
    )
    .await;
    if result.is_ok() && suggest_skill && should_suggest_skill(&git_repo, git_repo_path) {
        print_skill_suggestion();
    }
    result
}

fn should_suggest_skill(git_repo: &Repo, git_repo_path: &Path) -> bool {
    agent_guidance::reminders_enabled(git_repo).unwrap_or(false)
        && agent_guidance::status(git_repo_path).is_ok_and(|status| !status.installed)
}

fn print_skill_suggestion() {
    eprintln!(
        "{}",
        Style::new()
            .fg(console::Color::Color256(214))
            .apply_to(
                "tip: help coding agents collaborate through ngit by running `ngit skill install` (or `ngit skill opt-out --local` to stop reminders here)",
            )
            .for_stderr()
    );
}

fn parse_relay_url(s: &str) -> Result<RelayUrl> {
    // Attempt to parse the original string
    match RelayUrl::parse(s) {
        Ok(url) => Ok(url),
        Err(original_err) => {
            // If parsing fails, prefix with "wss://" and try again
            let prefixed = format!("wss://{s}");
            RelayUrl::parse(&prefixed).map_err(|_| original_err)
        }
    }
    .context(format!("failed to parse relay url: {s}"))
}

fn main_or_master_branch_name(git_repo: &Repo) -> Result<&'static str> {
    let local_branches = git_repo
        .get_local_branch_names()
        .context("failed to find any local branches")?;
    if local_branches.contains(&"main".to_string()) {
        Ok("main")
    } else if local_branches.contains(&"master".to_string()) {
        Ok("master")
    } else {
        bail!(
            "set remote origin to nostr url and tried to push main or master branch but they dont exist yet"
        )
    }
}

/// Push the local `main`/`master` branch and its repository state to the
/// repo's git servers through the state transaction — the in-process
/// equivalent of the `git push -u origin <branch>` subprocess init used
/// to spawn. The candidate state event is built exactly as the remote
/// helper would have built it (the first reachable git server's listing
/// plus the pushed branch) and becomes locally authoritative only after
/// a git server and at least one relay accepted it.
async fn push_initial_branch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    signer: &Arc<ngit::signer::NgitSigner>,
    nostr_url_decoded: &NostrUrlDecoded,
    branch_name: &str,
) -> Result<()> {
    let term = Term::stderr();
    let refspec = format!("refs/heads/{branch_name}:refs/heads/{branch_name}");
    let refspecs = vec![refspec.clone()];

    // Git-server reality must come from a same-invocation listing; local
    // remote-tracking refs are only written after a successful push.
    let list_outputs = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        nostr_url_decoded,
        None,
    )
    .await;

    // Mirror the remote helper: with no state event on nostr yet, the
    // baseline state is the first reachable git server's listing.
    let existing_state = repo_ref
        .git_server
        .iter()
        .find_map(|url| list_outputs.get(url).map(|(state, _)| state.clone()))
        .with_context(|| {
            format!(
                "failed to connect to git servers: {}",
                repo_ref.git_server.join(" ")
            )
        })?;

    let (rejected_refspecs, remote_refspecs) = create_rejected_refspecs_and_remotes_refspecs(
        &term,
        git_repo,
        &refspecs,
        &existing_state,
        &list_outputs,
    )?;
    if rejected_refspecs.contains_key(&refspec) {
        bail!("refs/heads/{branch_name} is out of sync with an existing git server");
    }

    let new_state = generate_updated_state(git_repo, &existing_state, &refspecs)?;

    // The newest cached state event across maintainer coordinates is
    // the NIP-01 ordering reference for the candidate. A fresh init
    // just fetched and normally finds none, but keeping the ordering
    // discipline means this invocation can never re-sign ref values as
    // an event that loses to a cached predecessor.
    let old_state_event = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![get_filter_state_events(&repo_ref.coordinates(), true)],
    )
    .await
    .ok()
    .and_then(|events| event_ordering::latest_event(&events).cloned());

    let state = RepoState::build(
        repo_ref.identifier.clone(),
        new_state,
        signer,
        old_state_event.as_ref(),
    )
    .await?;

    let repo_relay_only = git_repo
        .get_git_config_item("nostr.repo-relay-only", None)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true");
    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        user_ref.relays.write()
    };

    let mut ops = LiveOps {
        client,
        git_repo,
        term: &term,
        git_server_push_options: &[],
        decoded_nostr_url: nostr_url_decoded,
    };
    let mut transaction = StateTransaction::new(repo_ref, Some(state));
    transaction.publish_state_to_grasps_first(&mut ops).await;
    match transaction.push_git_state_refspecs(&mut ops, remote_refspecs, &refspecs) {
        GitStatePushOutcome::NoEligibleServers => {
            bail!(
                "{}",
                StateTransactionFailure::NoEligibleGitServers.user_message()
            )
        }
        GitStatePushOutcome::AllPushesFailed => {
            bail!(
                "{}",
                StateTransactionFailure::AllGitServerPushesFailed.user_message()
            )
        }
        GitStatePushOutcome::AcceptedByGitServer => {
            transaction
                .publish_state_to_remaining_relays(&mut ops, &my_write_relays, repo_relay_only)
                .await?;
            if !transaction.state_relay_accepted() {
                bail!(
                    "{}",
                    StateTransactionFailure::StateNotAcceptedByAnyRelay.user_message()
                );
            }
            // The commit point: the pushed state only now becomes the
            // authoritative cached state.
            transaction.commit(&mut ops).await?;
            record_accepted_push_refspecs(git_repo, "origin", &refspecs)
                .context("failed to update the origin remote-tracking ref after push")?;
            set_branch_upstream(git_repo, "origin", branch_name)?;
            println!("pushed {branch_name} branch and published repository state");
        }
    }
    Ok(())
}

/// Establish the state event built from the pre-existing `origin`'s
/// listing through a candidate state transaction: stage it on the grasp
/// relays, push the origin-derived refs to the repository's git servers
/// (fetching any objects missing locally first), fan the event out to
/// the remaining relays and cache it only after a git server and at
/// least one relay accepted it.
#[allow(clippy::too_many_lines)]
async fn publish_origin_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    signer: &Arc<ngit::signer::NgitSigner>,
    nostr_url_decoded: &NostrUrlDecoded,
    origin_state: HashMap<String, String>,
) -> Result<()> {
    let term = Term::stderr();

    // Ordered after the newest cached state event across maintainer
    // coordinates. Normally none exists in this branch — a cached event
    // routes through the in-process sync instead — but the ordering
    // discipline keeps an unexpected predecessor authoritative.
    let old_state_event = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![get_filter_state_events(&repo_ref.coordinates(), true)],
    )
    .await
    .ok()
    .and_then(|events| event_ordering::latest_event(&events).cloned());

    let candidate = RepoState::build(
        repo_ref.identifier.clone(),
        origin_state,
        signer,
        old_state_event.as_ref(),
    )
    .await?;

    // Git-server reality must come from a same-invocation listing;
    // refs/remotes/* may hold stale values from the pre-nostr origin.
    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        nostr_url_decoded,
        None,
    )
    .await;

    // Fetch state objects missing locally from whichever listed server
    // has them, so they can be pushed to the servers that don't.
    let missing_refs =
        super::sync::fetch_missing_refs(git_repo, &candidate, &remote_states, nostr_url_decoded);

    // Plans source branch pushes from the candidate's own oids: the
    // nostr remote has no tracking refs yet.
    let (per_server_plans, state_refspecs) = super::sync::build_state_push_plans(
        git_repo,
        &super::sync::BranchPushSource::StateOids,
        &candidate.state,
        &remote_states,
        None,
        &missing_refs,
        &HashSet::new(),
    );

    // Match the `ngit sync` (without --force) that init used to spawn
    // here: grasp servers are realigned to the state, vanilla servers
    // stay fast-forward-only.
    let force_policy = ServerForcePolicy::ForceOnlyOn(
        remote_states
            .iter()
            .filter_map(|(url, (_, is_grasp_server))| {
                if *is_grasp_server {
                    Some(url.clone())
                } else {
                    None
                }
            })
            .collect(),
    );

    let repo_relay_only = git_repo
        .get_git_config_item("nostr.repo-relay-only", None)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true");
    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        user_ref.relays.write()
    };

    let mut ops = LiveOps {
        client,
        git_repo,
        term: &term,
        git_server_push_options: &[],
        decoded_nostr_url: nostr_url_decoded,
    };
    let mut transaction =
        StateTransaction::new(repo_ref, Some(candidate)).with_force_policy(force_policy);
    transaction.publish_state_to_grasps_first(&mut ops).await;
    match transaction.push_git_state_refspecs(&mut ops, per_server_plans, &state_refspecs) {
        GitStatePushOutcome::NoEligibleServers => {
            bail!(
                "{}",
                StateTransactionFailure::NoEligibleGitServers.user_message()
            )
        }
        GitStatePushOutcome::AllPushesFailed => {
            bail!(
                "{}",
                StateTransactionFailure::AllGitServerPushesFailed.user_message()
            )
        }
        GitStatePushOutcome::AcceptedByGitServer => {
            transaction
                .publish_state_to_remaining_relays(&mut ops, &my_write_relays, repo_relay_only)
                .await?;
            if !transaction.state_relay_accepted() {
                bail!(
                    "{}",
                    StateTransactionFailure::StateNotAcceptedByAnyRelay.user_message()
                );
            }
            // The commit point: the origin-derived state only now
            // becomes the authoritative cached state.
            transaction.commit(&mut ops).await?;
            println!("published repository state from the existing origin's refs");
        }
    }

    if !missing_refs.is_empty() {
        println!(
            "skipped the following refs as could not find them locally or on any git servers: {}",
            join_with_and(&missing_refs)
        );
    }
    Ok(())
}

#[cfg(test)]
mod git_server_url_validation_tests {
    use super::validate_git_server_url;

    #[test]
    fn accepts_urls_dispatched_by_git_remote_helpers() {
        for url in [
            "htree://npub1example/project",
            "ext::%S /tmp/project.git",
            "custom+git://example.test/project",
        ] {
            assert_eq!(validate_git_server_url(url).unwrap(), url);
        }
    }

    #[test]
    fn retains_builtin_url_validation() {
        assert!(validate_git_server_url("https://example.test/project.git").is_ok());
        assert!(validate_git_server_url("not a git URL").is_err());
    }

    #[test]
    fn rejects_unsafe_or_reserved_helper_schemes() {
        for url in [
            "nostr://npub1example/project",
            "NoStR::npub1example/project",
            "fd::0,1/project",
            "ws://relay.example.com",
            "WSS://relay.example.com",
        ] {
            assert!(validate_git_server_url(url).is_err(), "{url}");
        }
    }
}
