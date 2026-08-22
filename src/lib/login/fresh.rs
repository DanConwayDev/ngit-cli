use std::{
    io::Write,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use console::Style;
use dialoguer::theme::{ColorfulTheme, Theme};
use nostr::prelude::{
    Keys, Metadata, PublicKey, RelayList, RelayUrl, ToBech32, event::FinalizeEvent,
    key::AsyncGetPublicKey, nip46::NostrConnectUri,
};
use nostr_connect::client::NostrConnect;
use qrcode::QrCode;
use tokio::{signal, sync::Mutex};

use super::{
    SignerInfo, SignerInfoSource, credential_store,
    existing::load_existing_login,
    key_encryption::decrypt_key,
    print_logged_in_as,
    user::{UserRef, get_user_details},
};
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, Printer, PromptChoiceParms, PromptConfirmParms,
        PromptInputParms, PromptPasswordParms, multi_select_with_custom_value,
        show_multi_input_prompt_success,
    },
    client::{Connect, save_event_in_global_cache, send_events},
    git::{Repo, RepoActions, remove_git_config_item, save_git_config_item},
};

pub async fn fresh_login_or_signup(
    git_repo: &Option<&Repo>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    signer_info: Option<SignerInfo>,
    save_local: bool,
    signer_relays: &[String],
    alias: Option<&str>,
    selected_by: Option<&str>,
) -> Result<(Arc<crate::NgitSigner>, UserRef, SignerInfoSource)> {
    let (signer, public_key, mut signer_info, _) = loop {
        if let Some(signer_info) = signer_info {
            let (signer, user_ref, source) = load_existing_login(
                git_repo,
                &Some(signer_info.clone()),
                &None,
                &Some(SignerInfoSource::CommandLineArguments),
                client,
                true,
                true,
                false,
            )
            .await?;
            break (signer, user_ref.public_key, signer_info, source);
        }
        match Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt("login to nostr")
                .with_default(0)
                .with_choices(vec![
                    "secret key (nsec / ncryptsec)".to_string(),
                    "nostr connect (remote signer)".to_string(),
                    "create account".to_string(),
                    "help".to_string(),
                ])
                .dont_report(),
        )? {
            0 => match get_fresh_nsec_signer().await {
                Ok(Some(res)) => break res,
                Ok(None) => continue,
                Err(e) => return Err(e.context("error getting fresh signer from nsec")),
            },
            1 => match get_fresh_nip46_signer(client, signer_relays).await {
                Ok(Some(res)) => break res,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("error getting fresh nip46 signer: {e}");
                    continue;
                }
            },
            2 => match signup(client).await {
                Ok(Some(res)) => break res,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("error getting fresh signer from signup: {e}");
                    continue;
                }
            },
            _ => {
                display_login_help_content().await;
                continue;
            }
        }
    };
    let npub = public_key.to_bech32()?;
    if let SignerInfo::Bunker {
        npub: signer_npub, ..
    } = &mut signer_info
    {
        if signer_npub.is_none() {
            *signer_npub = Some(npub.clone());
        }
    }
    if let Some(alias) = alias {
        crate::login::credential_store::ensure_alias_available(alias, &npub)?;
    }
    let mut saved_source = git_config_source(!save_local);
    let mut receipts = CredentialStorageReceipts::default();
    if let SignerInfo::Selection { selector } = &signer_info {
        // account login resolves selections before calling this function, but
        // library callers may still supply one. Persist the separately
        // resolved npub so an alias can never be copied into `nostr.npub`.
        if let Some(alias) = alias {
            save_signer_alias_into(git_repo, alias, &npub, !save_local, &mut receipts)?;
        } else {
            save_signer_selection(git_repo, selector, &npub, !save_local)?;
        }
    } else {
        saved_source =
            save_to_git_config(git_repo, &signer_info, !save_local, &mut receipts).await?;
        if saved_source != SignerInfoSource::CommandLineArguments {
            let saved_globally = saved_source == SignerInfoSource::GitGlobal;
            if let Some(alias) = alias {
                save_signer_alias_into(git_repo, alias, &npub, saved_globally, &mut receipts)?;
            } else if let Some(selector) = selected_by {
                save_signer_selection(git_repo, selector, &npub, saved_globally)?;
            }
        }
    }
    // dropping here prints the consolidated storage report ahead of the
    // login output
    drop(receipts);
    let user_ref = get_user_details(
        &public_key,
        client,
        if let Some(git_repo) = git_repo {
            Some(git_repo.get_path()?)
        } else {
            None
        },
        false,
        false,
    )
    .await?;
    print_logged_in_as(&user_ref, client.is_none(), &saved_source, alias)?;
    Ok((signer, user_ref, saved_source))
}

/// Non-interactive login using a `bunker://` URL provided directly.
///
/// Parses the URL, generates a fresh app key, connects to the remote signer,
/// stores the resulting connection and configures the selected login scope.
pub async fn login_with_bunker_url(
    git_repo: &Option<&Repo>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    bunker_url: &str,
    save_local: bool,
    signer_relays: &[String],
    alias: Option<&str>,
) -> Result<(Arc<crate::NgitSigner>, UserRef, SignerInfoSource)> {
    let url = NostrConnectUri::parse(bunker_url)
        .context("invalid bunker:// URL - must be a valid bunker:// URI")?;

    let (app_key, _) = generate_nostr_connect_app(client, signer_relays)?;

    let printer = Arc::new(Mutex::new(Printer::default()));
    {
        let mut p = printer.lock().await;
        p.println("connecting to remote signer...".to_string());
    }

    let (signer, user_public_key, bunker_uri) =
        listen_for_remote_signer(&app_key, &url, printer).await?;

    let signer_info = SignerInfo::Bunker {
        bunker_uri: bunker_uri.to_string(),
        bunker_app_key: app_key.secret_key().to_secret_hex(),
        npub: Some(user_public_key.to_bech32()?),
    };

    let npub = user_public_key.to_bech32()?;
    if let Some(alias) = alias {
        crate::login::credential_store::ensure_alias_available(alias, &npub)?;
    }
    let mut receipts = CredentialStorageReceipts::default();
    let source = save_to_git_config(git_repo, &signer_info, !save_local, &mut receipts).await?;
    if source != SignerInfoSource::CommandLineArguments {
        if let Some(alias) = alias {
            save_signer_alias_into(
                git_repo,
                alias,
                &npub,
                source == SignerInfoSource::GitGlobal,
                &mut receipts,
            )?;
        }
    }
    // dropping here prints the consolidated storage report ahead of the
    // login output
    drop(receipts);

    let user_ref = get_user_details(
        &user_public_key,
        client,
        if let Some(git_repo) = git_repo {
            Some(git_repo.get_path()?)
        } else {
            None
        },
        false,
        false,
    )
    .await?;

    print_logged_in_as(&user_ref, client.is_none(), &source, alias)?;
    Ok((signer, user_ref, source))
}

pub async fn get_fresh_nsec_signer() -> Result<
    Option<(
        Arc<crate::NgitSigner>,
        PublicKey,
        SignerInfo,
        SignerInfoSource,
    )>,
