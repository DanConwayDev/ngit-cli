use std::{collections::HashSet, path::Path, str::FromStr, time::Duration};

use anyhow::{bail, Context, Result};
use nostr::{
    nips::{nip05, nip46::NostrConnectURI},
    PublicKey,
};
use nostr_sdk::{
    Alphabet, FromBech32, JsonUtil, Keys, Kind, NostrSigner, SingleLetterTag, Timestamp, ToBech32,
};
use nostr_signer::Nip46Signer;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms, PromptPasswordParms,
    },
    client::{fetch_public_key, get_event_from_global_cache, Connect},
    git::{Repo, RepoActions},
};

mod key_encryption;
use key_encryption::{decrypt_key, encrypt_key};
mod user;
use user::{UserMetadata, UserRef, UserRelayRef, UserRelays};

/// handles the encrpytion and storage of key material
#[allow(clippy::too_many_arguments)]
pub async fn launch(
    git_repo: &Repo,
    bunker_uri: &Option<String>,
    bunker_app_key: &Option<String>,
    nsec: &Option<String>,
    password: &Option<String>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    change_user: bool,
    silent: bool,
) -> Result<(NostrSigner, UserRef)> {
    if let Ok(signer) = match get_signer_without_prompts(
        git_repo,
        bunker_uri,
        bunker_app_key,
        nsec,
        password,
        change_user,
    )
    .await
    {
        Ok(signer) => Ok(signer),
        Err(error) => {
            if error
                .to_string()
                .eq("git config item nostr.nsec is an ncryptsec")
            {
                println!(
                    "login as {}",
                    if let Ok(public_key) = PublicKey::from_bech32(
                        get_config_item(git_repo, "nostr.npub")
                            .unwrap_or("unknown ncryptsec".to_string()),
                    ) {
                        if let Ok(user_ref) =
                            get_user_details(&public_key, client, git_repo.get_path()?, silent)
                                .await
                        {
                            user_ref.metadata.name
                        } else {
                            "unknown ncryptsec".to_string()
                        }
                    } else {
                        "unknown ncryptsec".to_string()
                    }
                );
                loop {
                    // prompt for password
                    let password = Interactor::default()
                        .password(PromptPasswordParms::default().with_prompt("password"))
                        .context("failed to get password input from interactor.password")?;
                    if let Ok(keys) = get_keys_with_password(git_repo, &password) {
                        break Ok(NostrSigner::Keys(keys));
                    }
                    println!("incorrect password");
                }
            } else {
                if nsec.is_some() {
                    bail!(error);
                }
                Err(error)
            }
        }
    } {
        // get user ref
        let user_ref = get_user_details(
            &signer
                .public_key()
                .await
                .context("cannot get public key from signer")?,
            client,
            git_repo.get_path()?,
            silent,
        )
        .await?;
        if !silent {
            print_logged_in_as(&user_ref, client.is_none())?;
        }
        Ok((signer, user_ref))
    } else if silent {
        bail!("TODO: enable interactive login in nostr git remote helper");
    } else {
        fresh_login(git_repo, client, change_user).await
    }
}

fn print_logged_in_as(user_ref: &UserRef, offline_mode: bool) -> Result<()> {
    if !offline_mode && user_ref.metadata.created_at.eq(&Timestamp::from(0)) {
        println!("cannot find profile...");
    } else if !offline_mode && user_ref.metadata.name.eq(&user_ref.public_key.to_bech32()?) {
        println!("cannot extract account name from account metadata...");
    } else if !offline_mode && user_ref.relays.created_at.eq(&Timestamp::from(0)) {
        println!(
            "cannot find your relay list. consider using another nostr client to create one to enhance your nostr experience."
        );
    }
    println!("logged in as {}", user_ref.metadata.name);
    Ok(())
}

