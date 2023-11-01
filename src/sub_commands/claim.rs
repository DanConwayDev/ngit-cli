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
    sub_commands::repo_ref::RepoRef,
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

    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let (keys, user_ref) = login::launch(&cli_args.nsec, &cli_args.password, Some(&client)).await?;

    client.set_keys(&keys).await;

    // TODO: choice input defaulting to user relay list filtered by non paid relays
    let repo_relays: Vec<String> = vec![
        "ws://localhost:8055".to_string(),
        "ws://localhost:8056".to_string(),
    ];

    println!("publishing repostory reference...");

    let repo_event = RepoRef {
        name,
        description,
        root_commit: root_commit.to_string(),
        relays: repo_relays.clone(),
    }
    .to_event(&keys)?;

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