> {
    loop {
        let input = Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("nsec")
                    .with_flag_name("--nsec")
                    .dont_report(),
            )
            .context("failed to get nsec input from interactor")?;
        let (keys, signer_info) = if input.contains("ncryptsec") {
            let password = Interactor::default()
                .password(
                    PromptPasswordParms::default()
                        .with_prompt("password")
                        .dont_report(),
                )
                .context("failed to get password input from interactor.password")?;
            let keys = if let Ok(keys) = decrypt_key(&input, password.clone().as_str())
                .context("failed to decrypt ncryptsec with provided password")
            {
                keys
            } else {
                show_prompt_error(
                    "invalid ncryptsec and password combination",
                    &shorten_string(&input),
                );
                match Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_default(0)
                        .with_prompt("login to nostr")
                        .with_choices(vec!["try again with nsec".to_string(), "back".to_string()])
                        .dont_report(),
                )? {
                    0 => continue,
                    _ => break Ok(None),
                }
            };
            let npub = Some(keys.public_key().to_bech32()?);
            let signer_info = if Interactor::default()
                .confirm(PromptConfirmParms::default().with_prompt("remember details?"))?
                || !Interactor::default().confirm(PromptConfirmParms::default().with_prompt(
                    "you will be prompted for password to decrypt your ncryptsec at every git push. are you sure?",
                ))? {
                SignerInfo::Nsec {
                    nsec: keys.secret_key().to_bech32()?,
                    password: None,
                    npub,
                    verify_npub: false,
                }
            } else {
                show_prompt_success("nsec", &shorten_string(&input));
                SignerInfo::Nsec {
                    nsec: input,
                    password: Some(password),
                    npub,
                    verify_npub: false,
                }
            };
            (keys, signer_info)
        } else if let Ok(keys) = nostr::prelude::Keys::from_str(&input) {
            let nsec = keys.secret_key().to_bech32()?;
            show_prompt_success("nsec", &shorten_string(&input));
            let signer_info = SignerInfo::Nsec {
                nsec,
                password: None,
                npub: Some(keys.public_key().to_bech32()?),
                verify_npub: false,
            };
            (keys, signer_info)
        } else {
            show_prompt_error("invalid nsec", &shorten_string(&input));
            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_prompt("login to nostr")
                    .with_choices(vec!["try again with nsec".to_string(), "back".to_string()])
                    .dont_report(),
            )? {
                0 => continue,
                _ => break Ok(None),
            }
        };

        let public_key = keys.public_key();

        break Ok(Some((
            Arc::new(crate::NgitSigner::Keys(keys)),
            public_key,
            signer_info,
            // TODO factor in source
            SignerInfoSource::GitGlobal,
        )));
    }
}

pub fn show_prompt_success(label: &str, value: &str) {
    eprintln!("{}", {
        let mut s = String::new();
        let _ = ColorfulTheme::default().format_input_prompt_selection(&mut s, label, value);
        s
    });
}

fn show_prompt_error(label: &str, value: &str) {
    eprintln!("{}", {
        let mut s = String::new();
        let _ = ColorfulTheme::default().format_error(
            &mut s,
            &format!(
                "{label}: \"{}\"",
                if value.is_empty() {
                    "empty".to_string()
                } else {
                    shorten_string(value)
                }
            ),
        );
        s
    });
}

fn shorten_string(s: &str) -> String {
    if s.len() < 15 {
        s.to_string()
    } else {
        format!("{}...", &s[..15])
    }
}

pub async fn get_fresh_nip46_signer(
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    signer_relays: &[String],
) -> Result<
    Option<(
        Arc<crate::NgitSigner>,
        PublicKey,
        SignerInfo,
        SignerInfoSource,
    )>,
> {
    let (app_key, nostr_connect_url) = generate_nostr_connect_app(client, signer_relays)?;
    let printer = Arc::new(Mutex::new(Printer::default()));
    // Unified bunker flow: show the QR code and connection string and listen
    // for the signer to connect in the background, while a menu concurrently
    // offers to paste a bunker:// url, change relays, or cancel.  Loop so the
    // user can change relays and see a refreshed QR/URL.
    let url = {
        let mut current_url = nostr_connect_url;
        loop {
            // Display QR code and connection string with the current relay
            // list, followed by the menu options.
            display_nostr_connect(&current_url)?;
            print_connect_menu_options();

            // Start listening for the signer immediately after displaying
            // the QR/URL — don't wait for the user to press anything.  The
            // `done` flag lets the (blocking) menu poll loop notice when the
            // listener has finished so it can return without leaving a
            // thread blocked on a terminal read.
            let nostr_connect = Arc::new(NostrConnect::new(
                current_url.clone(),
                app_key.clone(),
                Duration::from_secs(10 * 60),
                None,
            )?);
            let done = Arc::new(AtomicBool::new(false));
            let done_listener = Arc::clone(&done);
            // Bootstrap on the *same* NostrConnect instance we later call
            // `bunker_uri()` on, and hand the same `Arc` to the returned
            // `NgitSigner` (all via `Arc` clones that share the allocation).
            // NostrConnect caches the discovered remote-signer pubkey in an
            // internal `OnceCell`; cloning the client *before* bootstrap gives
            // the clone an independent, empty cache, so a later `bunker_uri()`
            // (or signing) on a different instance re-runs the connect
            // handshake and hangs waiting for a second connect message that
            // never arrives. See the pre-0.45 regression where login froze
            // after "signer connection established".
            let nc_listener = Arc::clone(&nostr_connect);
            let pubkey_handle = tokio::spawn(async move {
                let res = nc_listener
                    .get_public_key_async()
                    .await
                    .map_err(|e| anyhow::anyhow!(e));
                done_listener.store(true, Ordering::Relaxed);
                res
            });

            // Wait (on a blocking thread so it doesn't stall the listener)
            // for either the signer to connect or the user to pick an option.
            let done_menu = Arc::clone(&done);
            let choice =
                tokio::task::spawn_blocking(move || wait_for_signer_or_menu_choice(&done_menu))
                    .await
                    .context("connect menu task panicked")??;

            match choice {
                // Signer connected (or the listener finished) on its own.
                None => match pubkey_handle.await {
                    Ok(Ok(public_key)) => {
                        // The QR code / connection string can be taller than the
                        // terminal window, so its top scrolls into the scrollback
                        // buffer that ANSI line-clearing can't reach — attempting
                        // to clear it leaves a confusing half-erased QR.  Instead
                        // leave it in place (the user may still be mid-scan) and
                        // print a clear green marker so it's obvious we've moved
                        // on to a connected signer.
                        eprintln!(
                            "{}",
                            Style::new()
                                .for_stderr()
                                .bold()
                                .green()
                                .apply_to("✓ signer connection established")
                        );
                        let bunker_url = nostr_connect
                            .bunker_uri()
                            .await
                            .context("failed to get bunker URI from NostrConnect client")?;
                        let signer_info = SignerInfo::Bunker {
                            bunker_uri: bunker_url.to_string(),
                            bunker_app_key: app_key.secret_key().to_secret_hex(),
                            npub: Some(public_key.to_bech32()?),
                        };
                        return Ok(Some((
                            Arc::new(crate::NgitSigner::Connect(Arc::clone(&nostr_connect))),
                            public_key,
                            signer_info,
                            SignerInfoSource::GitGlobal,
                        )));
                    }
                    _ => {
                        // Connection failed — redisplay and listen again.
                        eprintln!("failed to connect to signer, trying again...");
                        continue;
                    }
                },
                Some(ConnectMenuChoice::EnterBunkerUri) => {
                    pubkey_handle.abort();
                    break prompt_for_bunker_url()?;
                }
                Some(ConnectMenuChoice::ChangeRelays) => {
                    pubkey_handle.abort();
                    let selected = select_signer_relays(&current_url)?;
                    if !selected.is_empty() {
                        let new_relays: Vec<RelayUrl> =
                            selected.iter().flat_map(|s| RelayUrl::parse(s)).collect();
                        current_url =
                            NostrConnectUri::client(app_key.public_key(), new_relays, "ngit");
                    }
                }
                Some(ConnectMenuChoice::Cancel) => {
                    pubkey_handle.abort();
                    return Ok(None);
                }
            }
        }
    };

    {
        let printer_clone = Arc::clone(&printer);
        let mut printer_locked = printer_clone.lock().await;
        printer_locked.println(
            "add / approve in your signer or use ctrl + c to go back to login menu...".to_string(),
        );
    }

    let (signer, user_public_key, bunker_url) =
        listen_for_remote_signer(&app_key, &url, printer).await?;
    let signer_info = SignerInfo::Bunker {
        bunker_uri: bunker_url.to_string(),
        bunker_app_key: app_key.secret_key().to_secret_hex(),
        npub: Some(user_public_key.to_bech32()?),
    };
    Ok(Some((
        signer,
        user_public_key,
        signer_info,
        SignerInfoSource::GitGlobal,
    )))
}