async fn get_signer_without_prompts(
    git_repo: &Repo,
    bunker_uri: &Option<String>,
    bunker_app_key: &Option<String>,
    nsec: &Option<String>,
    password: &Option<String>,
    save_local: bool,
) -> Result<NostrSigner> {
    if let Some(nsec) = nsec {
        Ok(NostrSigner::Keys(get_keys_from_nsec(
            git_repo, nsec, password, save_local,
        )?))
    } else if let Some(password) = password {
        Ok(NostrSigner::Keys(get_keys_with_password(
            git_repo, password,
        )?))
    } else if let Some(bunker_uri) = bunker_uri {
        if let Some(bunker_app_key) = bunker_app_key {
            let signer = get_nip46_signer_from_uri_and_key(bunker_uri, bunker_app_key)
                .await
                .context("failed to connect with remote signer")?;
            if save_local {
                save_to_git_config(
                    git_repo,
                    &signer.public_key().await?.to_bech32()?,
                    &None,
                    &Some((bunker_uri.to_string(),bunker_app_key.to_string())),
                    false,
                )
                    .context("failed to save bunker details local git config nostr.bunker-uri and nostr.bunker-app-key")?;
            }
            Ok(signer)
        } else {
            bail!(
                "bunker-app-key parameter must be provided alongside bunker-uri. if unknown, login interactively."
            )
        }
    } else if !save_local {
        get_signer_with_git_config_nsec_or_bunker_without_prompts(git_repo).await
    } else {
        bail!("user wants prompts to specify new keys")
    }
}

fn get_keys_from_nsec(
    git_repo: &Repo,
    nsec: &String,
    password: &Option<String>,
    save_local: bool,
) -> Result<nostr::Keys> {
    #[allow(unused_assignments)]
    let mut s = String::new();
    let keys = if nsec.contains("ncryptsec") {
        s = nsec.to_string();
        decrypt_key(
            nsec,
            password
                .clone()
                .context("password must be supplied when using ncryptsec as nsec parameter")?
                .as_str(),
        )
        .context("failed to decrypt key with provided password")
        .context("failed to decrypt ncryptsec supplied as nsec with password")?
    } else {
        s = nsec.to_string();
        nostr::Keys::from_str(nsec).context("invalid nsec parameter")?
    };
    if save_local {
        if let Some(password) = password {
            s = encrypt_key(&keys, password)?;
        }
        save_to_git_config(
            git_repo,
            &keys.public_key().to_bech32()?,
            &Some(s),
            &None,
            false,
        )
        .context("failed to save encrypted nsec in local git config nostr.nsec")?;
    }
    Ok(keys)
}

fn save_to_git_config(
    git_repo: &Repo,
    npub: &str,
    nsec: &Option<String>,
    bunker: &Option<(String, String)>,
    global: bool,
) -> Result<()> {
    if let Err(error) = silently_save_to_git_config(git_repo, npub, nsec, bunker, global) {
        println!(
            "failed to save login details to {} git config",
            if global { "global" } else { "local" }
        );
        if let Some(nsec) = nsec {
            if nsec.contains("ncryptsec") {
                println!("manually set git config nostr.nsec to: {nsec}");
            } else {
                println!("manually set git config nostr.nsec");
            }
        }
        if let Some(bunker) = bunker {
            println!("manually set git config as follows:");
            println!("nostr.bunker-uri: {}", bunker.0);
            println!("nostr.bunker-app-key: {}", bunker.1);
        }
        Err(error)
    } else {
        println!(
            "saved login details to {} git config",
            if global { "global" } else { "local" }
        );
        Ok(())
    }
}
fn silently_save_to_git_config(
    git_repo: &Repo,
    npub: &str,
    nsec: &Option<String>,
    bunker: &Option<(String, String)>,
    global: bool,
) -> Result<()> {
    // must do this first otherwise it might remove the global items just added
    if global {
        git_repo.remove_git_config_item("nostr.npub", false)?;
        git_repo.remove_git_config_item("nostr.nsec", false)?;
        git_repo.remove_git_config_item("nostr.bunker-uri", false)?;
        git_repo.remove_git_config_item("nostr.bunker-app-key", false)?;
    }
    if let Some(bunker) = bunker {
        git_repo.remove_git_config_item("nostr.nsec", global)?;
        git_repo.save_git_config_item("nostr.bunker-uri", &bunker.0, global)?;
        git_repo.save_git_config_item("nostr.bunker-app-key", &bunker.1, global)?;
    }
    if let Some(nsec) = nsec {
        git_repo.save_git_config_item("nostr.nsec", nsec, global)?;
        git_repo.remove_git_config_item("nostr.bunker-uri", global)?;
        git_repo.remove_git_config_item("nostr.bunker-app-key", global)?;
    }
    git_repo.save_git_config_item("nostr.npub", npub, global)
}

