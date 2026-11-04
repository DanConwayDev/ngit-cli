use std::fmt::Write as _;

use anyhow::{Context, Result};
use clap;
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptChoiceParms},
    client::Params,
    login::{
        SignerInfo, SignerInfoSource, credential_store,
        existing::{
            configured_alias_names, load_existing_login, resolve_selection, selected_alias,
        },
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
    /// stored account to activate (full npub, profile, or exact Nostr display
    /// name)
    #[arg(
        value_name = "ACCOUNT",
        conflicts_with_all = [
            "nsec",
            "nsec_file",
            "nbunksec",
            "nbunksec_file",
            "signer",
            "bunker_uri",
            "bunker_app_key",
            "bunker_url"
        ]
    )]
    account: Option<String>,

    /// set a repository-specific login that overrides the default account
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
        conflicts_with_all = [
            "account",
            "nsec",
            "nsec_file",
            "nbunksec",
            "nbunksec_file",
            "signer",
            "bunker_uri",
            "bunker_app_key"
        ]
    )]
    bunker_url: Option<String>,

    /// where to store the account secret: auto (OS credential store, falling
    /// back to ngit's file store), file, or git-config (plaintext)
    #[arg(long, value_name = "auto|file|git-config")]
    secret_storage: Option<String>,

    /// save or reactivate a named signer profile for an account
    #[arg(long = "profile", alias = "alias", value_name = "NAME")]
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
    let account_selection = positional_account_selection(command_args);
    let signer_info = account_selection.as_ref().or(signer.info.as_ref());
    // Early validation: check if we have required parameters in non-interactive
    // mode
    if Interactor::is_non_interactive()
        && signer_info.is_none()
        && command_args.bunker_url.is_none()
        && alias.is_none()
    {
        return Err(missing_login_error());
    }

    let git_repo = discover_login_repo(command_args.local)?;

    let (signer_for_login, selected_by, selected_alias) = resolve_login_selection(
        git_repo.as_ref(),
        signer_info,
        signer.password.as_ref(),
        alias.as_deref(),
        command_args.bunker_url.is_some(),
        !Interactor::is_non_interactive(),
    )
    .await?;
    let login_alias = alias.as_deref().or(selected_alias.as_deref());
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

    let (proceed, log_in_locally_only) = choose_login_scope(
        git_repo.as_ref(),
        command_args.local,
        login_alias,
        selected_by.is_none(),
    )
    .await?;
    if proceed {
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

fn missing_login_error() -> anyhow::Error {
    ngit::cli_interactor::cli_error(
        "requires a new secret, a stored signer, or interactive login",
        &[
            (
                "ACCOUNT",
                "reactivate by full npub, profile, or exact Nostr display name",
            ),
            ("--nsec <key>", "provide secret key (nsec or hex)"),
            (
                "--nbunksec-file <path>",
                "provide an established remote signer connection",
            ),
            ("--bunker-url <url>", "bunker:// URL from signer app"),
            (
                "--signer <alias|npub|nostr-display-name>",
                "reactivate a stored signer",
            ),
            ("--profile <name>", "reactivate an existing stored profile"),
            ("--interactive", "for interactive nostr connect login"),
        ],
        &[
            "ngit account login <account>",
            "ngit account login --nsec <your-nsec>",
            "ngit account login --nbunksec-file <path>",
            "ngit account login --bunker-url <bunker-url>",
            "ngit account login --local --profile <stored-profile>",
            "ngit account create",
        ],
    )
}

fn positional_account_selection(command_args: &SubCommandArgs) -> Option<SignerInfo> {
    command_args
        .account
        .as_ref()
        .map(|selector| SignerInfo::Selection {
            selector: selector.clone(),
        })
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
                    .context(ngit::login::INVALID_SIGNING_KEY)?;
                if let Some(password) = password {
                    let public_key = Keys::new(
                        encrypted
                            .decrypt(password)
                            .context(ngit::login::SIGNING_KEY_UNLOCK_FAILED)?,
                    )
                    .public_key();
                    if *verify_npub && expected.is_some_and(|expected| expected != public_key) {
                        anyhow::bail!(ngit::login::SIGNING_ACCOUNT_MISMATCH);
                    }
                    return Ok(Some(public_key.to_bech32()?));
                } else if Interactor::is_non_interactive() {
                    anyhow::bail!(ngit::login::SIGNING_KEY_PASSWORD_REQUIRED);
                }
                return Ok(expected
                    .map(|public_key| public_key.to_bech32())
                    .transpose()?);
            }
            let public_key = Keys::parse(nsec)
                .context(ngit::login::INVALID_SIGNING_KEY)?
                .public_key();
            if *verify_npub && expected.is_some_and(|expected| expected != public_key) {
                anyhow::bail!(ngit::login::SIGNING_ACCOUNT_MISMATCH);
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
    interactive: bool,
) -> Result<(Option<SignerInfo>, Option<String>, Option<String>)> {
    // Positional and --signer selectors may fall back to cached profile names;
    // --profile is strictly the named signer namespace. In an explicitly
    // interactive login it labels the fresh signer instead of reactivating an
    // existing alias; positional ACCOUNT and --signer remain explicit
    // selectors.
    let from_explicit_selector = matches!(signer_info, Some(SignerInfo::Selection { .. }));
    let mut requested = signer_info
        .cloned()
        .or_else(|| implicit_alias_selection(alias, has_bunker_url, interactive));
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
        return Ok((requested, None, None));
    };
    // Resolve before removing the current login. Its only signer material may
    // be in the Git-config scope that account switching is about to clear.
    // Mutable profile names are resolved once, here: only the canonical npub
    // (or a matched alias) is handed on for persistence.
    let resolved = resolve_selection(
        &git_repo,
        selector,
        &password.cloned(),
        from_explicit_selector,
    )
    .await?;
    Ok((
        Some(resolved.signer_info),
        Some(resolved.npub),
        resolved.alias,
    ))
}