pub fn generate_nostr_connect_app(
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    signer_relays: &[String],
) -> Result<(Keys, NostrConnectUri)> {
    let app_key = Keys::generate();
    let relays = if !signer_relays.is_empty() {
        signer_relays
            .iter()
            .map(|s| {
                if s.starts_with("ws://") || s.starts_with("wss://") {
                    s.clone()
                } else {
                    format!("wss://{s}")
                }
            })
            .flat_map(|s| RelayUrl::parse(&s))
            .collect::<Vec<RelayUrl>>()
    } else if let Some(client) = client {
        client
            .get_fallback_signer_relays()
            .iter()
            .flat_map(|s| RelayUrl::parse(s))
            .collect::<Vec<RelayUrl>>()
    } else {
        vec![]
    };
    let nostr_connect_url = NostrConnectUri::client(app_key.public_key(), relays.clone(), "ngit");
    Ok((app_key, nostr_connect_url))
}

/// Print the QR code and the nostrconnect connection string to stderr.
///
/// Output goes directly to stderr with bare `eprintln!` (rather than via the
/// [`Printer`]) because the QR is frequently taller than the terminal window:
/// its top scrolls into the scrollback buffer, which ANSI line-clearing can't
/// reach, so there's no point tracking the lines for a later clear — see the
/// connect branch in [`get_fresh_nip46_signer`].
///
/// Both the QR code (to scan) and the connection string (to copy) are shown so
/// the user can use whichever their signer app supports.
fn display_nostr_connect(url: &NostrConnectUri) -> Result<()> {
    let dim = Style::new().for_stderr().color256(247);
    eprintln!(
        "{}",
        Style::new().for_stderr().bold().apply_to("nostr connect")
    );
    eprintln!(
        "{}",
        dim.apply_to("scan QR code in signer app (eg. Amber):")
    );
    for line in generate_qr(&url.to_string())? {
        eprintln!("{line}");
    }
    eprintln!();
    eprintln!(
        "{}",
        dim.apply_to("or copy this connection string into your signer:")
    );
    eprintln!(
        "{}",
        Style::new()
            .for_stderr()
            .bold()
            .cyan()
            .apply_to(url.to_string())
    );
    eprintln!();
    let relays = url
        .relays()
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("{}", dim.apply_to(format!("signer relays: {relays}")));
    Ok(())
}

/// The manual options offered on the bunker connect screen while the app waits
/// in the background for the signer to connect.
enum ConnectMenuChoice {
    EnterBunkerUri,
    ChangeRelays,
    Cancel,
}

/// Print the bunker connect screen menu options to stderr.
///
/// Printed in canonical (cooked) terminal mode before
/// [`wait_for_signer_or_menu_choice`] switches to raw mode for key polling, so
/// the line breaks render correctly.
fn print_connect_menu_options() {
    let bold = Style::new().for_stderr().bold();
    let dim = Style::new().for_stderr().color256(247);
    eprintln!();
    eprintln!(
        "{} {}",
        bold.apply_to("waiting for signer to connect…"),
        dim.apply_to("(scan the QR code or copy the connection string above)")
    );
    eprintln!("{} manually enter bunker:// url", bold.apply_to("[1]"));
    eprintln!("{} change signer relays", bold.apply_to("[2]"));
    eprintln!("{} cancel", bold.apply_to("[3]"));
}

/// Block until either the signer connects (the `done` flag is set by the
/// background listener) or the user picks one of the connect-screen options.
///
/// Returns `Ok(None)` when the listener finished (the caller should check its
/// result); otherwise returns the chosen menu option.
///
/// Key input is polled with a timeout rather than read with a blocking call so
/// the loop can notice `done` and return promptly — this is what lets the menu
/// run concurrently with the background listener without leaving a thread stuck
/// on a terminal read (which would hold the terminal in raw mode).
fn wait_for_signer_or_menu_choice(done: &AtomicBool) -> Result<Option<ConnectMenuChoice>> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode},
    };

    enable_raw_mode().context("failed to enable raw mode for connect menu")?;
    let outcome = (|| -> Result<Option<ConnectMenuChoice>> {
        loop {
            if done.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if event::poll(Duration::from_millis(150)).context("failed to poll for key input")? {
                if let Event::Key(key) = event::read().context("failed to read key input")? {
                    // ignore key-release / repeat events (Windows emits both)
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // ctrl+c cancels, matching the rest of the CLI
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c'))
                    {
                        return Ok(Some(ConnectMenuChoice::Cancel));
                    }
                    // Only digit shortcuts (and esc) are accepted — a stray
                    // paste of a bunker:// / nostrconnect:// url at this screen
                    // then gets harmlessly ignored rather than triggering an
                    // option and corrupting a later input prompt.
                    match key.code {
                        KeyCode::Char('1') => {
                            return Ok(Some(ConnectMenuChoice::EnterBunkerUri));
                        }
                        KeyCode::Char('2') => {
                            return Ok(Some(ConnectMenuChoice::ChangeRelays));
                        }
                        KeyCode::Char('3') | KeyCode::Esc => {
                            return Ok(Some(ConnectMenuChoice::Cancel));
                        }
                        _ => {}
                    }
                }
            }
        }
    })();
    // Always restore the terminal, even if polling errored.
    let _ = disable_raw_mode();
    outcome
}