fn get_keys_with_password(git_repo: &Repo, password: &str) -> Result<nostr::Keys> {
    decrypt_key(
        &git_repo
            .get_git_config_item("nostr.nsec", None)
            .context("failed get git config")?
            .context("git config item nostr.nsec doesn't exist so cannot decrypt it")?,
        password,
    )
    .context("failed to decrypt stored nsec key with provided password")
}

async fn get_nip46_signer_from_uri_and_key(uri: &str, app_key: &str) -> Result<NostrSigner> {
    let term = console::Term::stderr();
    term.write_line("connecting to remote signer...")?;
    let uri = NostrConnectURI::parse(uri)?;
    let signer = NostrSigner::nip46(
        Nip46Signer::new(
            uri,
            nostr::Keys::from_str(app_key).context("invalid app key")?,
            Duration::from_secs(30),
            None,
        )
        .await?,
    );
    term.clear_last_lines(1)?;
    Ok(signer)
}

async fn get_signer_with_git_config_nsec_or_bunker_without_prompts(
    git_repo: &Repo,
) -> Result<NostrSigner> {
    if let Ok(local_nsec) = &git_repo
        .get_git_config_item("nostr.nsec", Some(false))
        .context("failed get local git config")?
        .context("git local config item nostr.nsec doesn't exist")
    {
        if local_nsec.contains("ncryptsec") {
            bail!("git global config item nostr.nsec is an ncryptsec")
        }
        Ok(NostrSigner::Keys(
            nostr::Keys::from_str(local_nsec).context("invalid nsec parameter")?,
        ))
    } else if let Ok((uri, app_key)) = get_git_config_bunker_uri_and_app_key(git_repo, Some(false))
    {
        get_nip46_signer_from_uri_and_key(&uri, &app_key).await
    } else if let Ok(global_nsec) = &git_repo
        .get_git_config_item("nostr.nsec", Some(true))
        .context("failed get global git config")?
        .context("git global config item nostr.nsec doesn't exist")
    {
        if global_nsec.contains("ncryptsec") {
            bail!("git global config item nostr.nsec is an ncryptsec")
        }
        Ok(NostrSigner::Keys(
            nostr::Keys::from_str(global_nsec).context("invalid nsec parameter")?,
        ))
    } else if let Ok((uri, app_key)) = get_git_config_bunker_uri_and_app_key(git_repo, Some(true)) {
        get_nip46_signer_from_uri_and_key(&uri, &app_key).await
    } else {
        bail!("cannot get nsec or bunker from git config")
    }
}

fn get_git_config_bunker_uri_and_app_key(
    git_repo: &Repo,
    global: Option<bool>,
) -> Result<(String, String)> {
    Ok((
        git_repo
            .get_git_config_item("nostr.bunker-uri", global)
            .context("failed get local git config")?
            .context("git local config item nostr.bunker-uri doesn't exist")?
            .to_string(),
        git_repo
            .get_git_config_item("nostr.bunker-app-key", global)
            .context("failed get local git config")?
            .context("git local config item nostr.bunker-app-key doesn't exist")?
            .to_string(),
    ))
}

async fn fresh_login(
    git_repo: &Repo,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    always_save: bool,
) -> Result<(NostrSigner, UserRef)> {
    let mut public_key: Option<PublicKey> = None;
    // prompt for nsec
    let mut prompt = "login with nostr address / nsec";
    let signer = loop {
        let input = Interactor::default()
            .input(PromptInputParms::default().with_prompt(prompt))
            .context("failed to get nsec input from interactor")?;
        if let Ok(keys) = nostr::Keys::from_str(&input) {
            if let Err(error) = save_keys(git_repo, &keys, always_save) {
                println!("{error}");
            }
            break NostrSigner::Keys(keys);
        }
        let uri = if let Ok(uri) = NostrConnectURI::parse(&input) {
            uri
        } else if input.contains('@') {
            if let Ok(uri) = fetch_nip46_uri_from_nip05(&input).await {
                uri
            } else {
                prompt = "failed. try again with nostr address / bunker uri / nsec";
                continue;
            }
        } else {
            prompt = "invalid. try again with nostr address / bunker uri / nsec";
            continue;
        };
        let app_key = Keys::generate().secret_key()?.to_secret_hex();
        match get_nip46_signer_from_uri_and_key(&uri.to_string(), &app_key).await {
            Ok(signer) => {
                let pub_key = fetch_public_key(&signer).await?;
                if let Err(error) =
                    save_bunker(git_repo, &pub_key, &uri.to_string(), &app_key, always_save)
                {
                    println!("{error}");
                }
                public_key = Some(pub_key);
                break signer;
            }
            Err(_) => {
                prompt = "failed. try again with nostr address / bunker uri / nsec";
            }
        }
    };
    let public_key = if let Some(public_key) = public_key {
        public_key
    } else {
        signer.public_key().await?
    };
    // lookup profile
    let user_ref = get_user_details(&public_key, client, git_repo.get_path()?, false).await?;
    print_logged_in_as(&user_ref, client.is_none())?;
    Ok((signer, user_ref))
}

