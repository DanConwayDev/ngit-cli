use anyhow::{Context, Result};

use super::prs::create::send_events;
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::Connect,
    git::{Repo, RepoActions},
    login,
    repo_ref::{extract_pks, get_repo_config_from_yaml, save_repo_config_to_yaml, RepoRef},
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
    #[clap(short, long, value_parser, num_args = 1..)]
    /// relays contributors push patches and comments to
    relays: Vec<String>,
}

pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    let (main_or_master_branch_name, _) = git_repo
        .get_main_or_master_branch()
        .context("no main or master branch")?;

    let root_commit = git_repo
        .get_root_commit(main_or_master_branch_name)
        .context("failed to get root commit of the repository")?;

    // TODO: check for empty repo
    // TODO: check for existing maintaiers file

    let repo_config_result = get_repo_config_from_yaml(&git_repo);
    // TODO: check for other claims

    let name = match &args.title {
        Some(t) => t.clone(),
        None => Interactor::default().input(PromptInputParms::default().with_prompt("name"))?,
    };

    let description = match &args.description {
        Some(t) => t.clone(),
        None => Interactor::default()
            .input(PromptInputParms::default().with_prompt("description (Optional)"))?,
    };

    let git_server = git_repo
        .get_origin_url()
        .context(
            "to claim the repository it must be available on a publically accessable git server",
        )
        .context("no git remote origin configured")?;

    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let (keys, user_ref) = login::launch(&cli_args.nsec, &cli_args.password, Some(&client)).await?;

    client.set_keys(&keys).await;

    let mut maintainers = vec![keys.public_key()];

    let repo_relays: Vec<String> = if !args.relays.is_empty() {
        args.relays.clone()
    } else if let Ok(config) = &repo_config_result {
        config.relays.clone()
    } else {
        // TODO: choice input defaulting to user relay list filtered by non paid relays
        // TODO: allow manual input for more relays
        // TODO: reccommend some free relays
        user_ref.relays.write()
    };

    if let Ok(config) = &repo_config_result {
        maintainers = extract_pks(config.maintainers.clone())?;
    }

    // if yaml file doesnt exist or needs updating
    if match &repo_config_result {
        Ok(config) => {
            !(extract_pks(config.maintainers.clone())?.eq(&maintainers)
                && config.relays.eq(&repo_relays))
        }
        Err(_) => true,
    } {
        save_repo_config_to_yaml(&git_repo, maintainers.clone(), repo_relays.clone())?;
        println!(
            "maintainers.yaml {}. commit and push.",
            if repo_config_result.is_err() {
                "created"
            } else {
                "updated"
            }
        );
    }

    println!("publishing repostory reference...");

    let repo_event = RepoRef {
        name,
        description,
        root_commit: root_commit.to_string(),
        git_server,
        relays: repo_relays.clone(),
        maintainers,
    }
    .to_event(&keys)?;

    // TODO: send repo event to blaster
    send_events(
        &client,
        vec![repo_event],
        user_ref.relays.write(),
        repo_relays,
        !cli_args.disable_cli_spinners,
    )
    .await?;

    Ok(())
}
