use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use nostr::{Keys, PublicKey, ToBech32, nips::nip46::NostrConnectUri};
use nostr_connect::client::NostrConnect;

use super::{
    SignerInfo, SignerInfoSource, credential_store,
    key_encryption::decrypt_key,
    print_logged_in_as,
    user::{UserRef, get_user_details},
};
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{Interactor, InteractorPrompt, PromptPasswordParms},
    client::fetch_public_key,
    git::{Repo, RepoActions, get_git_config_item, get_git_config_item_system},
};

#[derive(Debug)]
pub(super) struct SignerInfoNotFound;

impl std::fmt::Display for SignerInfoNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "failed to get or find signer info in cli arguments, local git config, global git config or system git config",
        )
    }
}

impl std::error::Error for SignerInfoNotFound {}

/// load signer from git config and UserProfile from cache or relays
///
/// # Parameters
/// - `client`: include client to fetch profiles from relays that are missing
///   from cache
/// - `silent`: do not print outcome in termianl
#[allow(clippy::too_many_arguments)]
pub async fn load_existing_login(
    git_repo: &Option<&Repo>,
    signer_info: &Option<SignerInfo>,
    password: &Option<String>,
    source: &Option<SignerInfoSource>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    silent: bool,
    prompt_for_password: bool,
    fetch_profile_updates: bool,
) -> Result<(Arc<crate::NgitSigner>, UserRef, SignerInfoSource)> {
    let (signer_info, source) = get_signer_info(git_repo, signer_info, password, source)?;

    let (signer, public_key) = get_signer(&signer_info, prompt_for_password).await?;

    let user_ref = get_user_details(
        &public_key,
        client,
        if let Some(git_repo) = git_repo {
            Some(git_repo.get_path()?)
        } else {
            None
        },
        silent,
        fetch_profile_updates,
    )
    .await?;

    if !silent {
        print_logged_in_as(&user_ref, client.is_none(), &source)?;
    }
    Ok((signer, user_ref, source))
}