pub async fn fetch_nip46_uri_from_nip05(nip05: &str) -> Result<NostrConnectURI> {
    let term = console::Term::stderr();
    term.write_line("contacting login service provider...")?;
    let res = nip05::profile(&nip05, None).await;
    term.clear_last_lines(1)?;
    match res {
        Ok(profile) => {
            if profile.nip46.is_empty() {
                println!("nip05 provider isn't configured for remote login");
                bail!("nip05 provider isn't configured for remote login")
            }
            Ok(NostrConnectURI::Bunker {
                signer_public_key: profile.public_key,
                relays: profile.nip46,
                secret: None,
            })
        }
        Err(error) => {
            println!("error contacting login service provider: {error}");
            Err(error).context("error contacting login service provider")
        }
    }
}

fn save_bunker(
    git_repo: &Repo,
    public_key: &PublicKey,
    uri: &str,
    app_key: &str,
    always_save: bool,
) -> Result<()> {
    if always_save
        || Interactor::default()
            .confirm(PromptConfirmParms::default().with_prompt("save login details?"))?
    {
        let global = !Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("just for this repository?")
                .with_default(false),
        )?;
        let npub = public_key.to_bech32()?;
        if let Err(error) = save_to_git_config(
            git_repo,
            &npub,
            &None,
            &Some((uri.to_string(), app_key.to_string())),
            global,
        ) {
            if global {
                if Interactor::default().confirm(
                    PromptConfirmParms::default()
                        .with_prompt("save in repository git config?")
                        .with_default(true),
                )? {
                    save_to_git_config(
                        git_repo,
                        &npub,
                        &None,
                        &Some((uri.to_string(), app_key.to_string())),
                        false,
                    )?;
                }
            } else {
                Err(error)?;
            }
        };
    }
    Ok(())
}

fn save_keys(git_repo: &Repo, keys: &nostr::Keys, always_save: bool) -> Result<()> {
    if always_save
        || Interactor::default()
            .confirm(PromptConfirmParms::default().with_prompt("save login details?"))?
    {
        let global = !Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("just for this repository?")
                .with_default(false),
        )?;

        let encrypt = Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("require password?")
                .with_default(false),
        )?;

        let npub = keys.public_key().to_bech32()?;
        let nsec_string = if encrypt {
            let password = Interactor::default()
                .password(
                    PromptPasswordParms::default()
                        .with_prompt("encrypt with password")
                        .with_confirm(),
                )
                .context("failed to get password input from interactor.password")?;
            encrypt_key(keys, &password)?
        } else {
            keys.secret_key()?.to_bech32()?
        };

        if let Err(error) =
            save_to_git_config(git_repo, &npub, &Some(nsec_string.clone()), &None, global)
        {
            if global {
                if Interactor::default().confirm(
                    PromptConfirmParms::default()
                        .with_prompt("save in repository git config?")
                        .with_default(true),
                )? {
                    save_to_git_config(git_repo, &npub, &Some(nsec_string.clone()), &None, false)?;
                }
            } else {
                Err(error)?;
            }
        };
    };
    Ok(())
}

fn get_config_item(git_repo: &Repo, name: &str) -> Result<String> {
    git_repo
        .get_git_config_item(name, None)
        .context("failed get git config")?
        .context(format!("git config item {name} doesn't exist"))
}

