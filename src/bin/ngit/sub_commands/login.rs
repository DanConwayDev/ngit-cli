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
    login::fresh::{
        LoginTarget, fresh_login_or_signup, login_with_bunker_url, login_with_nostrconnect,
    },
};

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct SubCommandArgs {
    /// Stored account to activate (full npub, profile, or exact Nostr display
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

    /// Set a repository-specific login that overrides the default account
    #[arg(long, action)]
    local: bool,

    /// Save a named profile without changing the local or global login
    #[arg(long, requires = "alias", conflicts_with = "local")]
    no_activate: bool,

    /// Don't fetch user metadata and relay list from relays
    #[arg(long, action)]
    offline: bool,

    /// Signer relay for nostrconnect (can be used multiple times)
    #[arg(long = "signer-relay")]
    signer_relays: Vec<String>,

    /// Bunker:// URL from signer app for non-interactive remote signer login
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

    /// Print a nostrconnect:// URI and wait for pairing without terminal input
    #[arg(
        long,
        conflicts_with_all = [
            "interactive", "account", "nsec", "nsec_file", "nbunksec",
            "nbunksec_file", "signer", "bunker_uri", "bunker_app_key", "bunker_url"
        ]
    )]
    nostrconnect: bool,

    /// Where to store the account secret: auto (OS credential store, falling
    /// back to ngit's file store), file, or git-config (plaintext)
    #[arg(long, value_name = "auto|file|git-config")]
    secret_storage: Option<String>,

    /// Save or reactivate a named signer profile for an account
    #[arg(long = "profile", alias = "alias", value_name = "NAME")]
    alias: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(command_args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    if command_args.nostrconnect {
        // This explicit flow never reads stdin, even through the `connect`
        // alias or an inherited interactive-mode environment variable.
        std::env::remove_var("NGIT_INTERACTIVE_MODE");
    }
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
        && !command_args.nostrconnect
        && alias.is_none()
    {
        return Err(missing_login_error());
    }

    let git_repo = discover_login_repo(command_args.local)?;
    anyhow::ensure!(
        !command_args.no_activate
            || credential_store::policy(&git_repo.as_ref())
                != credential_store::SecretStorage::GitConfig,
        "--no-activate requires auto or file secret storage; use --secret-storage auto"
    );

    let (signer_for_login, selected_by, selected_alias) = resolve_login_selection(
        git_repo.as_ref(),
        signer_info,
        signer.password.as_ref(),
        alias.as_deref(),
        command_args.bunker_url.is_some() || command_args.nostrconnect,
        !Interactor::is_non_interactive(),
    )
    .await?;
    anyhow::ensure!(
        command_args.signer_relays.is_empty()
            || command_args.nostrconnect
            || (!Interactor::is_non_interactive()
                && signer_info.is_none()
                && command_args.bunker_url.is_none()),
        "--signer-relay is only used for new nostrconnect pairing; it cannot change a saved connection. Use `account login --nostrconnect --profile NAME --no-activate --signer-relay URL` to replace a profile without changing the default login. For --bunker-url, use the relays in the bunker URL"
    );
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

    let (proceed, log_in_locally_only) = if command_args.nostrconnect || command_args.no_activate {
        (true, command_args.local)
    } else {
        choose_login_scope(
            git_repo.as_ref(),
            command_args.local,
            login_alias,
            selected_by.is_none(),
        )
        .await?
    };
    if proceed {
        let target = if command_args.no_activate {
            LoginTarget::ProfileOnly
        } else if log_in_locally_only || command_args.local {
            LoginTarget::Local
        } else {
            LoginTarget::Global
        };
        if command_args.nostrconnect {
            login_with_nostrconnect(
                &git_repo.as_ref(),
                client.as_ref(),
                target,
                &command_args.signer_relays,
                login_alias,
            )
            .await?;
        } else if let Some(bunker_url) = &command_args.bunker_url {
            login_with_bunker_url(
                &git_repo.as_ref(),
                client.as_ref(),
                bunker_url,
                target,
                &command_args.signer_relays,
                login_alias,
            )
            .await?;
        } else {
            fresh_login_or_signup(
                &git_repo.as_ref(),
                client.as_ref(),
                signer_for_login,
                target,
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
                "--nostrconnect",
                "print a pairing URI without terminal prompts",
            ),
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
    fresh_remote_login: bool,
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
        .or_else(|| implicit_alias_selection(alias, fresh_remote_login, interactive));
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
    fresh_remote_login: bool,
    interactive: bool,
) -> Option<SignerInfo> {
    if fresh_remote_login || interactive {
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
        let current_saved = alias.as_ref().is_some_and(|name| {
            credential_store::inventory()
                .is_ok_and(|inventory| inventory.aliases.contains_key(name))
        });
        let prompt = login_scope_prompt(
            current,
            current_saved.then_some(current),
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
    saved_profile: Option<&str>,
    target: Option<&str>,
    local: bool,
    offer_local: bool,
    fresh_sign_in: bool,
    target_exists: Option<bool>,
) -> (String, Vec<String>) {
    let named = saved_profile.is_some() || target.is_some();
    let mut message = if named {
        if local {
            format!("Current login for this repository: {current}")
        } else {
            format!("Current global default: {current}")
        }
    } else if local {
        format!("Currently signed in as {current} for this repository.")
    } else {
        format!("Currently signed in globally as {current}.")
    };
    if let Some(profile) = saved_profile.filter(|profile| target != Some(*profile)) {
        let _ = write!(message, "\n{profile} will remain saved.");
    }
    if fresh_sign_in {
        match (target, target_exists) {
            (Some(name), Some(true)) => {
                let _ = write!(
                    message,
                    "\nSigning in again may replace {name}'s saved connection."
                );
            }
            (Some(name), None) => {
                let _ = write!(
                    message,
                    "\nIf {name} already exists, signing in may replace its saved connection."
                );
            }
            _ => {}
        }
    }
    let same_profile = target == Some(current) && fresh_sign_in && target_exists == Some(true);
    let proceed = if same_profile {
        format!("Sign in again as {current}")
    } else if let Some(target) = target {
        if local {
            format!("Sign in as {target} for this repository")
        } else {
            format!("Sign in as {target} and make it the global default")
        }
    } else {
        "Sign in with a different login".to_string()
    };
    let cancel = if same_profile {
        "Keep the current connection (cancel)".to_string()
    } else if named {
        if local {
            format!("Keep {current} for this repository (cancel)")
        } else {
            format!("Keep {current} as the global default (cancel)")
        }
    } else {
        format!("Keep {current} signed in (cancel)")
    };
    let mut choices = vec![proceed, cancel];
    if offer_local {
        choices.push(match target {
            Some(target) => format!("Sign in as {target} for this repository only"),
            None => "Sign in with a different login for this repository only".to_string(),
        });
    }
    (message, choices)
}

#[cfg(test)]
mod tests {
    use ngit::login::SignerInfo;

    use super::{implicit_alias_selection, login_scope_choice, login_scope_prompt};

    #[test]
    fn no_activate_requires_a_profile_and_excludes_local_selection() {
        use clap::Parser;
        let args = ["ngit", "account", "login", "--no-activate"];
        assert!(crate::cli::Cli::try_parse_from(args).is_err());
        let mut args = args.to_vec();
        args.extend(["--profile", "ci"]);
        assert!(crate::cli::Cli::try_parse_from(&args).is_ok());
        args.push("--local");
        assert!(crate::cli::Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn nostrconnect_accepts_profiles_and_json_without_interaction() {
        use clap::Parser;
        assert!(
            crate::cli::Cli::try_parse_from([
                "ngit",
                "account",
                "login",
                "--nostrconnect",
                "--profile",
                "agent",
                "--json"
            ])
            .is_ok()
        );
        for other in [
            "--interactive",
            "--bunker-url",
            "--nsec",
            "--nsec-file",
            "--nbunksec",
            "--nbunksec-file",
            "--signer",
            "--bunker-uri",
            "--bunker-app-key",
        ] {
            let mut args = vec!["ngit", "account", "login", "--nostrconnect", other];
            if other != "--interactive" {
                args.push("value");
            }
            assert!(crate::cli::Cli::try_parse_from(args).is_err(), "{other}");
        }
        assert!(
            crate::cli::Cli::try_parse_from([
                "ngit",
                "account",
                "login",
                "stored-account",
                "--nostrconnect"
            ])
            .is_err()
        );
    }

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
    fn simple_login_avoids_profile_and_session_explanations() {
        for local in [false, true] {
            let (message, choices) =
                login_scope_prompt("Alice", None, None, local, false, true, None);
            assert_eq!(message.lines().count(), 1);
            assert!(!message.contains("profile"));
            assert!(!message.contains("session"));
            assert_eq!(
                choices,
                [
                    "Sign in with a different login",
                    "Keep Alice signed in (cancel)"
                ]
            );
            assert_eq!(login_scope_choice(0, local), (true, local));
            assert_eq!(login_scope_choice(1, local), (false, local));
        }
    }

    #[test]
    fn named_switch_preserves_saved_profile_and_scope_choices() {
        let (message, choices) = login_scope_prompt(
            "dcdroid",
            Some("dcdroid"),
            Some("dcdev"),
            false,
            true,
            true,
            Some(false),
        );
        assert_eq!(
            message,
            "Current global default: dcdroid\ndcdroid will remain saved."
        );
        assert_eq!(
            choices[0],
            "Sign in as dcdev and make it the global default"
        );
        assert_eq!(choices[1], "Keep dcdroid as the global default (cancel)");
        assert_eq!(choices[2], "Sign in as dcdev for this repository only");
        assert_eq!(login_scope_choice(2, false), (true, true));
        let (message, choices) = login_scope_prompt(
            "dcdroid",
            Some("dcdroid"),
            Some("dcdev"),
            true,
            false,
            true,
            Some(false),
        );
        assert!(message.contains("for this repository"));
        assert_eq!(choices[0], "Sign in as dcdev for this repository");
        assert_eq!(choices.len(), 2);
    }

    #[test]
    fn existing_profile_pairing_warns_without_promising_retention() {
        let (message, choices) = login_scope_prompt(
            "dcdev",
            Some("dcdev"),
            Some("dcdev"),
            false,
            false,
            true,
            Some(true),
        );
        assert!(message.contains("Signing in again may replace dcdev's saved connection"));
        assert!(!message.contains("remain saved"));
        assert_eq!(
            choices,
            [
                "Sign in again as dcdev",
                "Keep the current connection (cancel)"
            ]
        );
        let (message, _) =
            login_scope_prompt("Alice", None, Some("dcdev"), false, false, true, None);
        assert!(message.contains("If dcdev already exists"));
        assert!(!message.contains("remain saved"));
        let (message, _) = login_scope_prompt(
            "dcdroid",
            Some("dcdroid"),
            Some("dcdev"),
            false,
            false,
            false,
            Some(true),
        );
        assert!(!message.contains("replace"));
    }
}
