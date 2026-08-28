use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use nostr::prelude::{EventBuilder, Kind, Tag, Url};

use crate::{
    git::Repo,
    login::{SignerInfo, existing::load_existing_login},
    repo_ref::RepoRef,
    signer::NgitSigner,
};

const NIP98_KIND: Kind = Kind::Custom(27235);

static AUTHORIZATIONS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn authorizations() -> &'static RwLock<HashMap<String, String>> {
    AUTHORIZATIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Return the canonical repository root required by GRASP-08.
///
/// Query parameters, fragments, and trailing slashes are deliberately removed.
pub fn canonical_repository_url(url: &str) -> Result<String> {
    let mut url = Url::parse(url).context("private git server URL must be absolute")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("private git server URL must use HTTP or HTTPS")
    }
    url.set_username("")
        .map_err(|()| anyhow::anyhow!("failed to remove Git URL username"))?;
    url.set_password(None)
        .map_err(|()| anyhow::anyhow!("failed to remove Git URL password"))?;
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Sign and install reusable NIP-98 credentials for a Smart HTTP operation.
///
/// Entries are merged into the process-local registry (never wholesale
/// replaced) so concurrent operations preparing other repository roots cannot
/// observe their credentials vanishing. Isolation between accounts is
/// provided by the explicit [`clear_private_git_auth`] call at the start of
/// each operation.
pub async fn prepare_private_git_auth(
    git_server_urls: &[String],
    signer: &Arc<NgitSigner>,
) -> Result<()> {
    let mut prepared = HashMap::new();
    for server_url in git_server_urls {
        let Some((canonical, authorization)) =
            build_private_git_authorization(server_url, signer).await?
        else {
            continue;
        };
        if prepared.contains_key(&canonical) {
            continue;
        }
        prepared.insert(canonical, authorization);
    }
    authorizations()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .extend(prepared);
    Ok(())
}

/// Refresh a single URL without invalidating credentials used by concurrent
/// Smart HTTP operations for other repository roots.
pub async fn refresh_private_git_auth_for_url(
    server_url: &str,
    signer: &Arc<NgitSigner>,
) -> Result<()> {
    let Some((canonical, authorization)) =
        build_private_git_authorization(server_url, signer).await?
    else {
        return Ok(());
    };
    authorizations()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(canonical, authorization);
    Ok(())
}

async fn build_private_git_authorization(
    server_url: &str,
    signer: &Arc<NgitSigner>,
) -> Result<Option<(String, String)>> {
    let Ok(canonical) = canonical_repository_url(server_url) else {
        // SSH, git, and filesystem transports do not issue HTTP requests.
        return Ok(None);
    };
    // GRASP-08 deliberately specifies a single repo-root NIP-98 credential
    // with a fixed `GET` method tag for ALL Smart HTTP operations, including
    // the `git-upload-pack`/`git-receive-pack` POSTs. Buzz validates exactly
    // this shape, so the method tag must stay `GET` regardless of the HTTP
    // method actually used. This is an intentional GRASP-08/Buzz contract,
    // not a NIP-98 spec mismatch — do not "fix" it to match the HTTP verb.
    let event = signer
        .sign_event_builder_with_description(
            EventBuilder::new(NIP98_KIND, "").tags([
                Tag::parse(["u", canonical.as_str()])?,
                Tag::parse(["method", "GET"])?,
            ]),
            "private repository HTTP authorization",
        )
        .await
        .context("failed to sign private repository HTTP authorization")?;
    Ok(Some((
        canonical,
        format!("Authorization: Nostr {}", STANDARD.encode(event.as_json())),
    )))
}

/// Load the current account and prepare authentication when a CLI command may
/// need to retrieve Git data outside the remote-helper process.
pub async fn prepare_private_git_auth_for_repo(
    repo_ref: &RepoRef,
    git_repo: &Repo,
    signer_info: &Option<SignerInfo>,
    password: &Option<String>,
) -> Result<Option<Arc<NgitSigner>>> {
    clear_private_git_auth();
    if !repo_ref.private {
        return Ok(None);
    }
    let (signer, _, _) = load_existing_login(
        &Some(git_repo),
        signer_info,
        password,
        &None,
        None,
        false,
        false,
        false,
    )
    .await
    .context("private repository Git access requires a logged-in account")?;
    Ok(Some(signer))
}

pub fn clear_private_git_auth() {
    authorizations()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Look up the custom header for a libgit2 operation at a repository root.
pub fn authorization_for_url(url: &str) -> Option<String> {
    let canonical = canonical_repository_url(url).ok()?;
    authorizations()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&canonical)
        .cloned()
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{Event, Keys};

    use super::*;

    #[test]
    fn canonical_url_drops_query_fragment_and_trailing_slashes() {
        assert_eq!(
            canonical_repository_url(
                "https://account:secret@git.example/repo.git///?service=x#fragment"
            )
            .unwrap(),
            "https://git.example/repo.git"
        );
        assert!(canonical_repository_url("ssh://git.example/repo.git").is_err());
    }

    #[tokio::test]
    async fn prepared_header_is_canonical_get_and_reusable() {
        let signer = Arc::new(NgitSigner::Keys(Keys::generate()));
        prepare_private_git_auth(
            &["https://git.example/repo.git/?ignored=true".to_string()],
            &signer,
        )
        .await
        .unwrap();

        let header = authorization_for_url("https://git.example/repo.git").unwrap();
        let encoded = header.strip_prefix("Authorization: Nostr ").unwrap();
        let event = Event::from_json(String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap())
            .unwrap();
        assert_eq!(event.kind, NIP98_KIND);
        assert!(event.verify().is_ok());
        assert!(event.tags.iter().any(|tag| {
            tag.as_slice() == ["u".to_string(), "https://git.example/repo.git".to_string()]
        }));
        assert!(
            event
                .tags
                .iter()
                .any(|tag| { tag.as_slice() == ["method".to_string(), "GET".to_string()] })
        );
        assert_eq!(
            authorization_for_url("https://git.example/repo.git/"),
            Some(header)
        );
    }

    #[tokio::test]
    async fn preparing_one_server_keeps_other_servers_credentials() {
        let signer = Arc::new(NgitSigner::Keys(Keys::generate()));
        prepare_private_git_auth(&["https://keep.example/repo.git".to_string()], &signer)
            .await
            .unwrap();
        prepare_private_git_auth(&["https://other.example/repo.git".to_string()], &signer)
            .await
            .unwrap();
        assert!(authorization_for_url("https://keep.example/repo.git").is_some());
        assert!(authorization_for_url("https://other.example/repo.git").is_some());
    }

    #[tokio::test]
    async fn single_url_refresh_builds_an_exact_fresh_scope() {
        let signer = Arc::new(NgitSigner::Keys(Keys::generate()));
        let (canonical, header) = build_private_git_authorization(
            "https://two.example/other.git/?service=git-upload-pack",
            &signer,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(canonical, "https://two.example/other.git");

        let encoded = header.strip_prefix("Authorization: Nostr ").unwrap();
        let event = Event::from_json(String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap())
            .unwrap();
        assert!(event.tags.iter().any(|tag| {
            tag.as_slice() == ["u".to_string(), "https://two.example/other.git".to_string()]
        }));
    }
}
