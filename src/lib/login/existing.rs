use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use nostr::prelude::{PublicKey, ToBech32, nip46::NostrConnectUri};
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
                    Err(error) => {
                        let explicit_cli_selection = source
                            == SignerInfoSource::CommandLineArguments
                            && matches!(signer_info, Some(SignerInfo::Selection { .. }));
                        let configured_selection = match source {
                            SignerInfoSource::GitLocal => git_repo.is_some_and(|repo| {
                                get_git_config_item(&Some(repo), "nostr.signer")
                                    .is_ok_and(|value| value.is_some())
                            }),
                            SignerInfoSource::GitGlobal => {
                                get_git_config_item(&None, "nostr.signer")
                                    .is_ok_and(|value| value.is_some())
                            }
                            SignerInfoSource::GitSystem => {
                                get_git_config_item_system("nostr.signer")
                                    .is_ok_and(|value| value.is_some())
                            }
                            SignerInfoSource::CommandLineArguments => false,
                        };
                        if explicit_cli_selection
                            || configured_selection
                            || error
                                .downcast_ref::<credential_store::LookupError>()
                                .is_some()
                        {
                            return Err(error);
                        }
                    }
                }
            }
            result.ok_or(SignerInfoNotFound)?
        }
        Some(SignerInfoSource::CommandLineArguments) => {
            if let Some(signer_info) = signer_info {
                let signer_info = match signer_info {
                    SignerInfo::Selection { selector } => {
                        resolve_selected_signer(git_repo, selector, password)?
                    }
                    signer_info => signer_info.clone(),
                };
                (signer_info, SignerInfoSource::CommandLineArguments)
            } else {
                bail!("failed to get signer from cli signer arguments because none were specified")
            }
        }
        Some(SignerInfoSource::GitLocal) => {
            let git_repo =
                git_repo.context("failed to get local git config as no git_repo supplied")?;
            if let Some(selector) = get_git_config_item(&Some(git_repo), "nostr.signer")
                .context("failed get local git config")?
            {
                (
                    resolve_selected_signer(&Some(git_repo), &selector, password)?,
                    SignerInfoSource::GitLocal,
                )
            } else if let Ok(nsec) = get_git_config_item(&Some(git_repo), "nostr.nsec")
                .context("failed get local git config")?
                .context("git local config item nostr.nsec doesn't exist")
            {
                let nsec = resolve_config_secret(&Some(git_repo), &nsec, true, true)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item(&Some(git_repo), "nostr.npub")
                            .context("failed get local git config")?,
                        verify_npub: false,
                    },
                    SignerInfoSource::GitLocal,
                )
            } else if let Ok(bunker_uri) = get_git_config_item(&Some(git_repo), "nostr.bunker-uri")
                .context("failed get local git config")?
                .context("git local config item nostr.bunker-uri doesn't exist")
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&Some(git_repo), &get_git_config_item(&Some(git_repo), "nostr.bunker-app-key")
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
            if let Some(selector) = get_git_config_item(&None, "nostr.signer")
                .context("failed to get global git config")?
            {
                (
                    resolve_selected_signer(git_repo, &selector, password)?,
                    SignerInfoSource::GitGlobal,
                )
            } else if let Some(nsec) = get_git_config_item(&None, "nostr.nsec")
                .context("failed to get global git config")?
            {
                let nsec = resolve_config_secret(&None, &nsec, true, true)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item(&None, "nostr.npub")
                            .context("failed to get global git config")?,
                        verify_npub: false,
                    },
                    SignerInfoSource::GitGlobal,
                )
            } else if let Some(bunker_uri) = get_git_config_item(&None, "nostr.bunker-uri")
                .context("failed to get global git config")?
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&None, &get_git_config_item(&None, "nostr.bunker-app-key")
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
            if let Some(selector) = get_git_config_item_system("nostr.signer")
                .context("failed to get system git config")?
            {
                (
                    resolve_selected_signer(git_repo, &selector, password)?,
                    SignerInfoSource::GitSystem,
                )
            } else if let Some(nsec) = get_git_config_item_system("nostr.nsec")
                .context("failed to get system git config")?
            {
                let nsec = resolve_config_secret(&None, &nsec, true, false)?;
                (
                    SignerInfo::Nsec {
                        nsec: nsec.to_string(),
                        password: password.clone(),
                        npub: get_git_config_item_system("nostr.npub")
                            .context("failed to get system git config")?,
                        verify_npub: false,
                    },
                    SignerInfoSource::GitSystem,
                )
            } else if let Some(bunker_uri) = get_git_config_item_system("nostr.bunker-uri")
                .context("failed to get system git config")?
            {
                (SignerInfo::Bunker {
                    bunker_uri, bunker_app_key: resolve_config_secret(&None, &get_git_config_item_system("nostr.bunker-app-key")
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

#[derive(Clone, Copy)]
enum ConfigScope {
    Local,
    Global,
    System,
}

impl ConfigScope {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
            Self::System => "system",
        }
    }
}

fn selection_scopes(git_repo: &Option<&Repo>) -> Vec<ConfigScope> {
    let mut scopes = Vec::with_capacity(3);
    if git_repo.is_some() {
        scopes.push(ConfigScope::Local);
    }
    if std::env::var("NGITTEST").is_err() {
        scopes.extend([ConfigScope::Global, ConfigScope::System]);
    }
    scopes
}

fn config_value(git_repo: &Option<&Repo>, scope: ConfigScope, key: &str) -> Result<Option<String>> {
    match scope {
        ConfigScope::Local => {
            let repo = git_repo.context("cannot read local git config without a repository")?;
            get_git_config_item(&Some(repo), key).context("failed to read local git config")
        }
        ConfigScope::Global => {
            get_git_config_item(&None, key).context("failed to read global git config")
        }
        ConfigScope::System => {
            get_git_config_item_system(key).context("failed to read system git config")
        }
    }
}

fn resolve_scope_secret(
    git_repo: &Option<&Repo>,
    scope: ConfigScope,
    value: &str,
    is_nsec: bool,
) -> Result<String> {
    resolve_config_secret(
        if matches!(scope, ConfigScope::Local) {
            git_repo
        } else {
            &None
        },
        value,
        is_nsec,
        !matches!(scope, ConfigScope::System),
    )
}

fn resolve_selected_signer(
    git_repo: &Option<&Repo>,
    selector: &str,
    password: &Option<String>,
) -> Result<SignerInfo> {
    let expected_npub = resolve_selector_npub(git_repo, selector)?;
    let mut store_error = None;

    match credential_store::retrieve(&expected_npub) {
        Ok(keys) => {
            return Ok(SignerInfo::Nsec {
                nsec: keys.secret_key().to_bech32()?,
                password: password.clone(),
                npub: Some(expected_npub),
                verify_npub: true,
            });
        }
        Err(error) => match &error {
            credential_store::LookupError::Missing(_) => {}
            credential_store::LookupError::Unavailable(_) => {
                store_error = Some(anyhow::Error::new(error));
            }
            credential_store::LookupError::Invalid(_) => {
                return Err(anyhow::Error::new(error));
            }
        },
    }

    // Every nsec source is considered before any bunker source. A matching
    // but unusable nsec profile fails instead of silently switching methods.
    for scope in selection_scopes(git_repo) {
        let Some(nsec) = matching_nsec_config(git_repo, scope, &expected_npub)? else {
            continue;
        };
        return Ok(SignerInfo::Nsec {
            nsec: resolve_scope_secret(git_repo, scope, &nsec, true)?,
            password: password.clone(),
            npub: Some(expected_npub),
            verify_npub: true,
        });
    }

    if let Some(error) = store_error.take() {
        return Err(error.context(format!(
            "failed to resolve nsec for selected signer {expected_npub}"
        )));
    }

    match credential_store::retrieve_bunker_signer(&expected_npub) {
        Ok(record) => {
            return Ok(SignerInfo::Bunker {
                bunker_uri: record.bunker_uri,
                bunker_app_key: record.client_nsec,
                npub: Some(expected_npub),
            });
        }
        Err(error) => match &error {
            credential_store::LookupError::Missing(_) => {}
            credential_store::LookupError::Unavailable(_) => {
                store_error = Some(anyhow::Error::new(error));
            }
            credential_store::LookupError::Invalid(_) => {
                return Err(anyhow::Error::new(error));
            }
        },
    }

    if let Some(error) = store_error.take() {
        return Err(error.context(format!(
            "failed to resolve bunker record for selected signer {expected_npub}"
        )));
    }

    // Legacy flat bunker fields remain readable, but a profile is only a
    // candidate when all of its fields come from the same scope.
    for scope in selection_scopes(git_repo) {
        if config_value(git_repo, scope, "nostr.npub")?.as_deref() != Some(&expected_npub) {
            continue;
        }
        let Some(bunker_uri) = config_value(git_repo, scope, "nostr.bunker-uri")? else {
            continue;
        };
        let app_key =
            config_value(git_repo, scope, "nostr.bunker-app-key")?.with_context(|| {
                format!(
                    "{} git config has a matching bunker URI but no bunker app key",
                    scope.label()
                )
            })?;
        return Ok(SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key: resolve_scope_secret(git_repo, scope, &app_key, false)?,
            npub: Some(expected_npub),
        });
    }

    bail!("selected signer {expected_npub} is not available in the credential store or git config")
}

fn resolve_selector_npub(git_repo: &Option<&Repo>, selector: &str) -> Result<String> {
    if selector.starts_with("npub1") {
        return PublicKey::parse(selector)
            .context("--signer contains an invalid npub")?
            .to_bech32()
            .map_err(Into::into);
    }
    let alias = credential_store::normalize_alias(selector)?;
    let key = format!("nostr.signer-alias.{alias}");
    for scope in selection_scopes(git_repo) {
        if let Some(npub) = config_value(git_repo, scope, &key)? {
            return PublicKey::parse(&npub)
                .with_context(|| {
                    format!(
                        "{} git config maps alias '{alias}' to an invalid npub",
                        scope.label()
                    )
                })?
                .to_bech32()
                .map_err(Into::into);
        }
    }
    credential_store::retrieve_alias(&alias).map_err(|error| match error {
        credential_store::LookupError::Missing(_) => anyhow::anyhow!(
            "signer alias '{alias}' is not defined in git config or the credential store"
        ),
        error => anyhow::Error::new(error),
    })
}

fn matching_nsec_config(
    git_repo: &Option<&Repo>,
    scope: ConfigScope,
    expected_npub: &str,
) -> Result<Option<String>> {
    let Some(nsec) = config_value(git_repo, scope, "nostr.nsec")? else {
        return Ok(None);
    };
    if config_value(git_repo, scope, "nostr.npub")?.as_deref() == Some(expected_npub)
        || credential_store::parse_pointer(&nsec) == Some(expected_npub)
    {
        return Ok(Some(nsec));
    }
    if let Ok(keys) = nostr::prelude::Keys::parse(&nsec) {
        if keys.public_key().to_bech32().as_deref() == Ok(expected_npub) {
            return Ok(Some(nsec));
        }
    }
    Ok(None)
}

fn resolve_config_secret(
    git_repo: &Option<&Repo>,
    value: &str,
    is_nsec: bool,
    hint_if_plaintext: bool,
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
        credential_store::ConfigSecret::Plaintext(value) => {
            if hint_if_plaintext {
                hint_plaintext_secret(git_repo);
            }
            Ok(value.to_string())
        }
    }
}

