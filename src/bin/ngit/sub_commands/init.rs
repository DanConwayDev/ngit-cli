use std::{
    collections::HashMap,
    env,
    process::{Command, Stdio},
    str::FromStr,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use console::{Style, Term};
use git2::Oid;
use ngit::{
    UrlWithoutSlash,
    cli_interactor::{
        PromptChoiceParms, PromptConfirmParms, multi_select_with_custom_value,
        show_multi_input_prompt_success,
    },
    client::{Params, get_state_from_cache, send_events},
    fetch::fetch_from_git_server,
    git::nostr_url::{CloneUrl, NostrUrlDecoded},
    list::list_from_remote,
    repo_ref::{
        detect_existing_grasp_servers, extract_npub, extract_pks,
        format_grasp_server_url_as_relay_url, is_grasp_server_clone_url,
        normalize_grasp_server_url, save_repo_config_to_yaml,
    },
    repo_state::RepoState,
};
use nostr::{
    FromBech32, PublicKey, ToBech32,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};
use nostr_sdk::{Kind, RelayUrl, Url};

use crate::{
    cli::{Cli, extract_signer_cli_arguments},
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions, nostr_url::convert_clone_url_to_https},
    login,
    repo_ref::{
        RepoRef, get_repo_config_from_yaml, try_and_get_repo_coordinates_when_remote_unknown,
    },
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[clap(short, long)]
    /// name of repository
    title: Option<String>,
    #[clap(short, long)]
    /// optional description
    description: Option<String>,
    #[clap(long)]
    /// git server url users can clone from
    clone_url: Vec<String>,
    #[clap(short, long, value_parser, num_args = 1..)]
    /// homepage
    web: Vec<String>,
    #[clap(short, long, value_parser, num_args = 1..)]
    /// relays contributors push patches and comments to
    relays: Vec<String>,
    #[clap(short, long, value_parser, num_args = 1..)]
    /// blossom servers
    blossoms: Vec<String>,
    #[clap(short, long, value_parser, num_args = 1..)]
    /// npubs of other maintainers
    other_maintainers: Vec<String>,
    #[clap(long)]
    /// usually root commit but will be more recent commit for forks
    earliest_unique_commit: Option<String>,
    #[clap(short, long)]
    /// shortname with no spaces or special characters
    identifier: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    // TODO: check for empty repo
    // TODO: check for existing maintaiers file

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinate = (try_and_get_repo_coordinates_when_remote_unknown(&git_repo).await).ok();

    let repo_ref = if let Some(repo_coordinate) = &repo_coordinate {
        fetching_with_report(git_repo_path, &client, repo_coordinate).await?;
        (get_repo_ref_from_cache(Some(git_repo_path), repo_coordinate).await).ok()
    } else {
        None
    };

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        Some(&client),
        true,
    )
    .await?;

    let repo_config_result = get_repo_config_from_yaml(&git_repo);
    // TODO: check for other claims

    let name = match &args.title {
        Some(t) => t.clone(),
        None => Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo name")
                .with_default(if let Some(repo_ref) = &repo_ref {
                    repo_ref.name.clone()
                } else if let Some(coordinate) = &repo_coordinate {
                    coordinate.identifier.clone()
                } else if let Ok(path) = env::current_dir() {
                    if let Some(current_dir_name) = path.file_name() {
                        current_dir_name.to_string_lossy().to_string()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }),
        )?,
    };

    let description = match &args.description {
        Some(t) => t.clone(),
        None => Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo description (one sentance)")
                .optional()
                .with_default(if let Some(repo_ref) = &repo_ref {
                    repo_ref.description.clone()
                } else {
                    String::new()
                }),
        )?,
    };

    // this is important so init can be completed done without prompts
    let has_server_and_relay_flags = !args.clone_url.is_empty() && !args.relays.is_empty();

    let simple_mode = if has_server_and_relay_flags {
        false
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

    let identifier_default = if let Some(repo_ref) = &repo_ref {
        repo_ref.identifier.clone()
    } else if let Some(repo_coordinate) = &repo_coordinate {
        repo_coordinate.identifier.clone()
    } else {
        let fallback = name
            .clone()
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
        if let Ok(config) = &repo_config_result {
            if let Some(identifier) = &config.identifier {
                identifier.to_string()
            } else {
                fallback
            }
        } else {
            fallback
        }
    };

    let identifier = match &args.identifier {
        Some(t) => t.clone(),
        None => {
            if simple_mode {
                identifier_default
            } else {
                Interactor::default().input(
                PromptInputParms::default()
                    .with_prompt(
                        "repo identifier (typically the short name with hypens instead of spaces)",
                    )
                    .with_default(identifier_default),
            )?
            }
        }
    };

    let mut git_server_defaults: Vec<String> = if !args.clone_url.is_empty() {
        args.clone_url.clone()
    } else if let Some(repo_ref) = &repo_ref {
        // TODO dont default to git servers of other maintainers (?)
        repo_ref.git_server.clone()
    } else if let Ok(url) = git_repo.get_origin_url() {
        if let Ok(fetch_url) = convert_clone_url_to_https(&url) {
            vec![fetch_url]
        } else if url.starts_with("nostr://") {
            // nostr added as origin remote before repo announcement sent
            vec![]
        } else {
            // local repo or custom protocol
            vec![url]
        }
    } else {
        vec![]
    };

    let mut relay_defaults = if args.relays.is_empty() {
        if let Ok(config) = &repo_config_result {
            config.relays.clone()
        } else if let Some(repo_ref) = &repo_ref {
            repo_ref
                .relays
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<String>>()
        } else {
            client.get_relay_default_set().clone()
        }
    } else {
        args.relays.clone()
    };

    let mut blossoms_defaults = if args.blossoms.is_empty() {
        if let Some(repo_ref) = &repo_ref {
            repo_ref
                .blossoms
                .iter()
                .map(UrlWithoutSlash::to_string_without_trailing_slash)
                .collect::<Vec<String>>()
        // } else if user_ref.blossoms.read().is_empty() {
        //     client.get_fallback_relays().clone()
        } else {
            vec![]
            // user_ref.relays.read().clone()
        }
    } else {
        args.blossoms.clone()
    };

    let fallback_grasp_servers = client.get_grasp_default_set();

    let selected_grasp_servers = if has_server_and_relay_flags {
        // ignore so a script running `ngit init` can contiue without prompts
        vec![]
    } else {
        let mut options: Vec<String> = detect_existing_grasp_servers(
            repo_ref.as_ref(),
            &args.relays,
            &args.clone_url,
            &identifier,
        );
        let mut selections: Vec<bool> = vec![true; options.len()]; // Initialize selections based on existing options
        let empty = options.is_empty();
        for user_grasp_option in user_ref.grasp_list.urls {
            // Check if any option contains the user_grasp_option as a substring
            if !options
                .iter()
                .any(|option| option.contains(user_grasp_option.as_str()))
            {
                options.push(user_grasp_option.to_string()); // Add if not found
                selections.push(empty); // mark as selected if no existing grasp otherwise not
            }
        }

        let empty = options.is_empty();
        for fallback in fallback_grasp_servers {
            // Check if any option contains the fallback as a substring
            if !options.iter().any(|option| option.contains(fallback)) {
                options.push(fallback.clone()); // Add fallback if not found
                selections.push(empty); // mark as selected if no existing selections otherwise not
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
        selected
    };

    // ensure ngit relays are added as git server, relay and blossom entries
    for grasp_server in &selected_grasp_servers {
        if args.clone_url.is_empty() {
            let clone_url = format_grasp_server_url_as_clone_url(
                grasp_server,
                &user_ref.public_key,
                &identifier,
            )?;

            let grasp_server_clone_root = if clone_url.contains("https://") {
                format!("https://{grasp_server}")
            } else {
                grasp_server.to_string()
            };

            // Find all positions of entries containing the relay root
            let matching_positions: Vec<usize> = git_server_defaults
                .iter()
                .enumerate()
                .filter_map(|(idx, url)| {
                    if url.contains(&grasp_server_clone_root) {
                        Some(idx)
                    } else {
                        None
                    }
                })
                .collect();

            // If we found any matches
            if matching_positions.is_empty() {
                // No existing entries found, so add a new one
                git_server_defaults.push(clone_url);
            } else {
                // Replace the first occurrence
                git_server_defaults[matching_positions[0]] = clone_url;

                // Remove any subsequent occurrences (in reverse order to avoid index issues)
                for &position in matching_positions.iter().skip(1).rev() {
                    git_server_defaults.remove(position);
                }
            }
        }
        if args.relays.is_empty() {
            let relay_url = format_grasp_server_url_as_relay_url(grasp_server)?;
            if !relay_defaults.contains(&relay_url) {
                relay_defaults.push(relay_url);
            }
        }
        if args.blossoms.is_empty() {
            let blossom = format_grasp_server_url_as_blossom_url(grasp_server)?;
            if !blossoms_defaults.contains(&blossom) {
                blossoms_defaults.push(blossom);
            }
        }
    }

    let no_state = if let Ok(Some(s)) = git_repo.get_git_config_item("nostr.nostate", None) {
        s == "true"
    } else {
        false
    };
    if no_state
        && Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("store state on nostr? required for nostr-permissioned git servers")
                .with_default(true),
        )?
    {
        // TODO check if grasp servers in use and if so turn this off:
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

    let git_server = if args.clone_url.is_empty() {
        let grasp_server_git_servers: Vec<String> = git_server_defaults
            .iter()
            .filter(|s| is_grasp_server_clone_url(s))
            .cloned()
            .collect();
        let mut additional_server_options: Vec<String> = git_server_defaults
            .iter()
            .filter(|s| !is_grasp_server_clone_url(s))
            .cloned()
            .collect();

        if simple_mode && !selected_grasp_servers.is_empty() {
            if additional_server_options.is_empty() {
                git_server_defaults
            } else {
                // additional git servers were listed
                let selected = loop {
                    let selections: Vec<bool> = vec![true; additional_server_options.len()];
                    let selected = multi_select_with_custom_value(
                        "additional git server(s) on top of grasp servers",
                        "git server remote url",
                        additional_server_options,
                        selections,
                        |s| {
                            CloneUrl::from_str(s)
                                .map(|_| s.to_string())
                                .context(format!("Invalid git server URL format: {s}"))
                        },
                    )?;

                    if selected.is_empty() || Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_prompt("if you or another maintainer start pushing directly to these, nostr will be out of date")
                        .dont_report()
                        .with_choices(vec![
                            "I'll always push to the nostr remote".to_string(),
                            "change setup".to_string(),
                        ])
                        .with_default(0),
                        )? == 1 {
                        additional_server_options = selected;
                        continue
                    }
                    break selected;
                };
                show_multi_input_prompt_success("additional git servers", &selected);
                let mut combined = grasp_server_git_servers;
                combined.extend(selected);
                combined
            }
        } else {
            // show all git servers
            let selections: Vec<bool> = vec![true; git_server_defaults.len()];

            let selected = multi_select_with_custom_value(
                "git server remote url(s)",
                "git server remote url",
                git_server_defaults,
                selections,
                |s| {
                    CloneUrl::from_str(s)
                        .map(|_| s.to_string())
                        .context(format!("Invalid git server URL format: {s}"))
                },
            )?;
            show_multi_input_prompt_success("git servers", &selected);
            selected
        }
    } else {
        git_server_defaults
    };

    let relays: Vec<RelayUrl> = {
        if simple_mode {
            let formatted_selected_grasp_servers: Vec<String> = selected_grasp_servers
                .iter()
                .filter_map(|r| format_grasp_server_url_as_relay_url(r).ok())
                .collect();
            let mut options: Vec<String> = relay_defaults
                .iter()
                .filter(|s| {
                    !formatted_selected_grasp_servers
                        .iter()
                        .any(|r| s.as_str() == r)
                })
                .cloned()
                .collect();

            let mut selections: Vec<bool> = vec![true; options.len()];

            // add fallback relays as options
            for relay in client.get_relay_default_set().clone() {
                if !options.iter().any(|r| r.contains(&relay))
                    && !formatted_selected_grasp_servers
                        .iter()
                        .any(|r| relay.contains(r))
                {
                    options.push(relay);
                    selections.push(selections.is_empty());
                }
            }

            let selected = multi_select_with_custom_value(
                "additional nostr relays on top of nostr-relays - 1 or 2 public relays are reccomended",
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
                formatted_selected_grasp_servers
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
            let selections: Vec<bool> = vec![true; relay_defaults.len()];
            if args.relays.is_empty() {
                let selected = multi_select_with_custom_value(
                    "nostr relays",
                    "nostr relay",
                    relay_defaults,
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
            } else {
                relay_defaults
                    .iter()
                    .filter_map(|r| parse_relay_url(r).ok())
                    .collect()
            }
        }
    };

    let blossoms: Vec<Url> = {
        if simple_mode || has_server_and_relay_flags {
            blossoms_defaults
                .iter()
                .filter_map(|b| Url::parse(b).ok())
                .collect()
        } else {
            let selections: Vec<bool> = vec![true; blossoms_defaults.len()];
            if args.blossoms.is_empty() {
                let selected = multi_select_with_custom_value(
                    "blossom servers",
                    "blossom server",
                    blossoms_defaults,
                    selections,
                    |s| {
                        format_grasp_server_url_as_blossom_url(s)
                            .context(format!("Invalid blossom URL format: {s}"))
                    },
                )?;
                show_multi_input_prompt_success("nostr relays", &selected);
                selected.iter().filter_map(|b| Url::parse(b).ok()).collect()
            } else {
                blossoms_defaults
                    .iter()
                    .filter_map(|b| Url::parse(b).ok())
                    .collect()
            }
        }
    };

    let default_maintainers = {
        let mut maintainers = vec![user_ref.public_key];
        if args.other_maintainers.is_empty() {
            if let Some(repo_ref) = &repo_ref {
                for m in &repo_ref.maintainers {
                    if !maintainers.contains(m) {
                        maintainers.push(*m);
                    }
                }
            }
        } else {
            for m in &args.other_maintainers {
                if let Ok(pubkey) = PublicKey::from_bech32(m).context("invalid npub") {
                    if !maintainers.contains(&pubkey) {
                        maintainers.push(pubkey);
                    }
                }
            }
        }
        maintainers
    };

    let maintainers: Vec<PublicKey> = if args.other_maintainers.is_empty() {
        if default_maintainers.len() == 1
            && Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_prompt("add other maintainers now?")
                    .dont_report()
                    .with_choices(vec![
                        "maybe later".to_string(),
                        "add maintainers".to_string(),
                    ])
                    .with_default(0),
            )? == 0
        {
            default_maintainers
        } else {
            let selections: Vec<bool> = vec![true; default_maintainers.len()];

            let selected = multi_select_with_custom_value(
                "maintainers",
                "maintainer npub",
                default_maintainers
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
        }
    } else {
        default_maintainers
    };

    if selected_grasp_servers.is_empty() && git_server.iter().any(|s| s.contains("github.com") || s.contains("codeberg.org")) && Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("you have listed github / codeberg. Are you or other maintainers planning on pushing directly to github / codeberg rather than using your shiny new nostr clone url which will do this for you?")
                .with_default(false),
        )? {
        println!("This means people using the nostr URL won't get your latest branch updates.");
        if Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("opt-out of storing git state on nostr and relay on github for now? you will still receive PRs and issues via nostr")
                .with_default(true),
        )? {
            git_repo.save_git_config_item("nostr.nostate", "true", false)?;
        }
    }

    let gitworkshop_url = NostrUrlDecoded {
        original_string: String::new(),
        coordinate: Nip19Coordinate {
            coordinate: Coordinate {
                public_key: user_ref.public_key,
                kind: Kind::GitRepoAnnouncement,
                identifier: identifier.clone(),
            },
            relays: if let Some(relay) = relays.first() {
                vec![relay.clone()]
            } else {
                vec![]
            },
        },
        protocol: None,
        ssh_key_file: None,
        nip05: None,
    }
    .to_string()
    .replace("nostr://", "https://gitworkshop.dev/");

    let web: Vec<String> = if args.web.is_empty() {
        let web_default = if let Some(repo_ref) = &repo_ref {
            if repo_ref
                .web
                .clone()
                .join(" ")
                // replace legacy gitworkshop.dev url format with new one
                .contains(format!("https://gitworkshop.dev/repo/{}", &identifier).as_str())
            {
                gitworkshop_url.clone()
            } else {
                repo_ref.web.clone().join(" ")
            }
        } else {
            gitworkshop_url.clone()
        };

        if simple_mode {
            web_default
        } else {
            Interactor::default().input(
                PromptInputParms::default()
                    .with_prompt("repo website")
                    .optional()
                    .with_default(web_default),
            )?
        }
        .split(' ')
        .map(std::string::ToString::to_string)
        .collect()
    } else {
        args.web.clone()
    };

    let earliest_unique_commit = if let Some(t) = &args.earliest_unique_commit {
        t.clone()
    } else {
        let mut earliest_unique_commit = if let Some(repo_ref) = &repo_ref {
            repo_ref.root_commit.clone()
        } else {
            root_commit.to_string()
        };
        if simple_mode {
            earliest_unique_commit
        } else {
            println!(
                "the earliest unique commit helps with discoverability. It defaults to the root commit. Only change this if your repo has completely forked off an has formed its own identity."
            );
            loop {
                earliest_unique_commit = Interactor::default().input(
                    PromptInputParms::default()
                        .with_prompt("earliest unique commit (to help with discoverability)")
                        .with_default(earliest_unique_commit.clone()),
                )?;
                if let Ok(exists) = git_repo.does_commit_exist(&earliest_unique_commit) {
                    if exists {
                        break earliest_unique_commit;
                    }
                    println!("commit does not exist on current repository");
                } else {
                    println!("commit id not formatted correctly");
                }
                if earliest_unique_commit.len().ne(&40) {
                    println!("commit id must be 40 characters long");
                }
            }
        }
    };

    println!("publishing repostory announcement to nostr...");

    let repo_ref = RepoRef {
        identifier: identifier.clone(),
        name,
        description,
        root_commit: earliest_unique_commit,
        git_server,
        web,
        relays: relays.clone(),
        blossoms,
        hashtags: if let Some(repo_ref) = repo_ref {
            repo_ref.hashtags
        } else {
            vec![]
        },
        trusted_maintainer: user_ref.public_key,
        maintainers_without_annoucnement: None,
        maintainers: maintainers.clone(),
        events: HashMap::new(),
        nostr_git_url: None,
    };
    let repo_event = repo_ref.to_event(&signer).await?;

    let nostr_url_decoded = repo_ref.to_nostr_git_url(&Some(&git_repo));

    let mut events = vec![repo_event];

    let (need_push, need_sync) = if std::env::var("NGITTEST").is_ok() || no_state {
        // dont push or sync during tests as git-remote-nostr isn't installed during
        // ngit binary tests
        (false, false)
    } else if let Ok(nostr_state) =
        &get_state_from_cache(Some(git_repo.get_path()?), &repo_ref).await
    {
        // issue fresh state event with same state to all (inc. new) repo relays
        let new_state_event = RepoState::build(
            repo_ref.identifier.clone(),
            nostr_state.state.clone(),
            &signer,
        )
        .await?
        .event;
        events.push(new_state_event);
        println!("publishing repostory state to nostr...");
        (false, true)
    } else if let Ok(remote) = git_repo.git_repo.find_remote("origin") {
        if let Some(url) = remote.url() {
            // issue a state event with origin state, to all (inc. new) repo relays
            if let Ok(mut origin_state) =
                list_from_remote(&Term::stdout(), &git_repo, url, &nostr_url_decoded, false)
            {
                origin_state.retain(|key, _| {
                    key.starts_with("refs/heads/")
                        || key.starts_with("refs/tags/")
                        || key.starts_with("HEAD")
                });
                let mut required_oids = vec![];
                for tip in origin_state.values() {
                    if let Ok(exist) = git_repo.does_commit_exist(tip) {
                        let oid_exists_as_tag = Oid::from_str(tip).is_ok_and(|tip| {
                            git_repo
                                .git_repo
                                .find_object(tip, Some(git2::ObjectType::Tag))
                                .is_ok()
                        });
                        if !exist && !oid_exists_as_tag {
                            required_oids.push(tip.clone());
                        }
                    }
                }
                if required_oids.is_empty() {
                    println!("fetching refs missing locally from existing origin...");
                    if let Err(error) = fetch_from_git_server(
                        &git_repo,
                        &required_oids,
                        url,
                        &nostr_url_decoded,
                        &Term::stdout(),
                        false,
                    ) {
                        println!("error fetching refs which will make ngit sync fail: {error}");
                    }
                }
                let new_state_event =
                    RepoState::build(repo_ref.identifier.clone(), origin_state, &signer)
                        .await?
                        .event;
                events.push(new_state_event);
                println!("publishing repostory state to nostr...");
                (false, true)
            } else {
                // cant reach existing origin so just try push
                (true, false)
            }
        } else {
            // origin never connected so just try push
            (true, false)
        }
    } else {
        // no origin so we need to just push
        (true, false)
    };

    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        events,
        user_ref.relays.write(),
        relays.clone(),
        !cli_args.disable_cli_spinners,
        false,
    )
    .await?;

    // TODO - does this git config item do more harm than good?
    git_repo.save_git_config_item(
        "nostr.repo",
        &Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: user_ref.public_key,
                identifier: identifier.clone(),
            },
            relays: vec![],
        }
        .to_bech32()?,
        false,
    )?;

    // set origin remote
    let nostr_url = nostr_url_decoded.to_string();

    if git_repo.git_repo.find_remote("origin").is_ok() {
        git_repo.git_repo.remote_set_url("origin", &nostr_url)?;
    } else {
        git_repo.git_repo.remote("origin", &nostr_url)?;
    }
    println!("set remote origin to nostr url");

    if need_push {
        if selected_grasp_servers.is_empty() {
            println!("running `ngit push` to publish your repository data");
        } else {
            let countdown_start = 5;
            println!(
                "waiting {countdown_start}s for grasp servers to create your repo before we push your data"
            );
            let term = Term::stdout();
            for i in (1..=countdown_start).rev() {
                term.write_line(format!("\rrunning `git push` in {i}s").as_str())?;
                thread::sleep(Duration::new(1, 0)); // Sleep for 1 second
                term.clear_last_lines(1)?;
            }
            term.flush().unwrap(); // Ensure the output is flushed to the terminal
        }

        if let Err(err) = push_main_or_master_branch(&git_repo) {
            println!(
                "your repository announcement was published to nostr but git push exited with an error: {err}"
            );
        }
    }
    if need_sync {
        if selected_grasp_servers.is_empty() {
            println!(
                "running `ngit sync` to ensure your repository data is available on repository git servers"
            );
        } else {
            let countdown_start = 5;
            println!(
                "waiting {countdown_start}s for any new grasp servers to create your repo before we sync your data"
            );
            let term = Term::stdout();
            for i in (1..=countdown_start).rev() {
                term.write_line(format!("\rrunning `ngit sync` in {i}s").as_str())?;
                thread::sleep(Duration::new(1, 0)); // Sleep for 1 second
                term.clear_last_lines(1)?;
            }
            term.flush().unwrap(); // Ensure the output is flushed to the terminal
        }

        if let Err(err) = run_ngit_sync() {
            println!(
                "your repository announcement was published to nostr but 'ngit sync' exited with an error: {err}"
            );
        }
    }

    // println!(
    //     "any remote branches beginning with `pr/` are open PRs from contributors.
    // they can submit these by simply pushing a branch with this `pr/` prefix."
    // );
    println!("share your repository: {gitworkshop_url}");
    println!("clone url: {nostr_url}");

    // no longer create a new maintainers.yaml file - its too confusing for users
    // as it falls out of sync with data in nostr event . update if it already
    // exists

    let relays = relays
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<String>>();
    if match &repo_config_result {
        Ok(config) => {
            !<std::option::Option<std::string::String> as Clone>::clone(&config.identifier)
                .unwrap_or_default()
                .eq(&identifier)
                || !extract_pks(config.maintainers.clone())?.eq(&maintainers)
                || !config.relays.eq(&relays)
        }
        Err(_) => false,
    } {
        let title_style = Style::new().bold().fg(console::Color::Yellow);
        println!("{}", title_style.apply_to("maintainers.yaml"));
        save_repo_config_to_yaml(
            &git_repo,
            identifier.clone(),
            maintainers.clone(),
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

fn format_grasp_server_url_as_clone_url(
    url: &str,
    public_key: &PublicKey,
    identifier: &str,
) -> Result<String> {
    let grasp_server_url = normalize_grasp_server_url(url)?;
    if grasp_server_url.contains("http://") {
        return Ok(format!(
            "{grasp_server_url}/{}/{identifier}.git",
            public_key.to_bech32()?
        ));
    }
    Ok(format!(
        "https://{grasp_server_url}/{}/{identifier}.git",
        public_key.to_bech32()?
    ))
}

fn format_grasp_server_url_as_blossom_url(url: &str) -> Result<String> {
    let grasp_server_url = normalize_grasp_server_url(url)?;
    if grasp_server_url.contains("http://") {
        return Ok(grasp_server_url);
    }
    Ok(format!("https://{grasp_server_url}"))
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

fn push_main_or_master_branch(git_repo: &Repo) -> Result<()> {
    let main_branch_name = {
        let local_branches = git_repo
            .get_local_branch_names()
            .context("failed to find any local branches")?;
        if local_branches.contains(&"main".to_string()) {
            "main"
        } else if local_branches.contains(&"master".to_string()) {
            "master"
        } else {
            bail!(
                "set remote origin to nostr url and tried to push main or master branch but they dont exist yet"
            )
        }
    };

    println!("========================================");
    println!("            GIT PUSH COMMAND            ");
    println!("========================================");

    let command = "git";
    let args = ["push", "origin", "-u", main_branch_name];

    // Spawn the process
    let mut child = Command::new(command)
        .args(args)
        .stdout(Stdio::inherit()) // Redirect stdout to the console
        .stderr(Stdio::inherit()) // Redirect stderr to the console
        .spawn()
        .context("Failed to start git push process")?;

    // Wait for the process to finish
    let exit_status = child.wait().context("Failed to start git push process")?;

    println!("========================================");
    println!("        END OF GIT PUSH OUTPUT");
    println!("========================================");

    // Check the exit status
    if exit_status.success() {
        Ok(())
    } else {
        bail!("git push process exited with an error: {}", exit_status);
    }
}

fn run_ngit_sync() -> Result<()> {
    println!("========================================");
    println!("            NGIT SYNC COMMAND            ");
    println!("========================================");

    let command = "ngit";
    let args = ["sync"];

    // Spawn the process
    let mut child = Command::new(command)
        .args(args)
        .stdout(Stdio::inherit()) // Redirect stdout to the console
        .stderr(Stdio::inherit()) // Redirect stderr to the console
        .spawn()
        .context("Failed to start ngit sync process")?;

    // Wait for the process to finish
    let exit_status = child.wait().context("Failed to start ngit sync process")?;

    println!("========================================");
    println!("        END OF NGIT SYNC OUTPUT");
    println!("========================================");

    // Check the exit status
    if exit_status.success() {
        Ok(())
    } else {
        bail!("ngit sync process exited with an error: {}", exit_status);
    }
}
