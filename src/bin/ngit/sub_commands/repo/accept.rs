use std::sync::Arc;

use anyhow::{Context, Result};
use ngit::{
    accept_maintainership::{accept_maintainership_with_defaults, wait_for_grasp_servers},
    cli_interactor::cli_error,
    client::{Params, fetching_with_report, get_repo_ref_from_cache, send_events},
    repo_ref::{RepoRef, apply_grasp_infrastructure, latest_event_repo_ref},
};
use nostr::{
    ToBech32,
    nips::{nip01::Coordinate, nip19::Nip19Coordinate},
};
use nostr_sdk::{Kind, NostrSigner, RelayUrl};

use crate::{
    cli::{Cli, extract_signer_cli_arguments},
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::try_and_get_repo_coordinates_when_remote_unknown,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[clap(short, long, value_parser, num_args = 1..)]
    /// where your git+nostr data is hosted (optional; uses your saved grasp
    /// server list or the trusted maintainer's servers if not specified)
    grasp_server: Vec<String>,
}

pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        Some(&client),
        false,
    )
    .await?;

    let my_pubkey = user_ref.public_key;

    let repo_coordinate = (try_and_get_repo_coordinates_when_remote_unknown(&git_repo).await).ok();

    let Some(repo_coordinate) = repo_coordinate else {
        return Err(cli_error(
            "no nostr repository found",
            &[],
            &["use `ngit repo init` to publish this repository to nostr"],
        ));
    };

    // Fetch latest data from relays
    fetching_with_report(git_repo_path, &client, &repo_coordinate).await?;

    let Some(repo_ref) =
        (get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await).ok()
    else {
        return Err(cli_error(
            "no announcement found on relays for this repository",
            &[],
            &[
                "if you created this repository, use `ngit repo init` to publish an announcement",
                "if this is a relay or network issue, try again later",
            ],
        ));
    };

    // Validate state
    let trusted = repo_ref.trusted_maintainer;

    if trusted == my_pubkey {
        return Err(cli_error(
            "you are already the trusted maintainer of this repository",
            &[],
            &["use `ngit repo edit` to update your announcement"],
        ));
    }

    let has_announcement = repo_ref
        .events
        .keys()
        .any(|c| c.coordinate.public_key == my_pubkey);

    if has_announcement {
        return Err(cli_error(
            "you have already published a co-maintainer announcement for this repository",
            &[],
            &["use `ngit repo edit` to update your announcement"],
        ));
    }

    if !repo_ref.maintainers.contains(&my_pubkey) {
        let trusted_npub = trusted.to_bech32().unwrap_or_else(|_| trusted.to_hex());
        return Err(cli_error(
            "you have not been invited as a maintainer of this repository",
            &[("trusted maintainer", trusted_npub.as_str())],
            &["the trusted maintainer must add your npub to their announcement first"],
        ));
    }

    // Happy path: CoMaintainer state without an existing announcement
    let repo_name = &repo_ref.name;
    let trusted_npub = trusted.to_bech32().unwrap_or_else(|_| trusted.to_hex());
    println!("accepting co-maintainership of '{repo_name}' (offered by {trusted_npub})");
    println!("publishing your repository announcement to nostr...");

    if args.grasp_server.is_empty() {
        // Use the existing defaults logic from the library
        accept_maintainership_with_defaults(&git_repo, &repo_ref, &user_ref, &mut client, &signer)
            .await?;
    } else {
        // User specified grasp servers explicitly — use them
        accept_with_grasp_servers(
            &git_repo,
            &repo_ref,
            &signer,
            &user_ref,
            &mut client,
            &args.grasp_server,
        )
        .await?;
    }

    println!("co-maintainership accepted.");
    println!("your announcement has been published to nostr. you can now push updates.");
    println!("run `ngit repo edit` at any time to update your announcement.");

    Ok(())
}

/// Accept co-maintainership with explicitly specified grasp servers.
#[allow(clippy::too_many_lines)]
async fn accept_with_grasp_servers(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    signer: &Arc<dyn NostrSigner>,
    user_ref: &ngit::login::user::UserRef,
    client: &mut Client,
    grasp_servers: &[String],
) -> Result<()> {
    let my_pubkey = &user_ref.public_key;
    let identifier = &repo_ref.identifier;

    let mut git_servers: Vec<String> = vec![];
    let mut relay_strings: Vec<String> = vec![];

    apply_grasp_infrastructure(
        grasp_servers,
        &mut git_servers,
        &mut relay_strings,
        my_pubkey,
        identifier,
    )?;

    let relays: Vec<RelayUrl> = relay_strings
        .iter()
        .filter_map(|r| RelayUrl::parse(r).ok())
        .collect();

    let latest = latest_event_repo_ref(repo_ref);
    let name = latest
        .as_ref()
        .map_or_else(|| identifier.clone(), |lr| lr.name.clone());
    let description = latest
        .as_ref()
        .map(|lr| lr.description.clone())
        .unwrap_or_default();
    let web = latest.as_ref().map(|lr| lr.web.clone()).unwrap_or_default();
    let hashtags = latest
        .as_ref()
        .map(|lr| lr.hashtags.clone())
        .unwrap_or_default();
    let blossoms = latest
        .as_ref()
        .map(|lr| lr.blossoms.clone())
        .unwrap_or_default();
    let root_commit = latest
        .as_ref()
        .map(|lr| lr.root_commit.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| repo_ref.root_commit.clone());

    let mut maintainers = vec![*my_pubkey];
    if repo_ref.trusted_maintainer != *my_pubkey {
        maintainers.push(repo_ref.trusted_maintainer);
    }

    let my_repo_ref = RepoRef {
        identifier: identifier.clone(),
        name,
        description,
        root_commit,
        git_server: git_servers,
        web,
        relays: relays.clone(),
        blossoms,
        hashtags,
        trusted_maintainer: *my_pubkey,
        maintainers_without_annoucnement: None,
        maintainers,
        events: std::collections::HashMap::new(),
        nostr_git_url: None,
    };

    let repo_event = my_repo_ref.to_event(signer).await?;

    client.set_signer(signer.clone()).await;

    send_events(
        client,
        Some(git_repo.get_path()?),
        vec![repo_event],
        user_ref.relays.write(),
        relays.clone(),
        true,
        false,
    )
    .await
    .context("failed to publish co-maintainer announcement")?;

    if !grasp_servers.is_empty() {
        wait_for_grasp_servers(git_repo, grasp_servers, my_pubkey, identifier).await?;
    }

    // Update nostr.repo git config
    git_repo
        .save_git_config_item(
            "nostr.repo",
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: *my_pubkey,
                    identifier: identifier.clone(),
                },
                relays: vec![],
            }
            .to_bech32()?,
            false,
        )
        .context("failed to update nostr.repo git config")?;

    // Update origin remote
    let nostr_url = my_repo_ref.to_nostr_git_url(&Some(git_repo)).to_string();
    if git_repo.git_repo.find_remote("origin").is_ok() {
        git_repo
            .git_repo
            .remote_set_url("origin", &nostr_url)
            .context("failed to update origin remote")?;
    } else {
        git_repo
            .git_repo
            .remote("origin", &nostr_url)
            .context("failed to set origin remote")?;
    }

    Ok(())
}
