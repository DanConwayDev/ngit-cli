use anyhow::{Context, Result};
use console::Style;
use ngit::{
    cli_interactor::cli_error,
    client::{Params, get_repo_ref_from_cache, send_events},
    repo_ref::RepoRef,
};
use nostr::prelude::{Timestamp, ToBech32};

use crate::{
    cli::SignerParams,
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::{print_selected_repo, try_resolve_repo_coordinate},
    sub_commands::repository_fetch::prepare_account_for_repo_fetch,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {}

/// Membership is declared by your own announcement; leaving means ending
/// the self-role it records. Errors when no announcement of `my_pubkey`
/// exists for the coordinate (an unaccepted invitation grants no role) or
/// when its self-role has already ended.
fn my_membership_announcement(
    repo_ref: &ngit::repo_ref::RepoRef,
    my_pubkey: &nostr::prelude::PublicKey,
) -> Result<RepoRef> {
    let Some(my_event) = repo_ref
        .events
        .values()
        .find(|event| event.pubkey == *my_pubkey)
        .cloned()
    else {
        let hint = if repo_ref.maintainers.contains(my_pubkey) {
            "you have been invited but never accepted with `ngit repo accept`; an unaccepted invitation grants no role, so there is nothing to leave"
        } else {
            "you are not a member of this repository"
        };
        return Err(cli_error(
            "you have no announcement for this repository",
            &[],
            &[hint],
        ));
    };

    let my_ref = RepoRef::try_from((my_event, None))
        .context("failed to parse your existing announcement")?;

    if !my_ref.maintainers.contains(my_pubkey) && !my_ref.moderators.contains(my_pubkey) {
        return Err(cli_error(
            "you are not a member of this repository",
            &[],
            &["your announcement already records your role as ended"],
        ));
    }
    Ok(my_ref)
}

pub async fn launch(_args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
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
            &["there is no nostr repository here to leave"],
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
            &["if this is a relay or network issue, try again later"],
        ));
    };

    let mut my_ref = my_membership_announcement(&repo_ref, &my_pubkey)?;

    // Leaving as the lead is allowed but may leave the repository leadless:
    // co-maintainers under a lead SHOULD list only themselves and the lead,
    // so nobody else's announcement may assert a replacement yet.
    if repo_ref.lead_maintainer() == Some(my_pubkey) {
        let warn_style = Style::new().yellow();
        eprintln!(
            "{}",
            warn_style.apply_to(
                "warning: you are the lead maintainer; leaving may leave the repository without a lead"
            ),
        );
    }

    let repo_name = my_ref.name.clone();
    println!("leaving '{repo_name}'");

    let ended = my_ref.end_self_role(&my_pubkey, Timestamp::now().as_secs());
    // defensive: the membership checks above guarantee an active self-role
    if !ended {
        return Err(cli_error(
            "you are not a member of this repository",
            &[],
            &["your announcement already records your role as ended"],
        ));
    }

    // Order the republished announcement after every announcement seen for
    // the coordinate, like `ngit init` does.
    my_ref.events = repo_ref.events.clone();

    println!("publishing your updated announcement to nostr...");
    let repo_event = my_ref.to_event(&signer).await?;

    client.set_signer(signer.clone()).await;
    if repo_ref.private {
        client.nip42_register_private_repo_relays(repo_ref.relays.clone());
    }

    // Publish to the union of members' relays, not just my own: other
    // members and consumers must observe the ended self-role, which per
    // NIP-34 takes precedence over their assignments.
    let _ = send_events(
        &client,
        Some(git_repo_path),
        vec![repo_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await
    .context("failed to publish the announcement ending your role")?;

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "status": "ok",
            "action": "left",
            "entity": "repository",
            "name": repo_name,
            "coordinate": repo_coordinate.to_bech32()?,
        }));
    }
    println!("membership ended. your announcement now records your role as ended.");
    println!(
        "other members' announcements may still list you; per NIP-34 your own record takes precedence."
    );

    Ok(())
}
