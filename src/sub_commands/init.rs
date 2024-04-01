use anyhow::{Context, Result};
use nostr::{FromBech32, PublicKey, ToBech32};

use super::send::send_events;
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::Connect,
    git::{Repo, RepoActions},
    login,
    repo_ref::{self, extract_pks, get_repo_config_from_yaml, save_repo_config_to_yaml, RepoRef},
    Cli,
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
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    // TODO: check for empty repo
    // TODO: check for existing maintaiers file

    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let (keys, user_ref) = login::launch(&cli_args.nsec, &cli_args.password, Some(&client)).await?;

    client.set_keys(&keys).await;

    let repo_ref = if let Ok(rep_ref) = repo_ref::fetch(
        &git_repo,
        root_commit.to_string(),
        &client,
        user_ref.relays.write(),
        false,
    )
    .await
    {
        Some(rep_ref)
    } else {
        None
    };

    let repo_config_result = get_repo_config_from_yaml(&git_repo);
    // TODO: check for other claims

    let name = match &args.title {
        Some(t) => t.clone(),
        None => Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("name")
                .with_default(if let Some(repo_ref) = &repo_ref {
                    repo_ref.name.clone()
                } else {
                    String::new()
                }),
        )?,
    };

    let identifier = match &args.identifier {
        Some(t) => t.clone(),
        None => Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("identifier")
                .with_default(if let Some(repo_ref) = &repo_ref {
                    repo_ref.identifier.clone()
                } else {
                    name.clone()
                        .replace(' ', "-")
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c.eq(&'/') {
                                c
                            } else {
                                '-'
                            }
                        })
                        .collect()
                }),
        )?,
    };

    let description = match &args.description {
        Some(t) => t.clone(),
        None => Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("description")
                .with_default(if let Some(repo_ref) = &repo_ref {
                    repo_ref.description.clone()
                } else {
                    String::new()
                }),
        )?,
    };

    let git_server = if args.clone_url.is_empty() {
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("clone url")
                    .with_default(if let Some(repo_ref) = &repo_ref {
                        repo_ref.git_server.clone().join(" ")
                    } else if let Ok(git_repo) = git_repo.get_origin_url() {
                        git_repo
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

    let web: Vec<String> = if args.web.is_empty() {
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("web")
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

    let maintainers: Vec<PublicKey> = {
        let mut dont_ask = !args.other_maintainers.is_empty();
        let mut maintainers_string = if !args.other_maintainers.is_empty() {
            [args.other_maintainers.clone()].concat().join(" ")
        } else if repo_ref.is_none() && repo_config_result.is_err() {
            keys.public_key().to_bech32()?
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
                vec![keys.public_key().to_bech32()?]
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
                [maintainers, vec![keys.public_key().to_bech32()?]]
                    .concat()
                    .join(" ")
            }
        };
        'outer: loop {
            if !dont_ask {
                maintainers_string = Interactor::default()
                    .input(
                        PromptInputParms::default()
                            .with_prompt("maintainers")
                            .with_default(maintainers_string),
                    )?
                    .split(' ')
                    .map(std::string::ToString::to_string)
                    .collect();
            }
            let mut maintainers: Vec<PublicKey> = vec![];
            for m in maintainers_string.split(' ') {
                if let Ok(m_pubkey) = PublicKey::from_bech32(m) {
                    maintainers.push(m_pubkey);
                } else {
                    println!("not a valid set of npubs seperated by a space");
                    dont_ask = false;
                    continue 'outer;
                }
            }
            // add current user incase removed
            if !maintainers.iter().any(|m| user_ref.public_key.eq(m)) {
                maintainers.push(keys.public_key());
            }
            break maintainers;
        }
    };

    // TODO: check if relays are free to post to so contributors can submit patches
    // TODO: recommend some reliable free ones
    let relays: Vec<String> = if args.relays.is_empty() {
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("relays")
                    .with_default(if let Ok(config) = &repo_config_result {
                        config.relays.clone().join(" ")
                    } else if let Some(repo_ref) = &repo_ref {
                        repo_ref.relays.clone().join(" ")
                    } else {
                        user_ref.relays.write().join(" ")
                    }),
            )?
            .split(' ')
            .map(std::string::ToString::to_string)
            .collect()
    } else {
        args.relays.clone()
    };

    let earliest_unique_commit = match &args.earliest_unique_commit {
        Some(t) => t.clone(),
        None => {
            let mut earliest_unique_commit = if let Some(repo_ref) = &repo_ref {
                repo_ref.root_commit.clone()
            } else {
                root_commit.to_string()
            };
            loop {
                earliest_unique_commit = Interactor::default().input(
                    PromptInputParms::default()
                        .with_prompt("earliest unique commit")
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

    let repo_event = RepoRef {
        identifier,
        name,
        description,
        root_commit: earliest_unique_commit,
        git_server,
        web,
        relays: relays.clone(),
        maintainers: maintainers.clone(),
    }
    .to_event(&keys)?;

    send_events(
        &client,
        vec![repo_event],
        user_ref.relays.write(),
        relays.clone(),
        !cli_args.disable_cli_spinners,
    )
    .await?;

    // if yaml file doesnt exist or needs updating
    if match &repo_config_result {
        Ok(config) => {
            !(extract_pks(config.maintainers.clone())?.eq(&maintainers)
                && config.relays.eq(&relays))
        }
        Err(_) => true,
    } {
        save_repo_config_to_yaml(&git_repo, maintainers.clone(), relays.clone())?;
        println!(
            "maintainers.yaml {}. commit and push.",
            if repo_config_result.is_err() {
                "created"
            } else {
                "updated"
            }
        );
        println!(
            "this optional file enables existing contributors to automatically fetch your repo event (instead of one from a pubkey pretending to be the maintainer)"
        );
    }
    Ok(())
}