/// Prompt the user to paste a `bunker://` url, re-prompting until it parses.
fn prompt_for_bunker_url() -> Result<NostrConnectUri> {
    let mut error = None;
    loop {
        let input = Interactor::default()
            .input(
                PromptInputParms::default().with_prompt(if let Some(error) = error {
                    format!("error: {error}. try again with bunker url")
                } else {
                    "bunker url".to_string()
                }),
            )
            .context("failed to get bunker url input from interactor")?;
        match NostrConnectUri::parse(&input) {
            Ok(url) => return Ok(url),
            Err(e) => error = Some(e),
        }
    }
}

/// Present the multiselect UI for choosing signer relays.
///
/// Returns the selected relay list as strings.  An empty return means the user
/// submitted without selecting anything (caller should keep the existing URL).
fn select_signer_relays(nostr_connect_url: &NostrConnectUri) -> Result<Vec<String>> {
    let current_relays: Vec<String> = nostr_connect_url
        .relays()
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let defaults = vec![true; current_relays.len()];
    let selected = multi_select_with_custom_value(
        "signer relays",
        "signer relay",
        current_relays,
        defaults,
        |s| {
            let url = if s.starts_with("ws://") || s.starts_with("wss://") {
                s.to_string()
            } else {
                format!("wss://{s}")
            };
            RelayUrl::parse(&url)
                .map(|r| r.to_string())
                .context(format!("invalid relay URL: {s}"))
        },
    )?;
    show_multi_input_prompt_success("signer relays", &selected);
    Ok(selected)
}

pub async fn listen_for_remote_signer(
    app_key: &Keys,
    nostr_connect_url: &NostrConnectUri,
    printer: Arc<Mutex<Printer>>,
) -> Result<(Arc<crate::NgitSigner>, PublicKey, NostrConnectUri)> {
    let app_key = app_key.clone();
    let nostr_connect_url_clone = nostr_connect_url.clone();

    let nostr_connect = Arc::new(NostrConnect::new(
        nostr_connect_url_clone,
        app_key,
        Duration::from_secs(10 * 60),
        None,
    )?);
    // Bootstrap on this exact instance so the remote-signer pubkey discovered
    // during the connect handshake is cached in its `OnceCell`. We then call
    // `bunker_uri()` (which reads that cache) and hand the same `Arc` to the
    // returned signer, so signing reuses the bootstrapped instance rather than
    // re-running the handshake. Cloning the client before bootstrap would
    // leave a separate empty cache and hang on `bunker_uri()`.
    let pubkey_future = {
        let nc = Arc::clone(&nostr_connect);
        async move {
            nc.get_public_key_async()
                .await
                .map_err(|e| anyhow::anyhow!(e))
        }
    };

    // wait for signer response or ctrl + c
    let res = tokio::select! {
        pubkey_result = pubkey_future => {
            Some(pubkey_result)
        },
        _ = signal::ctrl_c() => {
            None
        }
    };

    let printer_clone = Arc::clone(&printer);
    let mut printer = printer_clone.lock().await;
    printer.clear_all();

    if let Some(Ok(public_key)) = res {
        // Get the proper bunker URI from the NostrConnect client
        // This will contain the correct remote-signer-pubkey that was discovered
        // during the connection handshake, regardless of whether the original URL
        // was bunker:// (already had it) or nostrconnect:// (extracted from response
        // event author)
        let bunker_url = nostr_connect
            .bunker_uri()
            .await
            .context("failed to get bunker URI from NostrConnect client")?;

        let signer = Arc::new(crate::NgitSigner::Connect(Arc::clone(&nostr_connect)));
        Ok((signer, public_key, bunker_url))
    } else {
        bail!("failed to get signer")
    }
}

pub fn generate_qr(data: &str) -> Result<Vec<String>> {
    let mut lines = vec![];
    let qr = QrCode::new(data.as_bytes()).context("failed to create QR")?;
    let colors = qr.to_colors();
    let mut rows: Vec<&[qrcode::Color]> = colors.chunks(qr.width()).collect();
    let light_row = vec![qrcode::Color::Light; qr.width()];
    rows.insert(0, &light_row);
    rows.push(&light_row);
    for (row, data) in rows.iter().enumerate() {
        let odd = row % 2 != 0;
        if odd {
            continue;
        }
        let mut line = " ".to_string();
        for (col, color) in data.iter().enumerate() {
            let top = color;
            let mut bottom = qrcode::Color::Light;
            if let Some(next_row_data) = rows.get(row + 1) {
                if let Some(color) = next_row_data.get(col) {
                    bottom = *color;
                }
            }
            line.push(if *top == qrcode::Color::Dark {
                if bottom == qrcode::Color::Dark {
                    '█'
                } else {
                    '▀'
                }
            } else if bottom == qrcode::Color::Dark {
                '▄'
            } else {
                ' '
            });
        }
        lines.push(line);
    }
    Ok(lines)
}

#[derive(Debug)]
struct CredentialStorageReport {
    subject: String,
    entry: String,
    backend: credential_store::Backend,
    os_fallback: bool,
}

/// Credential-store receipts pending report.
///
/// A receipt is registered the moment its store write succeeds and printed
/// when the guard drops, so the report survives early returns and `?`
/// propagation between the write and the end of the login flow. Callers drop
/// the guard explicitly once every write has completed, keeping the report's
/// position in the happy-path output.
#[derive(Default)]
struct CredentialStorageReceipts {
    secret: Option<CredentialStorageReport>,
    alias: Option<CredentialStorageReport>,
}

impl Drop for CredentialStorageReceipts {
    fn drop(&mut self) {
        print_credential_storage(self.secret.as_ref(), self.alias.as_ref());
    }
}

