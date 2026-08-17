use std::{path::Path, sync::Arc};

use ngit::{
    client::{
        Client, Connect, FetchReport, fetching_with_private_discovery, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions},
    login::{
        existing::load_existing_login,
        user::{PrivateGitRelayDiscovery, UserRef, discover_private_git_relay_list},
    },
    signer::NgitSigner,
};
use nostr::nips::nip19::Nip19Coordinate;

use crate::cli::SignerParams;

/// Prepare the account explicitly selected for a repository operation.
pub async fn prepare_selected_account_for_repo_fetch(
    git_repo: &Repo,
    client: &mut Client,
    coordinate: &mut Nip19Coordinate,
    auth: SignerParams<'_>,
) -> anyhow::Result<PrivateGitRelayDiscovery> {
    let repository_is_known_private = git_repo
        .git_repo
        .config()
        .ok()
        .and_then(|config| config.get_bool("nostr.private").ok())
        .unwrap_or(false)
        || get_repo_ref_from_cache(Some(git_repo.get_path()?), coordinate)
            .await
            .is_ok_and(|repo_ref| repo_ref.private);
    let login = load_existing_login(
        &Some(git_repo),
        auth.info,
        auth.password,
        &None,
        None,
        true,
        false,
        false,
    )
    .await;
    let (signer, user_ref, _) = match login {
        Ok(login) => login,
        Err(error) if auth.info.is_some() => return Err(error),
        Err(error) if repository_is_known_private => {
            return Err(error
                .context("private repository relay authentication requires a logged-in account"));
        }
        Err(_) => return Ok(PrivateGitRelayDiscovery::Absent),
    };

    Ok(prepare_account_for_repo_fetch(client, coordinate, &signer, &user_ref).await)
}

/// Install a known account and load its private discovery hints.
pub async fn prepare_account_for_repo_fetch(
    client: &mut Client,
    _coordinate: &mut Nip19Coordinate,
    signer: &Arc<NgitSigner>,
    user_ref: &UserRef,
) -> PrivateGitRelayDiscovery {
    client.set_signer(signer.clone()).await;
    let mut discovery_relays = user_ref.relays.read();
    for relay in user_ref.relays.write() {
        if !discovery_relays.contains(&relay) {
            discovery_relays.push(relay);
        }
    }
    if discovery_relays.is_empty() {
        discovery_relays.extend(client.get_relay_default_set().iter().cloned());
    }

    discover_private_git_relay_list(client, discovery_relays, signer).await
}

/// Fetch repository state after giving an existing account the opportunity to
/// authenticate and extend discovery with its private relay list.
pub async fn fetching_with_account(
    git_repo: &Repo,
    git_repo_path: &Path,
    client: &mut Client,
    coordinate: &mut Nip19Coordinate,
    auth: SignerParams<'_>,
) -> anyhow::Result<FetchReport> {
    let private_discovery =
        prepare_selected_account_for_repo_fetch(git_repo, client, coordinate, auth).await?;
    fetching_with_private_discovery(git_repo_path, client, coordinate, &private_discovery).await
}