/// priority order: cli arguments, local git config, global git config, system
/// git config
pub fn get_signer_info(
    git_repo: &Option<&Repo>,
    signer_info: &Option<SignerInfo>,
    password: &Option<String>,
    source: &Option<SignerInfoSource>,
) -> Result<(SignerInfo, SignerInfoSource)> {
    Ok(match source {
        None => {
            let mut result = None;
            for source in if std::env::var("NGITTEST").is_ok() {
                vec![
                    SignerInfoSource::CommandLineArguments,
                    SignerInfoSource::GitLocal,
                ]
            } else {
                vec![
                    SignerInfoSource::CommandLineArguments,
                    SignerInfoSource::GitLocal,
                    SignerInfoSource::GitGlobal,
                    SignerInfoSource::GitSystem,
                ]
            } {
                match get_signer_info(git_repo, signer_info, password, &Some(source.clone())) {
                    Ok(res) => {
                        result = Some(res);
                        break;
                    }
                    Err(error)
                        if error
                            .downcast_ref::<credential_store::LookupError>()
                            .is_some() =>
                    {
                        return Err(error);
                    }
                    Err(_) => {}
                }
            }
            result.ok_or(SignerInfoNotFound)?
        }
        Some(SignerInfoSource::CommandLineArguments) => {
            if let Some(signer_info) = signer_info {
                (signer_info.clone(), SignerInfoSource::CommandLineArguments)
            } else {
                bail!("failed to get signer from cli signer arguments because none were specified")
            }
        }
        Some(SignerInfoSource::GitLocal) => {
            let git_repo =
                git_repo.context("failed to get local git config as no git_repo supplied")?;
            if let Ok(nsec) = get_git_config_item(&Some(git_repo), "nostr.nsec")
                .context("failed get local git config")?
                .context("git local config item nostr.nsec doesn't exist")
            {
                let nsec = resolve_config_secret(&Some(git_repo), "nostr.nsec", &nsec, true, true)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item(&Some(git_repo), "nostr.npub")
                            .context("failed get local git config")?,
                    },
                    SignerInfoSource::GitLocal,
                )
            } else if let Ok(bunker_uri) = get_git_config_item(&Some(git_repo), "nostr.bunker-uri")
                .context("failed get local git config")?
                .context("git local config item nostr.bunker-uri doesn't exist")
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&Some(git_repo), "nostr.bunker-app-key", &get_git_config_item(&Some(git_repo), "nostr.bunker-app-key")
                    .context("failed get local git config")?
                    .context("git local config item nostr.bunker-uri exists but nostr.bunker-app-key doesn't")?, false, true)?,
                    npub: get_git_config_item(&Some(git_repo), "nostr.npub")
                        .context("failed get local git config")?,
                }, SignerInfoSource::GitLocal)
            } else {
                bail!("no signer info in local git config")
            }
        }
        Some(SignerInfoSource::GitGlobal) => {
            if let Some(nsec) = get_git_config_item(&None, "nostr.nsec")
                .context("failed to get global git config")?
            {
                let nsec = resolve_config_secret(&None, "nostr.nsec", &nsec, true, true)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item(&None, "nostr.npub")
                            .context("failed to get global git config")?,
                    },
                    SignerInfoSource::GitGlobal,
                )
            } else if let Some(bunker_uri) = get_git_config_item(&None, "nostr.bunker-uri")
                .context("failed to get global git config")?
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&None, "nostr.bunker-app-key", &get_git_config_item(&None, "nostr.bunker-app-key")
                    .context("failed get local git config")?
                    .context("git global config item nostr.bunker-uri exists but nostr.bunker-app-key doesn't")?, false, true)?,
                    npub: get_git_config_item(&None, "nostr.npub")
                        .context("failed get global git config")?,
                }, SignerInfoSource::GitGlobal)
            } else {
                bail!("no signer info in global git config")
            }
        }
        Some(SignerInfoSource::GitSystem) => {
            if let Some(nsec) = get_git_config_item_system("nostr.nsec")
                .context("failed to get system git config")?
            {
                let nsec = resolve_config_secret(&None, "nostr.nsec", &nsec, true, false)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item_system("nostr.npub")
                            .context("failed to get system git config")?,
                    },
                    SignerInfoSource::GitSystem,
                )
            } else if let Some(bunker_uri) = get_git_config_item_system("nostr.bunker-uri")
                .context("failed to get system git config")?
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&None, "nostr.bunker-app-key", &get_git_config_item_system("nostr.bunker-app-key")
                    .context("failed to get system git config")?
                    .context("system git config item nostr.bunker-uri exists but nostr.bunker-app-key doesn't")?, false, false)?,
                    npub: get_git_config_item_system("nostr.npub")
                        .context("failed to get system git config")?,
                }, SignerInfoSource::GitSystem)
            } else {
                bail!("no signer info in system git config")
            }
        }
    })
}

