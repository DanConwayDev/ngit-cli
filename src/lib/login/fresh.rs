use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use console::Style;
use dialoguer::theme::{ColorfulTheme, Theme};
use nostr::nips::{nip05, nip46::NostrConnectURI};
use nostr_connect::client::NostrConnect;
use nostr_sdk::{EventBuilder, Keys, Metadata, NostrSigner, PublicKey, ToBech32, Url};
use qrcode::QrCode;
use tokio::{signal, sync::Mutex};

use super::{
    key_encryption::decrypt_key,
    print_logged_in_as,
    user::{get_user_details, UserRef},
    SignerInfo, SignerInfoSource,
};
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, Printer, PromptChoiceParms, PromptConfirmParms,
        PromptInputParms, PromptPasswordParms,
    },
    client::{send_events, Connect},
    git::{remove_git_config_item, save_git_config_item, Repo, RepoActions},
};

pub async fn fresh_login_or_signup(
    git_repo: &Option<&Repo>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    save_local: bool,
) -> Result<(Arc<dyn NostrSigner>, UserRef, SignerInfoSource)> {
    let (signer, public_key, signer_info, source) = loop {
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
                Err(e) => {
                    eprintln!("error getting fresh signer from nsec: {e}");
                    continue;
                }
            },
            1 => match get_fresh_nip46_signer(client).await {
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
    let _ = save_to_git_config(git_repo, &signer_info, !save_local);
    let user_ref = get_user_details(
        &public_key,
        client,
        if let Some(git_repo) = git_repo {
            Some(git_repo.get_path()?)
        } else {
            None
        },
        false,
    )
    .await?;
    print_logged_in_as(&user_ref, client.is_none(), &source)?;
    Ok((signer, user_ref, source))
}

pub async fn get_fresh_nsec_signer() -> Result<
    Option<(
        Arc<dyn NostrSigner>,
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
                    .optional()
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
                }
            } else {
                show_prompt_success("nsec", &shorten_string(&input));
                SignerInfo::Nsec {
                    nsec: input,
                    password: Some(password),
                    npub,
                }
            };
            (keys, signer_info)
        } else if let Ok(keys) = nostr::Keys::from_str(&input) {
            let nsec = keys.secret_key().to_bech32()?;
            show_prompt_success("nsec", &shorten_string(&nsec));
            let signer_info = SignerInfo::Nsec {
                nsec,
                password: None,
                npub: Some(keys.public_key().to_bech32()?),
            };
            (keys, signer_info)
        } else {
            show_prompt_error("invalid nsec", &shorten_string(&input));
            match Interactor::default().choice(
                PromptChoiceParms::default()
                    .with_default(0)
                    .with_choices(vec!["try again with nsec".to_string(), "back".to_string()])
                    .dont_report(),
            )? {
                0 => continue,
                _ => break Ok(None),
            }
        };

        let public_key = keys.public_key();

        break Ok(Some((
            Arc::new(keys),
            public_key,
            signer_info,
            // TODO factor in source
            SignerInfoSource::GitGlobal,
        )));
    }
}

