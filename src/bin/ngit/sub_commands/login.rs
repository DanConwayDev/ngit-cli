use anyhow::{Context, Result};
use clap;
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    client::Params,
    git::{get_git_config_item, remove_git_config_item},
    login::{
        SignerInfo, SignerInfoSource, credential_store,
        existing::{get_signer_info, load_existing_login, selected_alias},
        logged_in_message,
    },
};
use nostr::prelude::{FromBech32, Keys, NostrConnectUri, PublicKey, ToBech32};

use crate::{
    cli::SignerParams,
    client::{Client, Connect},
    git::Repo,
    login::fresh::{fresh_login_or_signup, login_with_bunker_url},
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// login to the local git repository only
    #[arg(long, action)]
    local: bool,

    /// don't fetch user metadata and relay list from relays
    #[arg(long, action)]
    offline: bool,

    /// signer relay for nostrconnect (can be used multiple times)
    #[arg(long = "signer-relay")]
    signer_relays: Vec<String>,

    /// bunker:// URL from signer app for non-interactive remote signer login
    #[arg(
        long = "bunker-url",
        conflicts_with_all = ["nsec", "nsec_file", "signer", "bunker_uri", "bunker_app_key"]
    )]
    bunker_url: Option<String>,

    /// where to store the account secret: auto (OS credential store, falling
    /// back to ngit's file store), file, or git-config (plaintext)
    #[arg(long, value_name = "auto|file|git-config")]
    secret_storage: Option<String>,

    /// save or reactivate a reusable name for an account
    #[arg(long, value_name = "ALIAS")]
    alias: Option<String>,
}

pub async fn launch(command_args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    if let Some(value) = &command_args.secret_storage {
        let policy = credential_store::parse_policy(value).with_context(|| {
            format!("invalid --secret-storage value '{value}'; expected auto, file or git-config")
        })?;
        credential_store::set_policy_override(policy);
    }
    let alias = command_args
        .alias
        .as_deref()
        .map(credential_store::normalize_alias)
        .transpose()?;
    let selected_alias = match signer.info {
        Some(ngit::login::SignerInfo::Selection { selector }) if !selector.starts_with("npub1") => {
            Some(credential_store::normalize_alias(selector)?)
        }
        _ => None,
    };
    let login_alias = alias.as_deref().or(selected_alias.as_deref());
    // Early validation: check if we have required parameters in non-interactive
    // mode
    if Interactor::is_non_interactive()
        && signer.info.is_none()
        && command_args.bunker_url.is_none()
        && alias.is_none()
    {
        use ngit::cli_interactor::cli_error;
        return Err(cli_error(
            "requires a new secret, a stored signer, or interactive login",
            &[
                ("--nsec <key>", "provide secret key (nsec or hex)"),
                ("--bunker-url <url>", "bunker:// URL from signer app"),
                ("--signer <npub|alias>", "reactivate a stored signer"),
                ("--alias <alias>", "reactivate an existing stored alias"),
                ("--interactive", "for interactive nostr connect login"),
            ],
            &[
                "ngit account login --nsec <your-nsec>",
                "ngit account login --bunker-url <bunker-url>",
                "ngit account login --local --alias <stored-alias>",
                "ngit account create",
            ],
        ));
    }

    let git_repo = discover_login_repo(command_args.local)?;

    let (signer_for_login, selected_by) = resolve_login_selection(
        git_repo.as_ref(),
        signer.info.as_ref(),
        signer.password.as_ref(),
        alias.as_deref(),
        command_args.bunker_url.is_some(),
    )
    .await?;
    let validated_npub = validate_signer_before_switch(
        signer_for_login.as_ref(),
        command_args.bunker_url.as_deref(),
    )?;
    if let (Some(alias), Some(npub)) = (login_alias, validated_npub.as_deref()) {
        credential_store::ensure_alias_available(alias, npub)?;
    }

    let client = if command_args.offline {
        None
    } else {
        Some(Client::new(Params::with_git_config_relay_defaults(
            &git_repo.as_ref(),
        )))
    };

    let (logged_out, log_in_locally_only) = logout(git_repo.as_ref(), command_args.local).await?;
    if logged_out || log_in_locally_only {
        let save_local = log_in_locally_only || command_args.local;
        if let Some(bunker_url) = &command_args.bunker_url {
            login_with_bunker_url(
                &git_repo.as_ref(),
                client.as_ref(),
                bunker_url,
                save_local,
                &command_args.signer_relays,
                login_alias,
            )
            .await?;
        } else {
            fresh_login_or_signup(
                &git_repo.as_ref(),
                client.as_ref(),
                signer_for_login,
                save_local,
                &command_args.signer_relays,
                login_alias,
                selected_by.as_deref(),
            )
            .await?;
        }
    }

    // If not offline, disconnect the client
    if let Some(client) = client {
        client.disconnect().await?;
    }
    Ok(())
}