async fn save_to_git_config(
    git_repo: &Option<&Repo>,
    signer_info: &SignerInfo,
    global: bool,
    receipts: &mut CredentialStorageReceipts,
) -> Result<SignerInfoSource> {
    let signer_info = protect_secrets(git_repo, signer_info, receipts)?;
    let signer_info = &signer_info;
    let global = if std::env::var("NGITTEST").is_ok() {
        false
    } else {
        global
    };
    let err_msg = format!(
        "failed to configure the signer in {} Git config",
        if global { "global" } else { "local" }
    );
    if let Err(error) =
        silently_save_to_git_config(git_repo, signer_info, global).context(err_msg.clone())
    {
        // Check if this is a read-only file system error
        let is_readonly_error = error
            .chain()
            .any(|e| e.to_string().contains("Read-only file system"));

        if is_readonly_error && global {
            // In non-interactive mode, provide a clear error with --local suggestion
            if crate::cli_interactor::Interactor::is_non_interactive() {
                use crate::cli_interactor::cli_error;
                return Err(cli_error(
                    "failed to configure the signer",
                    &[("cause", "global git config is read-only")],
                    &[
                        "ngit account create --local --nsec <your-nsec>",
                        "ngit account login --local --nsec <your-nsec>",
                    ],
                ));
            }
        }

        eprintln!("Error: {error:?}");
        match signer_info {
            SignerInfo::Nsec {
                nsec,
                password: _,
                npub: _,
                ..
            } => {
                eprintln!("consider manually setting git config nostr.nsec to: {nsec}");
            }
            SignerInfo::Bunker {
                bunker_uri,
                bunker_app_key,
                npub: _,
                ..
            } => {
                eprintln!("consider manually setting git config as follows:");
                eprintln!("nostr.bunker-uri: {bunker_uri}");
                eprintln!("nostr.bunker-app-key: {bunker_app_key}");
            }
            SignerInfo::Selection { selector } => {
                eprintln!("consider manually setting git config nostr.signer to: {selector}");
            }
        }
        if global {
            loop {
                match Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_default(0)
                        .with_prompt(&err_msg)
                        .with_choices(vec![
                            "i'll update global git config manually with above values".to_string(),
                            "only log into local git repository (save to local git config)"
                                .to_string(),
                            "one time login".to_string(),
                        ]),
                )? {
                    0 => {
                        // check
                        if let Ok((_, user_ref, _)) = load_existing_login(
                            git_repo,
                            &None,
                            &None,
                            &Some(SignerInfoSource::GitGlobal),
                            None,
                            true,
                            true,
                            false,
                        )
                        .await
                        {
                            if user_ref.public_key == get_pubkey_from_signer_info(signer_info)? {
                                return Ok(SignerInfoSource::GitGlobal);
                            } else {
                                eprintln!(
                                    "global git config hasn't been updated with different npub"
                                );
                            }
                        } else {
                            eprintln!(
                                "global git config hasn't been updated with nostr login values"
                            );
                        }
                    }
                    1 => {
                        silently_save_to_git_config(git_repo, signer_info, false)
                            .context("failed to configure the signer in local Git config")?;
                        return Ok(SignerInfoSource::GitLocal);
                    }
                    _ => {
                        return Ok(SignerInfoSource::CommandLineArguments);
                    }
                }
            }
        }
        Err(error)
    } else {
        Ok(git_config_source(global))
    }
}

fn git_config_source(global: bool) -> SignerInfoSource {
    if global && std::env::var("NGITTEST").is_err() {
        SignerInfoSource::GitGlobal
    } else {
        SignerInfoSource::GitLocal
    }
}

pub fn configured_signer_scope_message(global: bool, signer_info: &SignerInfo) -> String {
    let scope = if global {
        "global Git config"
    } else {
        "this repository's local Git config"
    };
    match signer_info {
        SignerInfo::Selection { selector } if selector.starts_with("npub1") => {
            format!("{scope} now selects signer {selector}")
        }
        SignerInfo::Selection { selector } => {
            format!("{scope} now selects signer alias '{selector}'")
        }
        SignerInfo::Nsec { nsec, .. } if credential_store::parse_pointer(nsec).is_some() => {
            format!("{scope} now points to the stored account secret")
        }
        SignerInfo::Nsec { nsec, .. } if nsec.starts_with("ncryptsec1") => {
            format!("{scope} now contains the encrypted account secret")
        }
        SignerInfo::Bunker { bunker_app_key, .. }
            if credential_store::parse_pointer(bunker_app_key).is_some() =>
        {
            format!("{scope} now points to the stored remote signer connection")
        }
        SignerInfo::Nsec { .. } => {
            format!("{scope} now contains the account secret in plaintext")
        }
        SignerInfo::Bunker { .. } => {
            format!("{scope} now contains the remote signer connection in plaintext")
        }
    }
}

/// Persist an alias in both the selected git-config scope and, when enabled,
/// the credential store. Git config remains a portable account selector for
/// repository directories mounted into environments with a different store.
fn save_signer_alias_into(
    git_repo: &Option<&Repo>,
    alias: &str,
    npub: &str,
    global: bool,
    receipts: &mut CredentialStorageReceipts,
) -> Result<()> {
    use crate::login::credential_store::{self, SecretStorage};

    let alias = credential_store::normalize_alias(alias)?;
    let npub = PublicKey::parse(npub)
        .context("cannot save signer alias for an invalid npub")?
        .to_bech32()?;
    let policy = credential_store::policy(git_repo);
    let credential_backed = policy != SecretStorage::GitConfig;
    receipts.alias = credential_backed
        .then(|| credential_store::store_alias(&alias, &npub, policy))
        .transpose()
        .context("failed to save signer alias in the credential store")?
        .map(|(entry, backend)| CredentialStorageReport {
            subject: format!("signer alias '{alias}'"),
            entry,
            backend,
            os_fallback: policy == SecretStorage::Auto
                && backend == credential_store::Backend::File,
        });

    let global = global && std::env::var("NGITTEST").is_err();
    let scope = if global {
        &None
    } else if git_repo.is_some() {
        git_repo
    } else {
        bail!("cannot save a local signer alias without a git repository");
    };
    save_git_config_item(scope, &format!("nostr.signer-alias.{alias}"), &npub)?;
    save_git_config_item(scope, "nostr.signer", &alias)?;
    save_git_config_item(scope, "nostr.npub", &npub)?;
    if credential_backed {
        // The alias is now the complete selector for credential-store-backed
        // signers. Keeping a redundant credential pointer here obscures that
        // model and makes mounted repositories appear to carry login material
        // that is actually held by the machine's credential store.
        remove_git_config_item(scope, "nostr.nsec")?;
        remove_git_config_item(scope, "nostr.bunker-uri")?;
        remove_git_config_item(scope, "nostr.bunker-app-key")?;
    }
    Ok(())
}

fn print_credential_storage(
    secret: Option<&CredentialStorageReport>,
    alias: Option<&CredentialStorageReport>,
) {
    let file_store = credential_store::file_store_path().map_or_else(
        |_| "ngit's file store".to_string(),
        |path| path.display().to_string(),
    );
    // this runs from the receipts guard's Drop, possibly mid-unwind, where
    // an eprintln! panic on a closed stderr would abort the process
    let mut stderr = std::io::stderr().lock();
    for message in credential_storage_messages(secret, alias, &file_store) {
        let _ = writeln!(stderr, "{message}");
    }
}

