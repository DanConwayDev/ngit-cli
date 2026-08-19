use std::sync::Arc;

use anyhow::{Context, Result};
use ngit::{
    accept_maintainership::{
        accept_maintainership_with_defaults, default_acceptance_maintainers, wait_for_grasp_servers,
    },
    cli_interactor::cli_error,
    client::{Params, get_repo_ref_from_cache, send_events},
    git::nostr_url::NostrUrlDecoded,
    login::user::publish_private_git_relay_list,
    repo_ref::{RepoRef, apply_grasp_infrastructure, latest_event_repo_ref},
    signer::NgitSigner,
};
use nostr::prelude::{RelayUrl, ToBech32, nip19::Nip19Coordinate};

use crate::{
    cli::SignerParams,
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::{print_selected_repo, try_resolve_repo_coordinate},
    sub_commands::repository_fetch::prepare_account_for_repo_fetch,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[clap(short, long, value_parser, num_args = 1..)]
    /// where your git+nostr data is hosted (optional; uses your saved grasp
    /// server list or the selected maintainer's servers if not specified)
    grasp_server: Vec<String>,
}

pub async fn launch(args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer.info,
        signer.password,
        Some(&client),
        false,
    )
    .await?;

    let my_pubkey = user_ref.public_key;

    let Some(resolved_repo_coordinate) = try_resolve_repo_coordinate(&git_repo).await? else {
        return Err(cli_error(
            "no nostr repository found",
            &[],
            &["use `ngit repo init` to publish this repository to nostr"],
        ));
    };
    print_selected_repo(&resolved_repo_coordinate);
    let mut repo_coordinate = resolved_repo_coordinate.coordinate;

    // Fetch latest data from relays
    let private_discovery =
        prepare_account_for_repo_fetch(&mut client, &mut repo_coordinate, &signer, &user_ref).await;
    ngit::client::fetching_with_private_discovery(
        git_repo_path,
        &client,
        &mut repo_coordinate,
        &private_discovery,
    )
    .await?;

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
    let selected = repo_ref.selected_maintainer;

    if selected == my_pubkey {
        return Err(cli_error(
            "you are already the selected maintainer of this repository",
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
        let selected_npub = selected.to_bech32().unwrap_or_else(|_| selected.to_hex());
        return Err(cli_error(
            "you have not been invited as a maintainer of this repository",
            &[("selected maintainer", selected_npub.as_str())],
            &["the selected maintainer must add your npub to their announcement first"],
        ));
    }

    // Happy path: CoMaintainer state without an existing announcement
    let repo_name = &repo_ref.name;
    println!("accepting maintainer invitation for '{repo_name}'");
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

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "action": "accepted",
            "entity": "repository",
            "name": repo_name,
            "coordinate": repo_coordinate.to_bech32()?,
        }));
    }
    print_completion_message(&git_repo, repo_coordinate);

    Ok(())
}

/// Report success and, when origin is not a `nostr://` remote, explain how
/// to get pushes flowing through nostr — suggesting the coordinate accept
/// ran against (the inviter's), never the accepter's own npub. See
/// `accept_with_grasp_servers` for why resolution must stay rooted on the
/// inviter's coordinate.
fn print_completion_message(git_repo: &Repo, repo_coordinate: Nip19Coordinate) {
    println!("co-maintainership accepted.");
    let origin_is_nostr = git_repo
        .git_repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(std::string::ToString::to_string).ok())
        .is_some_and(|url| url.starts_with("nostr://"));
    if origin_is_nostr {
        println!("your announcement has been published to nostr. you can now push updates.");
    } else {
        let inviter_url = NostrUrlDecoded {
            original_string: String::new(),
            coordinate: repo_coordinate,
            protocol: None,
            ssh_key_file: None,
            nip05: None,
        };
        println!("your announcement has been published to nostr.");
        println!(
            "pushes go through nostr only via a nostr remote; add one with `git remote add nostr {inviter_url}`"
        );
    }
    println!("run `ngit repo edit` at any time to update your announcement.");
}

/// Accept co-maintainership with explicitly specified grasp servers.
async fn accept_with_grasp_servers(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    signer: &Arc<NgitSigner>,
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
    let upstream = latest
        .as_ref()
        .map(|lr| lr.upstream.clone())
        .unwrap_or_default();
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

    let maintainers = default_acceptance_maintainers(repo_ref, *my_pubkey);

    let my_repo_ref = RepoRef {
        identifier: identifier.clone(),
        name,
        description,
        root_commit,
        git_server: git_servers,
        web,
        upstream,
        relays: relays.clone(),
        blossoms,
        hashtags,
        private: repo_ref.private,
        selected_maintainer: *my_pubkey,
        maintainers_without_annoucnement: None,
        maintainers,
        events: std::collections::HashMap::new(),
        nostr_git_url: None,
        extra_tags: vec![],
        role_tags: vec![],
        moderators: vec![],
    };

    let repo_event = my_repo_ref.to_event(signer).await?;

    client.set_signer(signer.clone()).await;

    if repo_ref.private {
        publish_private_git_relay_list(client, &relays, user_ref, signer)
            .await
            .context("failed to publish private Git relay discovery list")?;
    }

    let _ = send_events(
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
        wait_for_grasp_servers(
            git_repo,
            grasp_servers,
            my_pubkey,
            identifier,
            repo_ref.private.then(|| signer.clone()),
        )
        .await?;
    }

    // Deliberately leave `nostr.repo` and the origin remote untouched: the
    // coordinate the repo resolves from is the root of trust. Re-rooting
    // resolution on the accepter's own announcement — which always lists
    // them as a maintainer — would make it impossible to observe the
    // inviter removing them later. Keeping resolution on the inviter's
    // coordinate means removal surfaces naturally; only `ngit repo edit` /
    // `ngit init` may change the resolved coordinate deliberately.

    Ok(())
}