fn discover_login_repo(local: bool) -> Result<Option<Repo>> {
    let git_repo = Repo::discover().ok();
    if local && git_repo.is_none() {
        use ngit::cli_interactor::cli_error;
        return Err(cli_error(
            "cannot log in locally outside a git repository",
            &[],
            &[
                "run this command inside a git repository",
                "omit --local to log in globally",
            ],
        ));
    }
    Ok(git_repo)
}

fn validate_signer_before_switch(
    signer_info: Option<&SignerInfo>,
    bunker_url: Option<&str>,
) -> Result<Option<String>> {
    if let Some(bunker_url) = bunker_url {
        NostrConnectUri::parse(bunker_url).context("invalid --bunker-url")?;
    }
    let Some(signer_info) = signer_info else {
        return Ok(None);
    };
    match signer_info {
        SignerInfo::Nsec {
            nsec,
            password,
            npub,
            verify_npub,
            ..
        } => {
            let expected = npub
                .as_deref()
                .map(PublicKey::parse)
                .transpose()
                .context("invalid signer npub")?;
            if nsec.starts_with("ncryptsec1") {
                let encrypted = nostr::nips::nip49::EncryptedSecretKey::from_bech32(nsec)
                    .context("invalid ncryptsec parameter")?;
                if let Some(password) = password {
                    let public_key = Keys::new(
                        encrypted
                            .decrypt(password)
                            .context("failed to decrypt ncryptsec with provided password")?,
                    )
                    .public_key();
                    if *verify_npub && expected.is_some_and(|expected| expected != public_key) {
                        anyhow::bail!("selected nsec belongs to a different npub");
                    }
                    return Ok(Some(public_key.to_bech32()?));
                } else if Interactor::is_non_interactive() {
                    anyhow::bail!(
                        "an encrypted nsec requires --password for non-interactive login"
                    );
                }
                return Ok(expected
                    .map(|public_key| public_key.to_bech32())
                    .transpose()?);
            }
            let public_key = Keys::parse(nsec)
                .context("invalid nsec parameter")?
                .public_key();
            if *verify_npub && expected.is_some_and(|expected| expected != public_key) {
                anyhow::bail!("selected nsec belongs to a different npub");
            }
            Ok(Some(public_key.to_bech32()?))
        }
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            npub,
            ..
        } => {
            NostrConnectUri::parse(bunker_uri).context("invalid bunker URI")?;
            Keys::parse(bunker_app_key).context("invalid bunker app key")?;
            let public_key = npub
                .as_deref()
                .map(PublicKey::parse)
                .transpose()
                .context("invalid signer npub")?;
            Ok(public_key
                .map(|public_key| public_key.to_bech32())
                .transpose()?)
        }
        SignerInfo::Selection { .. } => {
            anyhow::bail!("internal error: signer selection was not resolved before login")
        }
    }
}

async fn resolve_login_selection(
    git_repo: Option<&Repo>,
    signer_info: Option<&SignerInfo>,
    password: Option<&String>,
    alias: Option<&str>,
    has_bunker_url: bool,
) -> Result<(Option<SignerInfo>, Option<String>)> {
    let mut requested = signer_info.cloned().or_else(|| {
        (!has_bunker_url).then(|| {
            alias.map(|alias| SignerInfo::Selection {
                selector: alias.to_string(),
            })
        })?
    });
    if let Some(SignerInfo::Nsec {
        password: signer_password,
        ..
    }) = &mut requested
    {
        if signer_password.is_none() {
            *signer_password = password.cloned();
        }
    }
    let Some(SignerInfo::Selection { selector }) = &requested else {
        return Ok((requested, None));
    };
    let selected_by = selector.clone();
    // Resolve before removing the current login. Its only signer material may
    // be in the Git-config scope that account switching is about to clear.
    let resolved = get_signer_info(
        &git_repo,
        &requested,
        &password.cloned(),
        &Some(SignerInfoSource::CommandLineArguments),
    )
    .await?
    .0;
    Ok((Some(resolved), Some(selected_by)))
}