fn credential_storage_messages(
    secret: Option<&CredentialStorageReport>,
    alias: Option<&CredentialStorageReport>,
    file_store: &str,
) -> Vec<String> {
    match (secret, alias) {
        (Some(secret), Some(alias)) if secret.backend == alias.backend => {
            let message = match secret.backend {
                credential_store::Backend::Os => format!(
                    "{} and associated {} are stored in the OS credential store as entries '{}' and '{}' under service '{}'",
                    alias.subject,
                    secret.subject,
                    alias.entry,
                    secret.entry,
                    credential_store::SERVICE
                ),
                credential_store::Backend::File => format!(
                    "{} and associated {} are stored in {file_store}{}",
                    alias.subject,
                    secret.subject,
                    os_fallback_suffix(secret.os_fallback || alias.os_fallback)
                ),
            };
            vec![message]
        }
        (secret, alias) => {
            let mut messages = Vec::with_capacity(2);
            if let Some(secret) = secret {
                messages.push(match secret.backend {
                    credential_store::Backend::Os => format!(
                        "{} is stored in the OS credential store as entry '{}' under service '{}'",
                        secret.subject,
                        secret.entry,
                        credential_store::SERVICE
                    ),
                    credential_store::Backend::File => format!(
                        "{} is stored in {file_store}{}",
                        secret.subject,
                        os_fallback_suffix(secret.os_fallback)
                    ),
                });
            }
            if let Some(alias) = alias {
                messages.push(match alias.backend {
                    credential_store::Backend::Os => format!(
                        "{} is stored in the OS credential store as entry '{}' under service '{}'",
                        alias.subject,
                        alias.entry,
                        credential_store::SERVICE
                    ),
                    credential_store::Backend::File => format!(
                        "{} is stored in {file_store}{}",
                        alias.subject,
                        os_fallback_suffix(alias.os_fallback)
                    ),
                });
            }
            messages
        }
    }
}

fn os_fallback_suffix(os_fallback: bool) -> &'static str {
    if os_fallback {
        " (OS credential store unavailable)"
    } else {
        ""
    }
}

fn save_signer_selection(
    git_repo: &Option<&Repo>,
    selector: &str,
    npub: &str,
    global: bool,
) -> Result<()> {
    use crate::login::credential_store::{self, SecretStorage};

    let credential_backed = credential_store::policy(git_repo) != SecretStorage::GitConfig;
    let global = global && std::env::var("NGITTEST").is_err();
    let scope = if global {
        &None
    } else if git_repo.is_some() {
        git_repo
    } else {
        bail!("cannot save a local signer selection without a git repository");
    };
    save_git_config_item(scope, "nostr.signer", selector)?;
    save_git_config_item(scope, "nostr.npub", npub)?;
    if credential_backed {
        remove_git_config_item(scope, "nostr.nsec")?;
        remove_git_config_item(scope, "nostr.bunker-uri")?;
        remove_git_config_item(scope, "nostr.bunker-app-key")?;
    }
    Ok(())
}

fn get_pubkey_from_signer_info(signer_info: &SignerInfo) -> Result<PublicKey> {
    let npub = match signer_info {
        SignerInfo::Bunker {
            bunker_uri: _,
            bunker_app_key: _,
            npub,
            ..
        } => npub,
        SignerInfo::Nsec {
            nsec: _,
            password: _,
            npub,
            ..
        } => npub,
        SignerInfo::Selection { selector } => {
            return PublicKey::parse(selector)
                .context("format of npub string in signer selection is invalid");
        }
    };
    if let Some(npub) = npub {
        PublicKey::parse(npub).context("format of npub string in signer_info is invalid")
    } else {
        bail!("no npub in signer_info object");
    }
}

/// Swap the plaintext secret in `signer_info` for a credential-store pointer
/// according to the secret-storage policy.
///
/// Fails closed: when neither the OS credential store nor ngit's file store
/// can hold the secret, interactive users are offered plaintext git config
/// explicitly and non-interactive callers get an error naming
/// `--secret-storage git-config`, instead of a silent plaintext fallback.
fn protect_secrets(
    git_repo: &Option<&Repo>,
    signer_info: &SignerInfo,
    receipts: &mut CredentialStorageReceipts,
) -> Result<SignerInfo> {
    use crate::login::credential_store::{self, SecretStorage};
    if matches!(signer_info, SignerInfo::Nsec { nsec, .. } if nsec.starts_with("ncryptsec1")) {
        return Ok(signer_info.clone());
    }
    if matches!(signer_info, SignerInfo::Nsec { nsec, .. } if credential_store::parse_pointer(nsec).is_some())
        || matches!(signer_info, SignerInfo::Bunker { bunker_app_key, .. } if credential_store::parse_pointer(bunker_app_key).is_some())
        || matches!(signer_info, SignerInfo::Selection { .. })
    {
        return Ok(signer_info.clone());
    }
    let policy = credential_store::policy(git_repo);
    if policy == SecretStorage::GitConfig {
        eprintln!(
            "{} will be stored in Git config in plaintext (secret-storage policy: git-config)",
            signer_storage_subject(signer_info)
        );
        return Ok(signer_info.clone());
    }
    let stored = match signer_info {
        SignerInfo::Nsec {
            nsec,
            password,
            npub: _,
            ..
        } => {
            let Ok(keys) = nostr::prelude::Keys::parse(nsec) else {
                return Ok(signer_info.clone());
            };
            credential_store::store(&keys, policy).map(|(pointer, backend)| {
                (
                    SignerInfo::Nsec {
                        nsec: pointer.clone(),
                        password: password.clone(),
                        npub: Some(
                            keys.public_key()
                                .to_bech32()
                                .expect("public keys always encode as npub"),
                        ),
                        verify_npub: false,
                    },
                    pointer,
                    backend,
                )
            })
        }
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            npub,
            ..
        } => {
            let Ok(keys) = nostr::prelude::Keys::parse(bunker_app_key) else {
                return Ok(signer_info.clone());
            };
            let npub = npub
                .as_deref()
                .context("cannot store bunker signer without its user npub")?;
            credential_store::store_bunker_signer(npub, bunker_uri, &keys, policy).map(
                |(pointer, backend)| {
                    (
                        SignerInfo::Selection {
                            selector: npub.to_string(),
                        },
                        pointer,
                        backend,
                    )
                },
            )
        }
        SignerInfo::Selection { .. } => unreachable!("selection returned before secret storage"),
    };
    match stored {
        Ok((protected, entry, backend)) => {
            receipts.secret = Some(CredentialStorageReport {
                subject: signer_storage_subject(signer_info).to_string(),
                entry,
                backend,
                os_fallback: policy == SecretStorage::Auto
                    && backend == credential_store::Backend::File,
            });
            Ok(protected)
        }
        Err(error) => {
            if !Interactor::is_non_interactive()
                && Interactor::default().confirm(PromptConfirmParms::default().with_prompt(
                    "failed to store the secret in a credential store. save it as plaintext in git config instead?",
                ))?
            {
                return Ok(signer_info.clone());
            }
            Err(error.context(
                "failed to store the account secret in a credential store; re-run with `--secret-storage git-config` to save it as plaintext in git config instead",
            ))
        }
    }
}

fn signer_storage_subject(signer_info: &SignerInfo) -> &'static str {
    match signer_info {
        SignerInfo::Nsec { .. } => "account secret",
        SignerInfo::Bunker { .. } => "remote signer connection",
        SignerInfo::Selection { .. } => "signer selection",
    }
}

