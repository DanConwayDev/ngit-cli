use std::{path::Path, sync::Arc};

use ngit::{
    client::{
        Client, Connect, FetchReport, fetching_with_private_discovery, get_repo_ref_from_cache,
        needs_private_relay_discovery,
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
    let configured_privacy = git_repo
        .git_repo
        .config()
        .ok()
        .and_then(|config| config.get_bool("nostr.private").ok());
    let cached_repo_ref = get_repo_ref_from_cache(Some(git_repo.get_path()?), coordinate)
        .await
        .ok();
    let repository_is_known_private = configured_privacy == Some(true)
        || cached_repo_ref
            .as_ref()
            .is_some_and(|repo_ref| repo_ref.private);
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

    Ok(prepare_account_for_repo_fetch_with_requirement(
        client,
        &signer,
        &user_ref,
        needs_private_relay_discovery(configured_privacy, cached_repo_ref.is_some(), false),
    )
    .await)
}

/// Install a known account and load its private discovery hints only for a
/// known private repository whose announcement remains unresolved.
pub async fn prepare_account_for_repo_fetch<C: Connect + Sync>(
    git_repo: &Repo,
    client: &mut C,
    coordinate: &Nip19Coordinate,
    signer: &Arc<NgitSigner>,
    user_ref: &UserRef,
) -> PrivateGitRelayDiscovery {
    let configured_privacy = git_repo
        .git_repo
        .config()
        .ok()
        .and_then(|config| config.get_bool("nostr.private").ok());
    let has_cached_announcement = match git_repo.get_path() {
        Ok(path) => get_repo_ref_from_cache(Some(path), coordinate)
            .await
            .is_ok(),
        Err(_) => false,
    };
    prepare_account_for_repo_fetch_with_requirement(
        client,
        signer,
        user_ref,
        needs_private_relay_discovery(configured_privacy, has_cached_announcement, false),
    )
    .await
}

async fn prepare_account_for_repo_fetch_with_requirement<C: Connect + Sync>(
    client: &mut C,
    signer: &Arc<NgitSigner>,
    user_ref: &UserRef,
    private_relay_discovery_required: bool,
) -> PrivateGitRelayDiscovery {
    client.set_signer(signer.clone()).await;
    if !private_relay_discovery_required {
        return PrivateGitRelayDiscovery::Absent;
    }

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

#[cfg(test)]
mod tests {
    use ngit::{
        client::Params,
        login::user::{UserGraspList, UserMetadata, UserRelays},
    };
    use nostr::prelude::{Coordinate, Keys, Kind, Timestamp};

    use super::*;

    #[rstest::rstest]
    #[case(None)]
    #[case(Some(false))]
    #[tokio::test]
    async fn public_or_unclassified_repository_skips_private_relay_discovery(
        #[case] privacy: Option<bool>,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init(temporary.path()).unwrap();
        if let Some(privacy) = privacy {
            repository
                .config()
                .unwrap()
                .set_bool("nostr.private", privacy)
                .unwrap();
        }
        drop(repository);
        let git_repo = Repo::from_path(&temporary.path().to_path_buf()).unwrap();

        let keys = Keys::generate();
        let signer = Arc::new(NgitSigner::Keys(keys.clone()));
        let user_ref = UserRef {
            public_key: keys.public_key(),
            metadata: UserMetadata {
                name: String::new(),
                created_at: Timestamp::from(0),
                nip05: None,
            },
            relays: UserRelays {
                relays: vec![],
                created_at: Timestamp::from(0),
            },
            grasp_list: UserGraspList {
                urls: vec![],
                created_at: Timestamp::from(0),
            },
        };
        let coordinate = Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: keys.public_key(),
                identifier: "public-repository".to_string(),
            },
            relays: vec![],
        };
        let mut client = Client::new(Params {
            keys: None,
            relay_default_set: vec![],
            announcement_indexer_relays: vec![],
            blaster_relays: vec![],
            fallback_signer_relays: vec![],
            grasp_default_set: vec![],
        });

        assert_eq!(
            prepare_account_for_repo_fetch(
                &git_repo,
                &mut client,
                &coordinate,
                &signer,
                &user_ref,
            )
            .await,
            PrivateGitRelayDiscovery::Absent
        );
    }
}