fn implicit_alias_selection(
    alias: Option<&str>,
    has_bunker_url: bool,
    interactive: bool,
) -> Option<SignerInfo> {
    if has_bunker_url || interactive {
        return None;
    }
    alias.map(|alias| SignerInfo::Selection {
        selector: alias.to_string(),
    })
}

/// Decide where to save a replacement without clearing the working login.
/// Authentication and credential storage can fail or be cancelled afterwards.
async fn choose_login_scope(
    git_repo: Option<&Repo>,
    local_only: bool,
    target_profile: Option<&str>,
    fresh_sign_in: bool,
) -> Result<(bool, bool)> {
    if Interactor::is_non_interactive() {
        return Ok((true, local_only));
    }
    let source = if local_only || std::env::var("NGITTEST").is_ok() {
        SignerInfoSource::GitLocal
    } else {
        SignerInfoSource::GitGlobal
    };
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
        let alias = selected_alias(&git_repo, &source)?;
        // Inventory failure only changes the wording: do not turn an optional
        // presentation lookup into a new authentication prerequisite.
        let target_exists = target_profile.and_then(|target| {
            configured_alias_names(&git_repo)
                .ok()
                .map(|names| names.contains(target))
        });
        let current = alias.as_deref().unwrap_or(&user_ref.metadata.name);
        let prompt = login_scope_prompt(
            current,
            target_profile,
            source == SignerInfoSource::GitLocal,
            source == SignerInfoSource::GitGlobal && git_repo.is_some(),
            fresh_sign_in,
            target_exists,
        );
        let choice = Interactor::default().choice(
            PromptChoiceParms::default()
                .with_default(0)
                .with_prompt(prompt.0)
                .with_choices(prompt.1),
        )?;
        return Ok(login_scope_choice(choice, local_only));
    }
    Ok((true, local_only))
}

/// Keep the choice-to-action mapping independent of terminal interaction.
/// In particular, keeping the current selection cancels before authentication.
fn login_scope_choice(choice: usize, local_only: bool) -> (bool, bool) {
    match choice {
        0 => (true, local_only),
        1 => (false, local_only),
        _ => (true, true),
    }
}