fn extract_user_metadata(
    public_key: &nostr::PublicKey,
    events: &[nostr::Event],
) -> Result<UserMetadata> {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&nostr::Kind::Metadata) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    let metadata: Option<nostr::Metadata> = if let Some(event) = event {
        Some(
            nostr::Metadata::from_json(event.content.clone())
                .context("metadata cannot be found in kind 0 event content")?,
        )
    } else {
        None
    };

    Ok(UserMetadata {
        name: if let Some(metadata) = metadata {
            if let Some(n) = metadata.name {
                n
            } else if let Some(n) = metadata.custom.get("displayName") {
                // strip quote marks that custom.get() adds
                let binding = n.to_string();
                let mut chars = binding.chars();
                chars.next();
                chars.next_back();
                chars.as_str().to_string()
            } else if let Some(n) = metadata.display_name {
                n
            } else {
                public_key.to_bech32()?
            }
        } else {
            public_key.to_bech32()?
        },
        created_at: if let Some(event) = event {
            event.created_at
        } else {
            Timestamp::from(0)
        },
    })
}

fn extract_user_relays(public_key: &nostr::PublicKey, events: &[nostr::Event]) -> UserRelays {
    let event = events
        .iter()
        .filter(|e| e.kind.eq(&nostr::Kind::RelayList) && e.pubkey.eq(public_key))
        .max_by_key(|e| e.created_at);

    UserRelays {
        relays: if let Some(event) = event {
            event
                .tags
                .iter()
                .filter(|t| {
                    t.kind()
                        .eq(&nostr::TagKind::SingleLetter(SingleLetterTag::lowercase(
                            Alphabet::R,
                        )))
                })
                .map(|t| UserRelayRef {
                    url: t.as_vec()[1].clone(),
                    read: t.as_vec().len() == 2 || t.as_vec()[2].eq("read"),
                    write: t.as_vec().len() == 2 || t.as_vec()[2].eq("write"),
                })
                .collect()
        } else {
            vec![]
        },
        created_at: if let Some(event) = event {
            event.created_at
        } else {
            Timestamp::from(0)
        },
    }
}

async fn get_user_details(
    public_key: &PublicKey,
    #[cfg(test)] client: Option<&crate::client::MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    git_repo_path: &Path,
    cache_only: bool,
) -> Result<UserRef> {
    if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
        Ok(user_ref)
    } else {
        let empty = UserRef {
            public_key: public_key.to_owned(),
            metadata: extract_user_metadata(public_key, &[])?,
            relays: extract_user_relays(public_key, &[]),
        };
        if cache_only {
            Ok(empty)
        } else if let Some(client) = client {
            let term = console::Term::stderr();
            term.write_line("searching for profile...")?;
            let (_, progress_reporter) = client
                .fetch_all(
                    git_repo_path,
                    &HashSet::new(),
                    &HashSet::from_iter(vec![*public_key]),
                )
                .await?;
            if let Ok(user_ref) = get_user_ref_from_cache(git_repo_path, public_key).await {
                progress_reporter.clear()?;
                // if std::env::var("NGITTEST").is_err() {term.clear_last_lines(1)?;}
                Ok(user_ref)
            } else {
                Ok(empty)
            }
        } else {
            Ok(empty)
        }
    }
}
pub async fn get_logged_in_user(git_repo_path: &Path) -> Result<Option<PublicKey>> {
    let git_repo = Repo::from_path(&git_repo_path.to_path_buf())?;
    Ok(
        if let Some(npub) = git_repo.get_git_config_item("nostr.npub", None)? {
            if let Ok(pubic_key) = PublicKey::parse(npub) {
                Some(pubic_key)
            } else {
                None
            }
        } else {
            None
        },
    )
}

pub async fn get_user_ref_from_cache(
    git_repo_path: &Path,
    public_key: &PublicKey,
) -> Result<UserRef> {
    let filters = vec![
        nostr::Filter::default()
            .author(*public_key)
            .kind(Kind::Metadata),
        nostr::Filter::default()
            .author(*public_key)
            .kind(Kind::RelayList),
    ];

    let events = get_event_from_global_cache(git_repo_path, filters.clone()).await?;

    if events.is_empty() {
        bail!("no metadata and profile list in cache for selected public key");
    }
    Ok(UserRef {
        public_key: public_key.to_owned(),
        metadata: extract_user_metadata(public_key, &events)?,
        relays: extract_user_relays(public_key, &events),
    })
}

pub fn get_curent_user(git_repo: &Repo) -> Result<Option<PublicKey>> {
    Ok(
        if let Some(npub) = git_repo.get_git_config_item("nostr.npub", None)? {
            if let Ok(public_key) = PublicKey::parse(npub) {
                Some(public_key)
            } else {
                None
            }
        } else {
            None
        },
    )
}