/// Plaintext git-config secrets are read as-is and never auto-migrated: a
/// read path that rewrites credential storage nags on every command when no
/// store is available and can strand the only copy of a key inside a
/// sandboxed environment's store. Interactive users get a once-per-run nudge
/// towards `ngit account login` instead; system-config and CLI-supplied
/// secrets never hint.
fn hint_plaintext_secret(git_repo: &Option<&Repo>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HINTED: AtomicBool = AtomicBool::new(false);
    if Interactor::is_non_interactive()
        || credential_store::policy(git_repo) == credential_store::SecretStorage::GitConfig
        || HINTED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    eprintln!(
        "hint: this login's nostr secret is stored in plaintext in git config; run `ngit account login` to move it into a credential store, or set `git config --global nostr.secret-storage git-config` to keep it there and silence this hint."
    );
}

async fn get_signer(
    signer_info: &SignerInfo,
    prompt_for_ncryptsec_password: bool,
) -> Result<(Arc<crate::NgitSigner>, PublicKey)> {
    match signer_info {
        SignerInfo::Nsec {
            nsec,
            password,
            npub,
            verify_npub,
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
                nostr::prelude::Keys::from_str(nsec).context("invalid nsec parameter")?
            };
            let public_key = keys.public_key();
            if *verify_npub {
                let expected = npub
                    .as_deref()
                    .context("selected nsec has no expected npub")?;
                let expected = PublicKey::parse(expected).context("invalid configured npub")?;
                if public_key != expected {
                    bail!("selected nsec belongs to a different npub");
                }
            }
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
                nostr::prelude::Keys::from_str(bunker_app_key).context("invalid app key")?,
                Duration::from_secs(10 * 60),
                None,
            )?;
            if let Some(public_key) = npub.clone().and_then(|npub| PublicKey::parse(&npub).ok()) {
                // This key was learned during initial pairing and persisted
                // with the connection. Seed NostrConnect's cache so normal
                // commands do not prompt for a redundant identity request.
                // NgitSigner validates every real signed response instead.
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
        SignerInfo::Selection { .. } => {
            bail!("internal error: unresolved signer selection reached signer construction")
        }
    }
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{Keys, ToBech32};

    use super::*;
    use crate::git::{Repo, RepoActions, test_helpers::GitTestRepo};

    #[test]
    fn local_alias_resolves_matching_nsec() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        repo.save_git_config_item("nostr.signer-alias.fred", &npub, false)?;
        repo.save_git_config_item("nostr.npub", &npub, false)?;
        repo.save_git_config_item("nostr.nsec", &keys.secret_key().to_bech32()?, false)?;

        let selection = SignerInfo::Selection {
            selector: "fred".to_string(),
        };
        let (info, source) = get_signer_info(
            &Some(&repo),
            &Some(selection),
            &None,
            &Some(SignerInfoSource::CommandLineArguments),
        )?;
        assert_eq!(source, SignerInfoSource::CommandLineArguments);
        assert!(matches!(
            info,
            SignerInfo::Nsec { npub: Some(selected), .. } if selected == npub
        ));
        Ok(())
    }

    #[test]
    fn alias_mapping_does_not_accept_a_different_nsec() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let expected = Keys::generate().public_key().to_bech32()?;
        let other = Keys::generate();
        repo.save_git_config_item("nostr.signer-alias.fred", &expected, false)?;
        repo.save_git_config_item("nostr.npub", &other.public_key().to_bech32()?, false)?;
        repo.save_git_config_item("nostr.nsec", &other.secret_key().to_bech32()?, false)?;

        let error = resolve_selected_signer(&Some(&repo), "fred", &None).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("not available") || message.contains("credential store"),
            "unexpected error: {message}"
        );
        Ok(())
    }

    #[test]
    fn malformed_selected_alias_fails_closed() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        repo.save_git_config_item("nostr.signer", "not/a/portable-alias", false)?;
        repo.save_git_config_item(
            "nostr.nsec",
            &Keys::generate().secret_key().to_bech32()?,
            false,
        )?;

        let error = get_signer_info(&Some(&repo), &None, &None, &None).unwrap_err();
        assert!(format!("{error:#}").contains("signer alias"));
        Ok(())
    }

    #[test]
    fn nsec_precedes_bunker_for_same_selected_npub() -> Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32()?;
        repo.save_git_config_item("nostr.npub", &npub, false)?;
        repo.save_git_config_item("nostr.nsec", &keys.secret_key().to_bech32()?, false)?;
        repo.save_git_config_item(
            "nostr.bunker-uri",
            &format!("bunker://{}", Keys::generate().public_key()),
            false,
        )?;
        repo.save_git_config_item(
            "nostr.bunker-app-key",
            &Keys::generate().secret_key().to_bech32()?,
            false,
        )?;

        let info = resolve_selected_signer(&Some(&repo), &npub, &None)?;
        assert!(matches!(info, SignerInfo::Nsec { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn configured_nsec_npub_is_verified_on_construction() -> Result<()> {
        let info = SignerInfo::Nsec {
            nsec: Keys::generate().secret_key().to_bech32()?,
            password: None,
            npub: Some(Keys::generate().public_key().to_bech32()?),
            verify_npub: true,
        };
        let error = get_signer(&info, false).await.unwrap_err();
        assert!(error.to_string().contains("different npub"));
        Ok(())
    }
}
