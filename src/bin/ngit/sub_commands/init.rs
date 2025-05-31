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
use dialoguer::theme::{ColorfulTheme, Theme};
use ngit::{
    UrlWithoutSlash,
    cli_interactor::{PromptChoiceParms, PromptConfirmParms, PromptMultiChoiceParms},
    client::{Params, send_events},
    git::nostr_url::{CloneUrl, NostrUrlDecoded},
    repo_ref::{
        detect_existing_ngit_relays, extract_npub, extract_pks, normalize_ngit_relay_url,
        save_repo_config_to_yaml,
    },
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
            client.get_fallback_relays().clone()
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

    let fallback_ngit_relays =
        if let Ok(Some(s)) = git_repo.get_git_config_item("nostr.ngit-relay-default-set", None) {
            s.split(';')
                .filter_map(|url| normalize_ngit_relay_url(url).ok()) // Attempt to parse and filter out errors
                .collect()
        } else {
            vec!["relay.ngit.dev".to_string(), "gitnostr.com".to_string()]
        };

    let selected_ngit_relays = if has_server_and_relay_flags {
        // ignore so a script running `ngit init` can contiue without prompts
        vec![]
    } else {
        let mut options: Vec<String> = detect_existing_ngit_relays(
            repo_ref.as_ref(),
            &args.relays,
            &args.clone_url,
            &args.blossoms,
            &identifier,
        );
        let mut selections: Vec<bool> = vec![true; options.len()]; // Initialize selections based on existing options
        let empty = options.is_empty();
        for fallback in fallback_ngit_relays {
            // Check if any option contains the fallback as a substring
            if !options.iter().any(|option| option.contains(&fallback)) {
                options.push(fallback.clone()); // Add fallback if not found
                selections.push(empty); // mark as selected if no existing ngit relay otherwise not
            }
        }
        let selected = multi_select_with_custom_value(
            "ngit-relays (ideally use between 2-4)",
            "ngit-relay",
            options,
            selections,
            normalize_ngit_relay_url,
        )?;
        show_multi_input_prompt_success("ngit-relays", &selected);
        selected
    };

    // ensure ngit relays are added as git server, relay and blossom entries
    for ngit_relay in &selected_ngit_relays {
        if args.clone_url.is_empty() {
            let clone_url =
                format_ngit_relay_url_as_clone_url(ngit_relay, &user_ref.public_key, &identifier)?;
            if !git_server_defaults.contains(&clone_url) {
                git_server_defaults.push(clone_url);
            }
        }
        if args.relays.is_empty() {
            let relay_url = format_ngit_relay_url_as_relay_url(ngit_relay)?;
            if !relay_defaults.contains(&relay_url) {
                relay_defaults.push(relay_url);
            }
        }
        if args.blossoms.is_empty() {
            let blossom = format_ngit_relay_url_as_blossom_url(ngit_relay)?;
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
        // TODO check if ngit-relays in use and if so turn this off:
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
        let ngit_relay_git_servers: Vec<String> = git_server_defaults
            .iter()
            .filter(|s| selected_ngit_relays.iter().any(|r| s.contains(r)))
            .cloned()
            .collect();
        let mut additional_server_options: Vec<String> = git_server_defaults
            .iter()
            .filter(|s| ngit_relay_git_servers.iter().any(|r| s.eq(&r)))
            .cloned()
            .collect();

        if simple_mode && !selected_ngit_relays.is_empty() {
            if additional_server_options.is_empty() {
                // additional git servers were listed
                let selected = loop {
                    let selections: Vec<bool> = vec![true; additional_server_options.len()];
                    let selected = multi_select_with_custom_value(
                        "additional git server(s) on top of ngit-relays",
                        "git server remote url",
                        additional_server_options,
                        selections,
                        |s| {
                            CloneUrl::from_str(s)
                                .map(|_| s.to_string())
                                .context(format!("Invalid git server URL format: {s}"))
                        },
                    )?;

                    if !selected.is_empty() || Interactor::default().choice(
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
                show_multi_input_prompt_success("git servers", &selected);
                let mut combined = ngit_relay_git_servers;
                combined.extend(selected);
                combined
            } else {
                git_server_defaults
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
            let formatted_selected_ngit_relays: Vec<String> = selected_ngit_relays
                .iter()
                .filter_map(|r| format_ngit_relay_url_as_relay_url(r).ok())
                .collect();
            let mut options: Vec<String> = relay_defaults
                .iter()
                .filter(|s| {
                    !formatted_selected_ngit_relays
                        .iter()
                        .any(|r| s.as_str() == r)
                })
                .cloned()
                .collect();

            let mut selections: Vec<bool> = vec![true; options.len()];

            // add fallback relays as options
            for relay in client.get_fallback_relays().clone() {
                if !options.iter().any(|r| r.contains(&relay))
                    && !formatted_selected_ngit_relays
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
                formatted_selected_ngit_relays
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
                        format_ngit_relay_url_as_blossom_url(s)
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

    if selected_ngit_relays.is_empty() && git_server.iter().any(|s| s.contains("github.com") || s.contains("codeberg.org")) && Interactor::default().confirm(
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
        user: None,
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
        trusted_maintainer: user_ref.public_key,
        maintainers_without_annoucnement: None,
        maintainers: maintainers.clone(),
        events: HashMap::new(),
        nostr_git_url: None,
    };
    let repo_event = repo_ref.to_event(&signer).await?;

    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![repo_event],
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
    let nostr_url = repo_ref.to_nostr_git_url(&Some(&git_repo)).to_string();

    if git_repo.git_repo.find_remote("origin").is_ok() {
        git_repo.git_repo.remote_set_url("origin", &nostr_url)?;
    } else {
        git_repo.git_repo.remote("origin", &nostr_url)?;
    }
    println!("set remote origin to nostr url");

    if std::env::var("NGITTEST").is_err() {
        // ignore during tests as git-remote-nostr isn't installed during ngit binary
        // tests

        if selected_ngit_relays.is_empty() {
            println!("running `git push` to publish your repository data");
        } else {
            let countdown_start = 5;
            println!(
                "waiting {countdown_start}s for ngit-relay servers to create your repo before we push your data"
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

fn multi_select_with_custom_value<F>(
    prompt: &str,
    custom_choice_prompt: &str,
    mut choices: Vec<String>,
    mut defaults: Vec<bool>,
    validate_choice: F,
) -> Result<Vec<String>>
where
    F: Fn(&str) -> Result<String>,
{
    let mut selected_choices = vec![];

    // Loop to allow users to add more choices
    loop {
        // Add 'add another' option at the end of the choices
        let mut current_choices = choices.clone();
        current_choices.push(if current_choices.is_empty() {
            "add".to_string()
        } else {
            "add another".to_string()
        });

        // Create default selections based on the provided defaults
        let mut current_defaults = defaults.clone();
        current_defaults.push(current_choices.len() == 1); // 'add another' should not be selected by default

        // Prompt for selections
        let selected_indices: Vec<usize> = Interactor::default().multi_choice(
            PromptMultiChoiceParms::default()
                .with_prompt(prompt)
                .dont_report()
                .with_choices(current_choices.clone())
                .with_defaults(current_defaults),
        )?;

        // Collect selected choices
        selected_choices.clear(); // Clear previous selections to update
        for &index in &selected_indices {
            if index < choices.len() {
                // Exclude 'add another' option
                selected_choices.push(choices[index].clone());
            }
        }

        // Check if 'add another' was selected
        if selected_indices.contains(&(choices.len())) {
            // Last index is 'add another'
            let mut new_choice: String;
            loop {
                new_choice = Interactor::default().input(
                    PromptInputParms::default()
                        .with_prompt(custom_choice_prompt)
                        .dont_report()
                        .optional(),
                )?;

                if new_choice.is_empty() {
                    break;
                }
                // Validate the new choice
                match validate_choice(&new_choice) {
                    Ok(valid_choice) => {
                        new_choice = valid_choice; // Use the fixed version of the input
                        break; // Valid choice, exit the loop
                    }
                    Err(err) => {
                        // Inform the user about the validation error
                        println!("Error: {err}");
                    }
                }
            }

            // Add the new choice to the choices vector
            if !new_choice.is_empty() {
                choices.push(new_choice.clone()); // Add new choice to the end of the list
                selected_choices.push(new_choice); // Automatically select the new choice
                defaults.push(true); // Set the new choice as selected by default
            }
        } else {
            // Exit the loop if 'add another' was not selected
            break;
        }
    }

    Ok(selected_choices)
}

fn format_ngit_relay_url_as_clone_url(
    url: &str,
    public_key: &PublicKey,
    identifier: &str,
) -> Result<String> {
    let ngit_relay_url = normalize_ngit_relay_url(url)?;
    if ngit_relay_url.contains("http://") {
        return Ok(format!(
            "{ngit_relay_url}/{}/{identifier}.git",
            public_key.to_bech32()?
        ));
    }
    Ok(format!(
        "https://{ngit_relay_url}/{}/{identifier}.git",
        public_key.to_bech32()?
    ))
}

fn format_ngit_relay_url_as_relay_url(url: &str) -> Result<String> {
    let ngit_relay_url = normalize_ngit_relay_url(url)?;
    if ngit_relay_url.contains("http://") {
        return Ok(ngit_relay_url.replace("http://", "ws://"));
    }
    Ok(format!("wss://{ngit_relay_url}"))
}

fn format_ngit_relay_url_as_blossom_url(url: &str) -> Result<String> {
    let ngit_relay_url = normalize_ngit_relay_url(url)?;
    if ngit_relay_url.contains("http://") {
        return Ok(ngit_relay_url);
    }
    Ok(format!("https://{ngit_relay_url}"))
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

pub fn show_multi_input_prompt_success(label: &str, values: &[String]) {
    let values_str: Vec<&str> = values.iter().map(std::string::String::as_str).collect();
    eprintln!("{}", {
        let mut s = String::new();
        let _ = ColorfulTheme::default().format_multi_select_prompt_selection(
            &mut s,
            label,
            &values_str,
        );
        s
    });
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