fn show_prompt_success(label: &str, value: &str) {
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
                "{label}: {}",
                if value.is_empty() {
                    "empty".to_string()
                } else {
                    shorten_string(&format!("\"{}\"", &value))
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
) -> Result<
    Option<(
        Arc<dyn NostrSigner>,
        PublicKey,
        SignerInfo,
        SignerInfoSource,
    )>,
> {
    let (app_key, nostr_connect_url) = generate_nostr_connect_app(client)?;
    let printer = Arc::new(Mutex::new(Printer::default()));
    let signer_choice = Interactor::default().choice(
        PromptChoiceParms::default()
            .with_prompt("login to nostr with remote signer")
            .with_default(0)
            .with_choices(vec![
                "show QR code to scan in signer app".to_string(),
                "show nostrconnect:// url to paste into signer".to_string(),
                "use NIP-05 address to connect to signer".to_string(),
                "paste in bunker:// url from signer app".to_string(),
                "back".to_string(),
            ])
            .dont_report(),
    )?;
    let url = match signer_choice {
        0 | 1 => nostr_connect_url,
        2 => {
            let mut error = None;
            loop {
                let input = Interactor::default()
                    .input(
                        PromptInputParms::default().with_prompt(if let Some(error) = error {
                            format!("error: {}. try again with NIP-05 address", error)
                        } else {
                            "NIP-05 address".to_string()
                        }),
                    )
                    .context("failed to get NIP-05 address input from interactor")?;
                match fetch_nip46_uri_from_nip05(&input).await {
                    Ok(url) => break url,
                    Err(e) => error = Some(e),
                }
            }
        }
        3 => {
            let mut error = None;
            loop {
                let input = Interactor::default()
                    .input(
                        PromptInputParms::default().with_prompt(if let Some(error) = error {
                            format!("error: {}. try again with bunker url", error)
                        } else {
                            "bunker url".to_string()
                        }),
                    )
                    .context("failed to get bunker url input from interactor")?;
                match NostrConnectURI::parse(&input) {
                    Ok(url) => break url,
                    Err(e) => error = Some(e),
                }
            }
        }
        _ => return Ok(None),
    };

    {
        let printer_clone = Arc::clone(&printer);
        let mut printer_locked = printer_clone.lock().await;
        match signer_choice {
            0 => {
                printer_locked
                    .println("login to nostr with remote signer via nostr connect".to_string());
                printer_locked.println("scan QR code in signer app (eg Amber):".to_string());
                printer_locked.printlns(generate_qr(&url.to_string())?);
                printer_locked.println(
                    "scan QR code in signer app or use ctrl + c to go back to login menu..."
                        .to_string(),
                );
            }
            1 => {
                printer_locked
                    .println("login to nostr with remote signer via nostr connect".to_string());
                printer_locked.println("".to_string());
                printer_locked.println_with_custom_formatting(
                    format!("{}", Style::new().bold().apply_to(url.to_string()),),
                    url.to_string(),
                );
                printer_locked.println("".to_string());
                printer_locked.println(
                    "paste this url into signer app or use ctrl + c to go back to login menu..."
                        .to_string(),
                );
            }
            _ => {
                printer_locked.println(
                    "add / approve in your signer or use ctrl + c to go back to login menu..."
                        .to_string(),
                );
            }
        }
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
) -> Result<(Keys, NostrConnectURI)> {
    let app_key = Keys::generate();
    let relays = if let Some(client) = client {
        client
            .get_fallback_signer_relays()
            .iter()
            .flat_map(|s| Url::parse(s))
            .collect::<Vec<Url>>()
    } else {
        vec![]
    };
    let nostr_connect_url = NostrConnectURI::client(app_key.public_key(), relays.clone(), "ngit");
    Ok((app_key, nostr_connect_url))
}

pub async fn fetch_nip46_uri_from_nip05(nip05: &str) -> Result<NostrConnectURI> {
    let term = console::Term::stderr();
    term.write_line("contacting login service provider...")?;
    let res = nip05::profile(&nip05, None).await;
    term.clear_last_lines(1)?;
    match res {
        Ok(profile) => {
            if profile.nip46.is_empty() {
                eprintln!("nip05 provider isn't configured for remote login");
                bail!("nip05 provider isn't configured for remote login")
            }
            Ok(NostrConnectURI::Bunker {
                remote_signer_public_key: profile.public_key,
                relays: profile.nip46,
                secret: None,
            })
        }
        Err(error) => {
            eprintln!("error contacting login service provider: {error}");
            Err(error).context("error contacting login service provider")
        }
    }
}

pub async fn listen_for_remote_signer(
    app_key: &Keys,
    nostr_connect_url: &NostrConnectURI,
    printer: Arc<Mutex<Printer>>,
) -> Result<(Arc<dyn NostrSigner>, PublicKey, NostrConnectURI)> {
    let app_key = app_key.clone();
    let nostr_connect_url_clone = nostr_connect_url.clone();

    let nostr_connect = NostrConnect::new(
        nostr_connect_url_clone,
        app_key,
        Duration::from_secs(10 * 60),
        None,
    )?;
    let signer: Arc<dyn NostrSigner> = Arc::new(nostr_connect);
    let pubkey_future = signer.get_public_key();

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
        let bunker_url = NostrConnectURI::Bunker {
            // TODO the remote signer pubkey may not be the user pubkey
            remote_signer_public_key: public_key,
            relays: nostr_connect_url.relays(),
            secret: nostr_connect_url.secret(),
        };
        Ok((signer, public_key, bunker_url))
    } else {
        bail!("failed to get signer")
    }
}

fn generate_qr(data: &str) -> Result<Vec<String>> {
    let mut lines = vec![];
    let qr =
        QrCode::new(data.as_bytes()).context("failed to create QR of nostrconnect login url")?;
    let colors = qr.to_colors();
    let rows: Vec<&[qrcode::Color]> = colors.chunks(qr.width()).collect();
    for (row, data) in rows.iter().enumerate() {
        let odd = row % 2 != 0;
        if odd {
            continue;
        }
        let mut line = String::new();
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

fn save_to_git_config(
    git_repo: &Option<&Repo>,
    signer_info: &SignerInfo,
    global: bool,
) -> Result<()> {
    if let Err(error) = silently_save_to_git_config(git_repo, signer_info, global).context(format!(
        "failed to save login details to {} git config",
        if global { "global" } else { "local" }
    )) {
        eprintln!("Error: {:?}", error);
        match signer_info {
            SignerInfo::Nsec {
                nsec,
                password: _,
                npub: _,
            } => {
                if nsec.contains("ncryptsec") {
                    eprintln!("consider manually setting git config nostr.nsec to: {nsec}");
                } else {
                    eprintln!("consider manually setting git config nostr.nsec");
                }
            }
            SignerInfo::Bunker {
                bunker_uri,
                bunker_app_key,
                npub: _,
            } => {
                eprintln!("consider manually setting git config as follows:");
                eprintln!("nostr.bunker-uri: {bunker_uri}");
                eprintln!("nostr.bunker-app-key: {bunker_app_key}");
            }
        }
        if global {
            save_to_git_config(git_repo, signer_info, false)?
        }
        Err(error)
    } else {
        eprintln!(
            "{}",
            if global {
                "saved login details to global git config"
            } else {
                "saved login details to local git config. you are only logged in to this local repository."
            }
        );
        Ok(())
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
        }
    }

    let git_repo = if global {
        &None
    } else if git_repo.is_none() {
        bail!("cannot update local git config wihout git_repo object")
    } else {
        git_repo
    };

    let npub_to_save;
    match signer_info {
        SignerInfo::Nsec {
            nsec,
            password: _,
            npub,
        } => {
            npub_to_save = npub;
            save_git_config_item(git_repo, "nostr.nsec", nsec)?;
            remove_git_config_item(git_repo, "nostr.bunker-uri")?;
            remove_git_config_item(git_repo, "nostr.bunker-app-key")?;
        }
        SignerInfo::Bunker {
            bunker_uri,
            bunker_app_key,
            npub,
        } => {
            npub_to_save = npub;
            remove_git_config_item(git_repo, "nostr.nsec")?;
            save_git_config_item(git_repo, "nostr.bunker-uri", bunker_uri)?;
            save_git_config_item(git_repo, "nostr.bunker-app-key", bunker_app_key)?;
        }
    }
    if let Some(npub) = npub_to_save {
        save_git_config_item(git_repo, "nostr.npub", npub)?;
    } else {
        remove_git_config_item(git_repo, "nostr.npub")?;
    }
    Ok(())
}

async fn signup(
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
) -> Result<
    Option<(
        Arc<dyn NostrSigner>,
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
            show_prompt_error("emtpy display name", "");
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
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32()?;
        show_prompt_success("user display name", &name);
        let signer_info = SignerInfo::Nsec {
            nsec,
            password: None,
            npub: Some(keys.public_key().to_bech32()?),
        };
        let public_key = keys.public_key();
        if let Some(client) = client {
            let profile =
                EventBuilder::metadata(&Metadata::new().name(name)).sign_with_keys(&keys)?;
            let relay_list = EventBuilder::relay_list(
                client
                    .get_fallback_relays()
                    .iter()
                    .map(|s| (Url::parse(s).unwrap(), None)),
            )
            .sign_with_keys(&keys)?;
            eprintln!("publishing user profile to relays");
            send_events(
                client,
                None,
                vec![profile, relay_list],
                client.get_fallback_relays().clone(),
                vec![],
                true,
                false,
            )
            .await?;
        }
        eprintln!("TODO: advice about using in other clients");
        break Ok(Some((
            Arc::new(keys),
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
    // printer.println("|                              |".to_owned());
    printer.println_with_custom_formatting(
        format!(
            "|  {}  |",
            title_style.apply_to("nostr login / sign up help")
        ),
        "|  nostr login / sign up help  |".to_string(),
    );
    // printer.println("|                              |".to_owned());
    printer.println("|==============================|".to_owned());
    printer.printlns(vec![
        "".to_string(),
        "login / sign up help content should go here...".to_string(),
        "press ctrl + c to return the login / sign up menu again...".to_string(),
    ]);
    let _ = signal::ctrl_c().await;
    printer.clear_all();
}
