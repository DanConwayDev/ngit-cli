use std::collections::HashMap;

use anyhow::{Context, Result};
use ngit::cli_interactor::PromptConfirmParms;
use nostr::{nips::nip01::Coordinate, FromBech32, PublicKey, ToBech32};
use nostr_sdk::{Kind, RelayUrl};

use crate::{
    cli::{extract_signer_cli_arguments, Cli},
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::{fetching_with_report, get_repo_ref_from_cache, send_events, Client, Connect},
    git::{nostr_url::convert_clone_url_to_https, Repo, RepoActions},
    login,
    repo_ref::{
        extract_pks, get_repo_config_from_yaml, save_repo_config_to_yaml,
        try_and_get_repo_coordinates, RepoRef,
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

    let mut client = Client::default();

    let repo_coordinates = if let Ok(repo_coordinates) =
        try_and_get_repo_coordinates(&git_repo, &client, false).await
    {
        Some(repo_coordinates)
    } else {
        None
    };

    let repo_ref = if let Some(repo_coordinates) = repo_coordinates.clone() {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
        if let Ok(repo_ref) = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await
        {
            Some(repo_ref)
        } else {
            None
        }
    } else {
        None
    };

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        Some(&client),
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
                } else if let Some(repo_coordinates) = repo_coordinates.clone() {
                    if let Some(coordinate) = repo_coordinates.iter().next() {
                        coordinate.identifier.clone()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }),
        )?,
    };

    let identifier = match &args.identifier {
        Some(t) => t.clone(),
        None => Interactor::default().input(
            PromptInputParms::default()
                .with_prompt(
                    "repo identifier (typically the short name with hypens instead of spaces)",
                )
                .with_default(if let Some(repo_ref) = &repo_ref {
                    repo_ref.identifier.clone()
                } else if let Some(repo_coordinates) = repo_coordinates.clone() {
                    if let Some(coordinate) = repo_coordinates.iter().next() {
                        coordinate.identifier.clone()
                    } else {
                        String::new()
                    }
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

    let maintainers: Vec<PublicKey> = {
        let mut dont_ask = !args.other_maintainers.is_empty();
        let mut maintainers_string = if !args.other_maintainers.is_empty() {
            [args.other_maintainers.clone()].concat().join(" ")
        } else if repo_ref.is_none() && repo_config_result.is_err() {
            user_ref.public_key.to_bech32()?
        } else {
            let maintainers = if let Ok(config) = &repo_config_result {
                config.maintainers.clone()
            } else if let Some(repo_ref) = &repo_ref {
                repo_ref
                    .maintainers
                    .clone()
                    .iter()
                    .map(|k| k.to_bech32().unwrap())
                    .collect()
            } else {
                //unreachable
                vec![user_ref.public_key.to_bech32()?]
            };
            // add current user if not present
            if maintainers.iter().any(|m| {
                if let Ok(m_pubkey) = PublicKey::from_bech32(m) {
                    user_ref.public_key.eq(&m_pubkey)
                } else {
                    false
                }
            }) {
                maintainers.join(" ")
            } else {
                [maintainers, vec![user_ref.public_key.to_bech32()?]]
                    .concat()
                    .join(" ")
            }
        };
        'outer: loop {
            if !dont_ask && user_ref.public_key.to_bech32()?.eq(&maintainers_string) {
                if Interactor::default().confirm(
                    PromptConfirmParms::default()
                        .with_prompt("are you the only maintainer?")
                        .with_default(true),
                )? {
                    dont_ask = true;
                } else {
                    let mut opt_out_default = false;
                    if !Interactor::default().confirm(
                        PromptConfirmParms::default()
                            .with_prompt("are the other maintainers on nostr?")
                            .with_default(true),
                    )? {
                        opt_out_default = true;
                        dont_ask = true;
                    }
                    println!(
                        "nostr can reduce the trust placed in git servers by storing the state of git branches and tags. if you have other maintainers not using git via nostr, the verifiable state can fall behind the git server."
                    );

                    if Interactor::default().confirm(
                        PromptConfirmParms::default()
                            .with_prompt("opt-out of storing git state on nostr and relay on git server for now? you will still receive PRs and issues via nostr")
                            .with_default(true),
                    )? {
                        git_repo.save_git_config_item("nostr.nostate", "true", opt_out_default)?;
                    }
                }
            }
            if !dont_ask {
                println!("{}", &maintainers_string);
                maintainers_string = Interactor::default().input(
                    PromptInputParms::default()
                        .with_prompt("maintainers - space seperated list of npubs")
                        .with_default(maintainers_string),
                )?;
            }
            let mut maintainers: Vec<PublicKey> = vec![];
            for m in maintainers_string.split(' ') {
                if let Ok(m_pubkey) = PublicKey::from_bech32(m) {
                    maintainers.push(m_pubkey);
                } else {
                    println!("not a valid set of space seperated npubs");
                    dont_ask = false;
                    continue 'outer;
                }
            }
            // add current user incase removed
            if !maintainers.iter().any(|m| user_ref.public_key.eq(m)) {
                maintainers.push(user_ref.public_key);
            }
            break maintainers;
        }
    };

    let git_server = if args.clone_url.is_empty() {
        let no_state = if let Ok(Some(s)) = git_repo.get_git_config_item("nostr.nostate", None) {
            s == "true"
        } else {
            false
        };
        if no_state {
            println!(
                "you have opted out of storing git state on nostr, so a git server must be used for the state of authoritative branches, tags and related git objects."
            );
        } else {
            println!(
                "your repository state will be stored on nostr, but a git server is still required to store the git objects associated with this state."
            );
        }
        println!(
            "you can change this git server at any time and even configure multiple servers for redundancy. In this case, the git plugin will push to all of them when using the nostr remote."
        );
        println!("only maintainers need write access as PRs are sent over nostr.");
        println!(
            "a lightweight git server implementation for use with nostr, requiring no signup, is in development. several providers have shown interest in hosting it. for now use github, codeberg, or self-hosted song, forge, etc."
        );
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("git server remote url(s) (space seperated)")
                    .with_default(if let Some(repo_ref) = &repo_ref {
                        repo_ref.git_server.clone().join(" ")
                    } else if let Ok(url) = git_repo.get_origin_url() {
                        if let Ok(fetch_url) = convert_clone_url_to_https(&url) {
                            fetch_url
                        } else if url.starts_with("nostr://") {
                            // nostr added as origin remote before repo announcement sent
                            String::new()
                        } else {
                            // local repo or custom protocol
                            url
                        }
                    } else {
                        String::new()
                    }),
            )?
            .split(' ')
            .map(std::string::ToString::to_string)
            .collect()
    } else {
        args.clone_url.clone()
    };

    // TODO: when NIP-66 is functional, use this to reccommend relays and filter out
    //       relays that won't accept contributors events. NIP-11 'limitations'
    //       isn't widely used enough to be usedful.

    let relays: Vec<RelayUrl> = {
        let mut default = if let Ok(config) = &repo_config_result {
            config.relays.clone()
        } else if let Some(repo_ref) = &repo_ref {
            repo_ref
                .relays
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<String>>()
        } else if user_ref.relays.read().is_empty() {
            client.get_fallback_relays().clone()
        } else {
            user_ref.relays.read().clone()
        }
        .join(" ");
        'outer: loop {
            let relays: Vec<String> = if args.relays.is_empty() {
                Interactor::default()
                    .input(
                        PromptInputParms::default()
                            .with_prompt("relays")
                            .with_default(default),
                    )?
                    .split(' ')
                    .map(std::string::ToString::to_string)
                    .collect()
            } else {
                args.relays.clone()
            };
            let mut relay_urls = vec![];
            for r in &relays {
                if let Ok(r) = RelayUrl::parse(r) {
                    relay_urls.push(r);
                } else {
                    eprintln!("{r} is not a valid relay url");
                    default = relays.join(" ");
                    continue 'outer;
                }
            }
            break relay_urls;
        }
    };

    let web: Vec<String> = if args.web.is_empty() {
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("repo website")
                    .optional()
                    .with_default(if let Some(repo_ref) = &repo_ref {
                        repo_ref.web.clone().join(" ")
                    } else {
                        format!("https://gitworkshop.dev/repo/{}", &identifier)
                    }),
            )?
            .split(' ')
            .map(std::string::ToString::to_string)
            .collect()
    } else {
        args.web.clone()
    };

    let earliest_unique_commit = match &args.earliest_unique_commit {
        Some(t) => t.clone(),
        None => {
            let mut earliest_unique_commit = if let Some(repo_ref) = &repo_ref {
                repo_ref.root_commit.clone()
            } else {
                root_commit.to_string()
            };
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

    println!("publishing repostory reference...");

    let repo_ref = RepoRef {
        identifier: identifier.clone(),
        name,
        description,
        root_commit: earliest_unique_commit,
        git_server,
        web,
        relays: relays.clone(),
        maintainers: maintainers.clone(),
        events: HashMap::new(),
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

    git_repo.save_git_config_item(
        "nostr.repo",
        &Coordinate {
            kind: Kind::GitRepoAnnouncement,
            public_key: user_ref.public_key,
            identifier: identifier.clone(),
            relays: vec![],
        }
        .to_bech32()?,
        false,
    )?;

    let relays = relays
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<String>>();

    // no longer create a new maintainers.yaml file - its too confusing for users
    // as it falls out of sync with data in nostr event . update if it already
    // exists
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