fn silently_save_to_git_config(
    git_repo: &Option<&Repo>,
    signer_info: &SignerInfo,
    global: bool,
) -> Result<()> {
    if global {
        // remove local login otherwise it will override global next time ngit is called
        if let Some(git_repo) = git_repo {
            git_repo.remove_git_config_item("nostr.npub", false)?;
            git_repo.remove_git_config_item("nostr.nsec", false)?;
            git_repo.remove_git_config_item("nostr.bunker-uri", false)?;
            git_repo.remove_git_config_item("nostr.bunker-app-key", false)?;
            git_repo.remove_git_config_item("nostr.signer", false)?;
        }
    }

    let git_repo = if global {
        &None
    } else if git_repo.is_none() {
        bail!("failed to update local git config wihout git_repo object")
    } else {
        git_repo
    };

    let npub_to_save;
    match signer_info {
        SignerInfo::Nsec {
            nsec,
            password: _,
            npub,
            ..
        } => {
            npub_to_save = npub.as_deref();
            save_git_config_item(git_repo, "nostr.nsec", nsec)?;
            remove_git_config_item(git_repo, "nostr.bunker-uri")?;
            remove_git_config_item(git_repo, "nostr.bunker-app-key")?;
            remove_git_config_item(git_repo, "nostr.signer")?;
        }
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            npub,
            ..
        } => {
            npub_to_save = npub.as_deref();
            save_git_config_item(git_repo, "nostr.bunker-uri", bunker_uri)?;
            save_git_config_item(git_repo, "nostr.bunker-app-key", bunker_app_key)?;
            remove_git_config_item(git_repo, "nostr.nsec")?;
            remove_git_config_item(git_repo, "nostr.signer")?;
        }
        SignerInfo::Selection { selector } => {
            // Selection persistence needs both the selector and its resolved
            // npub, so normal callers must use `save_signer_selection`.
            // Failing here protects future callers from writing an alias into
            // `nostr.npub` as though it were a public key.
            bail!(
                "internal error: unresolved signer selection '{selector}' reached direct Git-config persistence"
            );
        }
    }
    if let Some(npub) = npub_to_save {
        save_git_config_item(git_repo, "nostr.npub", npub)?;
    } else {
        remove_git_config_item(git_repo, "nostr.npub")?;
    }
    Ok(())
}

/// Non-interactive signup function for creating a new account
///
/// # Arguments
/// * `name` - Display name for the new account
/// * `client` - Optional client for publishing metadata to relays
/// * `save_local` - If true, configure this local repository instead of the
///   global Git-config scope
/// * `publish` - If true, publish metadata and relay list to relays
///
/// # Returns
/// Returns a tuple of (signer, public_key, signer_info, keys) where keys can be
/// used to display the nsec
pub async fn signup_non_interactive(
    name: String,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    save_local: bool,
    publish: bool,
    relay_urls: Vec<String>,
) -> Result<(Arc<crate::NgitSigner>, PublicKey, SignerInfo, Keys)> {
    // Generate new keypair
    let keys = nostr::prelude::Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let public_key = keys.public_key();

    let signer_info = SignerInfo::Nsec {
        nsec,
        password: None,
        npub: Some(public_key.to_bech32()?),
        verify_npub: false,
    };

    // Store the secret and configure the selected Git-config scope.
    let git_repo = Repo::discover().ok();
    let mut receipts = CredentialStorageReceipts::default();
    let config_signer_info = protect_secrets(&git_repo.as_ref(), &signer_info, &mut receipts)?;
    if let Err(error) =
        silently_save_to_git_config(&git_repo.as_ref(), &config_signer_info, !save_local)
    {
        let is_readonly = error
            .chain()
            .any(|e| e.to_string().contains("Read-only file system"));

        if is_readonly && !save_local {
            use crate::cli_interactor::cli_error;

            let mut cmds: Vec<String> = match &config_signer_info {
                SignerInfo::Nsec { nsec, npub, .. } => {
                    let mut v = vec![format!("git config --global nostr.nsec {nsec}")];
                    if let Some(npub) = npub {
                        v.push(format!("git config --global nostr.npub {npub}"));
                    }
                    v
                }
                SignerInfo::Bunker {
                    bunker_uri,
                    bunker_app_key,
                    npub,
                    ..
                } => {
                    let mut v = vec![
                        format!("git config --global nostr.bunker-uri {bunker_uri}"),
                        format!("git config --global nostr.bunker-app-key {bunker_app_key}"),
                    ];
                    if let Some(npub) = npub {
                        v.push(format!("git config --global nostr.npub {npub}"));
                    }
                    v
                }
                SignerInfo::Selection { selector } => {
                    vec![format!("git config --global nostr.signer {selector}")]
                }
            };
            cmds.push("ngit account create --local --name <your-name>".to_string());

            let cmd_refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
            return Err(cli_error(
                "global Git config is read-only; configure the account locally or update Git config manually",
                &[("--local", "use the account only in this repository")],
                &cmd_refs,
            ));
        }

        return Err(error);
    }
    // dropping here prints the storage report ahead of the remaining output
    drop(receipts);

    let git_repo_path = if let Some(ref git_repo) = git_repo {
        Some(git_repo.get_path()?)
    } else {
        None
    };

    // Build events, save to cache, and optionally publish to relays
    if let Some(client) = client {
        let profile = Metadata::new().name(name).finalize(&keys)?;
        let relay_list = RelayList::new(
            relay_urls
                .iter()
                .filter_map(|s| RelayUrl::parse(s).ok().map(|url| (url, None))),
        )
        .finalize(&keys)?;

        // Save to global cache so subsequent commands don't need to fetch
        save_event_in_global_cache(git_repo_path, &profile).await?;
        save_event_in_global_cache(git_repo_path, &relay_list).await?;

        if publish {
            // Account creation publishes before the complete login object is
            // returned. Attach these newly created keys explicitly so the
            // selected outbox can request NIP-42 authentication.
            client.nip42_set_auth_signer(Arc::new(crate::NgitSigner::Keys(keys.clone())));
            let _ = send_events(
                client,
                git_repo_path,
                vec![profile, relay_list],
                relay_urls,
                vec![],
                true,
                false,
            )
            .await?;
        }
    }

    Ok((
        Arc::new(crate::NgitSigner::Keys(keys.clone())),
        public_key,
        config_signer_info,
        keys,
    ))
}

async fn signup(
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
) -> Result<
    Option<(
        Arc<crate::NgitSigner>,
        PublicKey,
        SignerInfo,
        SignerInfoSource,
    )>,