fn resolve_config_secret(
    git_repo: &Option<&Repo>,
    config_key: &str,
    value: &str,
    is_nsec: bool,
    migrate: bool,
) -> Result<String> {
    let classified = if is_nsec {
        credential_store::classify_nsec(value)
    } else {
        credential_store::classify_app_key(value)
    };
    match classified {
        credential_store::ConfigSecret::Encrypted(value) => Ok(value.to_string()),
        credential_store::ConfigSecret::Pointer(name) => {
            let keys = credential_store::retrieve(name)?;
            if is_nsec {
                keys.secret_key().to_bech32().map_err(Into::into)
            } else {
                Ok(keys.secret_key().to_secret_hex())
            }
        }
        credential_store::ConfigSecret::Plaintext(value) if !migrate => Ok(value.to_string()),
        credential_store::ConfigSecret::Plaintext(value)
            if !credential_store::enabled(git_repo) =>
        {
            Ok(value.to_string())
        }
        credential_store::ConfigSecret::Plaintext(value) => {
            let keys = match Keys::parse(value) {
                Ok(keys) => keys,
                Err(_) => return Ok(value.to_string()),
            };
            let pointer = match credential_store::store(&keys) {
                Ok(pointer) => pointer,
                Err(error) => {
                    warn_migration(&format!(
                        "could not move {config_key} into the OS credential store: {error}"
                    ));
                    return Ok(value.to_string());
                }
            };
            let previous_npub = if is_nsec {
                crate::git::get_git_config_item(git_repo, "nostr.npub")
                    .ok()
                    .flatten()
            } else {
                None
            };
            let rewrite = crate::git::save_git_config_item(git_repo, config_key, &pointer)
                .and_then(|_| {
                    if is_nsec {
                        crate::git::save_git_config_item(
                            git_repo,
                            "nostr.npub",
                            &keys.public_key().to_bech32()?,
                        )
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = rewrite {
                let _ = crate::git::save_git_config_item(git_repo, config_key, value);
                if is_nsec {
                    if let Some(npub) = previous_npub {
                        let _ = crate::git::save_git_config_item(git_repo, "nostr.npub", &npub);
                    } else {
                        let _ = crate::git::remove_git_config_item(git_repo, "nostr.npub");
                    }
                }
                let _ = credential_store::delete(&pointer);
                warn_migration(&format!(
                    "could not replace plaintext {config_key} with its credential-store pointer: {error}"
                ));
            }
            Ok(value.to_string())
        }
    }
}

fn warn_migration(message: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !Interactor::is_non_interactive() && !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: {message}; continuing with the existing plaintext credential");
    }
}

async fn get_signer(
    signer_info: &SignerInfo,
    prompt_for_ncryptsec_password: bool,
) -> Result<(Arc<crate::NgitSigner>, PublicKey)> {
    match signer_info {
        SignerInfo::Nsec {
            nsec,
            password,
            npub: _,
        } => {
            let keys = if nsec.contains("ncryptsec") {
                // TODO get user details from npub
                // TODO add retry loop
                // TODO in retry loop give option to login again
                let password = if let Some(password) = password {
                    password.clone()
                } else {
                    if !prompt_for_ncryptsec_password {
                        bail!(
                            "failed to login without prompts a nsec is encrypted with a password"
                        );
                    }
                    Interactor::default()
                        .password(PromptPasswordParms::default().with_prompt("password"))
                        .context("failed to get password input from interactor.password")?
                };
                decrypt_key(nsec, password.clone().as_str())
                    .context("failed to decrypt key with provided password")
                    .context("failed to decrypt ncryptsec supplied as nsec with password")?
            } else {
                nostr::Keys::from_str(nsec).context("invalid nsec parameter")?
            };
            let public_key = keys.public_key();
            Ok((Arc::new(crate::NgitSigner::Keys(keys)), public_key))
        }
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            npub,
        } => {
            let uri = NostrConnectUri::parse(bunker_uri)?;
            let s = NostrConnect::new(
                uri,
                nostr::Keys::from_str(bunker_app_key).context("invalid app key")?,
                Duration::from_secs(10 * 60),
                None,
            )?;
            if let Some(public_key) = npub.clone().and_then(|npub| PublicKey::parse(&npub).ok()) {
                s.non_secure_set_user_public_key(public_key)?;
                let signer = Arc::new(crate::NgitSigner::Connect(Arc::new(s)));
                Ok((signer, public_key))
            } else {
                let signer = Arc::new(crate::NgitSigner::Connect(Arc::new(s)));
                let term = console::Term::stderr();
                term.write_line("connecting to remote signer...")?;
                let public_key = fetch_public_key(&signer).await?;
                term.clear_last_lines(1)?;
                Ok((signer, public_key))
            }
        }
    }
}