fn login_scope_prompt(
    current: &str,
    target: Option<&str>,
    local: bool,
    offer_local: bool,
    fresh_sign_in: bool,
    target_exists: Option<bool>,
) -> (String, Vec<String>) {
    let scope = if local {
        "repository override"
    } else {
        "global default"
    };
    let target_label = target.unwrap_or("the new profile");
    let mut message = format!("Current {scope}: {current}");
    message.push_str("\nOther profiles in the credential store are kept.");
    if fresh_sign_in {
        match (target, target_exists) {
            (Some(name), Some(true)) => {
                let _ = write!(
                    message,
                    "\nSigning in as {name} may replace that profile's saved signer connection."
                );
            }
            (Some(name), None) => {
                let _ = write!(
                    message,
                    "\nIf {name} already exists, signing in may replace its saved signer connection."
                );
            }
            (None, _) => message.push_str(
                "\nA new remote sign-in may replace this account's default remote session.",
            ),
            _ => {}
        }
    }
    message.push_str("\nKeeping the current selection cancels sign-in; no new profile is saved.");
    let mut choices = vec![
        format!("Make {target_label} the {scope} after signing in"),
        format!("Keep {current} as the {scope} (cancel sign-in)"),
    ];
    if offer_local {
        choices.push(format!(
            "Use {target_label} in this repository only; keep the global default"
        ));
    }
    (message, choices)
}

#[cfg(test)]
mod tests {
    use ngit::login::SignerInfo;

    use super::{implicit_alias_selection, login_scope_choice, login_scope_prompt};

    #[test]
    fn interactive_alias_labels_a_fresh_signer() {
        assert!(implicit_alias_selection(Some("dcdev"), false, true).is_none());
    }

    #[test]
    fn non_interactive_alias_reactivates_the_stored_signer() {
        assert!(matches!(
            implicit_alias_selection(Some("dcdev"), false, false),
            Some(SignerInfo::Selection { selector }) if selector == "dcdev"
        ));
    }

    #[test]
    fn alias_labels_an_explicit_bunker_login() {
        assert!(implicit_alias_selection(Some("dcdev"), true, false).is_none());
    }
    #[test]
    fn named_global_switch_explains_selection_and_cancellation() {
        let (message, choices) =
            login_scope_prompt("dcdroid", Some("dcdev"), false, true, true, Some(false));
        assert!(message.contains("Current global default: dcdroid"));
        assert!(message.contains("Other profiles in the credential store are kept"));
        assert!(message.contains("no new profile is saved"));
        assert!(!message.contains("may replace"));
        assert_eq!(choices[0], "Make dcdev the global default after signing in");
        assert_eq!(
            choices[1],
            "Keep dcdroid as the global default (cancel sign-in)"
        );
        assert_eq!(choices.len(), 3);
        assert_eq!(login_scope_choice(0, false), (true, false));
        assert_eq!(login_scope_choice(1, false), (false, false));
        assert_eq!(login_scope_choice(2, false), (true, true));
    }

    #[test]
    fn local_switch_and_existing_profile_pairing_are_explicit() {
        let (message, choices) =
            login_scope_prompt("dcagent", Some("dcdev"), true, false, true, Some(true));
        assert!(message.contains("Current repository override: dcagent"));
        assert!(
            message
                .contains("Signing in as dcdev may replace that profile's saved signer connection")
        );
        assert_eq!(
            choices[0],
            "Make dcdev the repository override after signing in"
        );
        assert_eq!(choices.len(), 2);
        assert_eq!(login_scope_choice(0, true), (true, true));
        assert_eq!(login_scope_choice(1, true), (false, true));
    }

    #[test]
    fn unnamed_and_unknown_targets_do_not_promise_session_retention() {
        let (message, choices) = login_scope_prompt("dcdroid", None, false, false, true, None);
        assert!(message.contains("may replace this account's default remote session"));
        assert_eq!(
            choices[0],
            "Make the new profile the global default after signing in"
        );
        let (message, _) = login_scope_prompt("dcdroid", Some("dcdev"), false, false, true, None);
        assert!(message.contains("If dcdev already exists"));
        let (message, _) =
            login_scope_prompt("dcdroid", Some("dcdev"), false, false, false, Some(true));
        assert!(!message.contains("may replace"));
    }
}