> {
    eprintln!("create account");
    loop {
        let name = Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("user display name")
                    .optional()
                    .dont_report(),
            )
            .context("failed to get display name input from interactor")?;
        if name.is_empty() {
            show_prompt_error("empty display name", "");
            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec![
                        "enter non-empty display name".to_string(),
                        "back to login menu".to_string(),
                    ])
                    .dont_report(),
            )? {
                0 => continue,
                _ => break Ok(None),
            }
        }

        // Call the non-interactive function, using relay_default_set as the
        // relay list for interactive signup
        let relay_urls = if let Some(c) = client {
            c.get_relay_default_set().clone()
        } else {
            vec![]
        };
        let (signer, public_key, signer_info, _keys) = signup_non_interactive(
            name.clone(),
            client,
            false, // save_local = false (will be saved globally by caller)
            true,  // publish = true (always publish in interactive mode)
            relay_urls,
        )
        .await?;

        show_prompt_success("user display name", &name);
        eprintln!(
            "to login to other nostr clients eg. gitworkshop.dev with this account run `ngit export-keys` at any time to reveal your nostr account secret"
        );
        break Ok(Some((
            signer,
            public_key,
            signer_info,
            // TODO factor in source
            SignerInfoSource::GitGlobal,
        )));
    }
}

async fn display_login_help_content() {
    let mut printer = Printer::default();
    let title_style = Style::new().bold().fg(console::Color::Yellow);
    printer.println("|==============================|".to_owned());
    printer.println_with_custom_formatting(
        format!(
            "|  {}  |",
            title_style.apply_to("nostr login / sign up help")
        ),
        "|  nostr login / sign up help  |".to_string(),
    );
    printer.println("|==============================|".to_owned());
    print_lines_with_headings(
        vec![
            "# What is a Nostr account?",
            "A Nostr account consists of a secret key you control, known as an 'nsec' and a corresponding public key called an 'npub.' Clients like ngit and gitworkshop.dev use your keys to sign messages and verify that other messages are signed by the correct keys.",
            "",
            "# How do I sign into an existing Nostr account?",
            "1. Using your secret key (nsec): Export your nsec from an existing client or browser extension. Run `ngit login` and enter your nsec.",
            "2. Using Nostr Connect.",
            "",
            "# What is Nostr Connect?",
            "Nostr Connect allows you to use multiple clients without sharing your secret key. A signer app manages your secret and signs messages on behalf of connected clients. This technology is new, and as of December 2024, only Amber for Android is recommended.",
            "",
            "# If I create a Nostr account using ngit, how can I sign in with other Nostr clients?",
            "You can export your secret key by running `ngit export-key` and import it into another client.",
            "",
            "press ctrl + c to return the login / sign up menu again...",
        ],
        &mut printer,
    );
    let _ = signal::ctrl_c().await;
    printer.clear_all();
}

fn print_lines_with_headings(lines: Vec<&str>, printer: &mut Printer) {
    let heading_style = Style::new().bold();
    for line in lines {
        if line.starts_with("# ") {
            let s = line.replace("# ", "").to_string();
            printer.println_with_custom_formatting(heading_style.apply_to(&s).to_string(), s);
        } else {
            printer.println(line.to_string());
        }
    }
}

#[cfg(test)]
mod storage_reporting_tests {
    use super::{CredentialStorageReport, credential_storage_messages};
    use crate::login::credential_store::Backend;

    #[test]
    fn file_fallback_reports_what_was_stored_before_the_reason() {
        let secret = CredentialStorageReport {
            subject: "account secret".to_string(),
            entry: "npub1account".to_string(),
            backend: Backend::File,
            os_fallback: true,
        };
        let alias = CredentialStorageReport {
            subject: "signer alias 'dcagent'".to_string(),
            entry: "alias:dcagent".to_string(),
            backend: Backend::File,
            os_fallback: true,
        };
        let path = "/home/dcdev/.local/share/ngit/credentials.json";

        assert_eq!(
            credential_storage_messages(Some(&secret), Some(&alias), path),
            vec![format!(
                "signer alias 'dcagent' and associated account secret are stored in {path} (OS credential store unavailable)"
            )]
        );
        assert_eq!(
            credential_storage_messages(Some(&secret), None, path),
            vec![format!(
                "account secret is stored in {path} (OS credential store unavailable)"
            )]
        );
    }

    #[test]
    fn empty_receipts_produce_no_messages() {
        assert!(credential_storage_messages(None, None, "unused").is_empty());
    }

    #[test]
    fn os_storage_combines_alias_and_secret_entries() {
        let secret = CredentialStorageReport {
            subject: "account secret".to_string(),
            entry: "npub1account".to_string(),
            backend: Backend::Os,
            os_fallback: false,
        };
        let alias = CredentialStorageReport {
            subject: "signer alias 'dcagent'".to_string(),
            entry: "alias:dcagent".to_string(),
            backend: Backend::Os,
            os_fallback: false,
        };

        assert_eq!(
            credential_storage_messages(Some(&secret), Some(&alias), "unused"),
            vec![
                "signer alias 'dcagent' and associated account secret are stored in the OS credential store as entries 'alias:dcagent' and 'npub1account' under service 'nostr'"
                    .to_string()
            ]
        );
    }
}

#[cfg(test)]
mod status_message_tests {
    use nostr::prelude::{Keys, ToBech32};

    use super::configured_signer_scope_message;
    use crate::login::SignerInfo;

    #[test]
    fn configured_scope_status_distinguishes_selection_and_storage() {
        let pointer = Keys::generate().public_key().to_bech32().unwrap();
        for (global, signer_info, expected) in [
            (
                false,
                SignerInfo::Selection {
                    selector: "dcagent".to_string(),
                },
                "this repository's local Git config now selects signer alias 'dcagent'",
            ),
            (
                true,
                SignerInfo::Nsec {
                    nsec: pointer.clone(),
                    password: None,
                    npub: None,
                    verify_npub: false,
                },
                "global Git config now points to the stored account secret",
            ),
            (
                false,
                SignerInfo::Nsec {
                    nsec: "nsec1plaintext".to_string(),
                    password: None,
                    npub: None,
                    verify_npub: false,
                },
                "this repository's local Git config now contains the account secret in plaintext",
            ),
            (
                false,
                SignerInfo::Bunker {
                    bunker_uri: "bunker://example".to_string(),
                    bunker_app_key: pointer,
                    npub: None,
                },
                "this repository's local Git config now points to the stored remote signer connection",
            ),
        ] {
            assert_eq!(
                configured_signer_scope_message(global, &signer_info),
                expected
            );
        }
    }
}

#[cfg(test)]
mod selection_persistence_tests {
    use super::silently_save_to_git_config;
    use crate::{
        git::{Repo, RepoActions, test_helpers::GitTestRepo},
        login::SignerInfo,
    };

    #[test]
    fn direct_selection_persistence_rejects_an_unresolved_alias() -> anyhow::Result<()> {
        let fixture = GitTestRepo::new("main")?;
        let repo = Repo::from_path(&fixture.dir)?;
        let selection = SignerInfo::Selection {
            selector: "dcagent".to_string(),
        };

        let error = silently_save_to_git_config(&Some(&repo), &selection, false)
            .expect_err("an unresolved alias must not be written as an npub");

        assert!(error.to_string().contains("unresolved signer selection"));
        assert!(
            repo.get_git_config_item("nostr.signer", Some(false))?
                .is_none()
        );
        assert!(
            repo.get_git_config_item("nostr.npub", Some(false))?
                .is_none()
        );
        Ok(())
    }
}