/// return ( bool - logged out, bool - log in to local git locally)
#[allow(clippy::too_many_lines)]
async fn logout(git_repo: Option<&Repo>, local_only: bool) -> Result<(bool, bool)> {
    for source in if local_only || std::env::var("NGITTEST").is_ok() {
        vec![SignerInfoSource::GitLocal]
    } else {
        vec![SignerInfoSource::GitLocal, SignerInfoSource::GitGlobal]
    } {
        if let Ok((_, user_ref, source)) = load_existing_login(
            &git_repo,
            &None,
            &None,
            &Some(source),
            None,
            true,
            false,
            false,
        )
        .await
        {
            // In non-interactive mode, automatically logout without prompting
            // Stored secrets are deliberately kept when switching accounts:
            // the credential store may hold the only copy of the key, and a
            // re-login of the same account reuses its entry. `ngit account
            // forget-keys` removes entries explicitly.
            if Interactor::is_non_interactive() {
                for item in [
                    "nostr.nsec",
                    "nostr.npub",
                    "nostr.bunker-uri",
                    "nostr.bunker-app-key",
                    "nostr.signer",
                ] {
                    if let Err(_error) = remove_git_config_item(
                        if source == SignerInfoSource::GitLocal {
                            &git_repo
                        } else {
                            &None
                        },
                        item,
                    ) {
                        use ngit::cli_interactor::cli_error;
                        return Err(cli_error(
                            &format!(
                                "failed to edit {} git config item '{item}'",
                                if source == SignerInfoSource::GitGlobal {
                                    "global"
                                } else {
                                    "local"
                                },
                            ),
                            &[],
                            &["ngit account login --local --nsec <your-nsec>"],
                        ));
                    }
                }
                return Ok((true, local_only));
            }

            // Interactive mode: prompt user for what to do
            let alias = selected_alias(&git_repo, &None, &source)?;
            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_prompt(logged_in_message(
                        &user_ref.metadata.name,
                        &source,
                        alias.as_deref(),
                    ))
                    .with_choices(if source == SignerInfoSource::GitGlobal {
                        vec![
                            "logout".to_string(),
                            "remain logged in".to_string(),
                            "login to local git repo only as another user".to_string(),
                        ]
                    } else {
                        vec![
                            alias.as_ref().map_or_else(
                                || format!("logout as \"{}\"", user_ref.metadata.name),
                                |alias| format!("logout as signer alias '{alias}'"),
                            ),
                            "remain logged in".to_string(),
                        ]
                    }),
            )? {
                0 => {
                    for item in [
                        "nostr.nsec",
                        "nostr.npub",
                        "nostr.bunker-uri",
                        "nostr.bunker-app-key",
                        "nostr.signer",
                    ] {
                        if let Err(error) = remove_git_config_item(
                            if source == SignerInfoSource::GitLocal {
                                &git_repo
                            } else {
                                &None
                            },
                            item,
                        ) {
                            eprintln!("{error:?}");

                            eprintln!(
                                "consider manually removing {} git config items: {}",
                                if source == SignerInfoSource::GitGlobal {
                                    "global"
                                } else {
                                    "local"
                                },
                                format_items_as_list(&get_global_login_config_items_set())
                            );
                            match Interactor::default().choice(
                                PromptChoiceParms::default().with_default(0)
                                .with_prompt("failed to remove the signer configuration from global Git config")
                                .with_choices(
                                    vec![
                                        "continue with global login to reveal what git config items to manually set".to_string(),
                                        "login to this local repository with a different account".to_string(),
                                        "cancel".to_string(),
                                    ]
                                ),
                            )? {
                                0 => return Ok((true, false)),
                                1 => return Ok((true, true)),
                                _ => return Ok((false, local_only)),
                            }
                        }
                    }
                }
                1 => return Ok((false, local_only)),
                _ => return Ok((false, true)),
            }
        }
    }
    Ok((true, local_only))
}

pub fn get_global_login_config_items_set() -> Vec<&'static str> {
    [
        "nostr.nsec",
        "nostr.npub",
        "nostr.bunker-uri",
        "nostr.bunker-app-key",
        "nostr.signer",
    ]
    .iter()
    .copied()
    .filter(|item| get_git_config_item(&None, item).is_ok_and(|item| item.is_some()))
    .collect::<Vec<&str>>()
}

pub fn format_items_as_list(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let all_but_last = items[..items.len() - 1].join(", ");
            format!("{}, and {}", all_but_last, items[items.len() - 1])
        }
    }
}
