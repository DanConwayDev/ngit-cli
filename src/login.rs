use std::str::FromStr;

use anyhow::{bail, Context, Result};
use nostr::PublicKey;
use nostr_database::Order;
use nostr_sdk::{Alphabet, FromBech32, JsonUtil, Kind, NostrDatabase, SingleLetterTag, ToBech32};
use nostr_sqlite::SQLiteDatabase;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms, PromptPasswordParms,
    },
    client::Connect,
    config::{get_dirs, UserMetadata, UserRef, UserRelayRef, UserRelays},
    git::{Repo, RepoActions},
    key_handling::encryption::{decrypt_key, encrypt_key},
};

/// handles the encrpytion and storage of key material
pub async fn launch(
    git_repo: &Repo,
    nsec: &Option<String>,
    password: &Option<String>,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    change_user: bool,
) -> Result<(nostr::Keys, UserRef)> {
    if let Ok(keys) = match get_keys_without_prompts(git_repo, nsec, password, change_user) {
        Ok(keys) => Ok(keys),
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
                        if let Ok(user_ref) = get_user_details(&public_key, client, git_repo).await
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
                        break Ok(keys);
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
        let user_ref = get_user_details(&keys.public_key(), client, git_repo).await?;
        print_logged_in_as(&user_ref, client.is_none())?;
        Ok((keys, user_ref))
    } else {
        fresh_login(git_repo, client, change_user).await
    }
}

fn print_logged_in_as(user_ref: &UserRef, offline_mode: bool) -> Result<()> {
    if !offline_mode && user_ref.metadata.created_at.eq(&0) {
        println!("cannot find your account metadata (name, etc) on relays");
    } else if !offline_mode && user_ref.metadata.name.eq(&user_ref.public_key.to_bech32()?) {
        println!("cannot extract account name from account metadata...");
    } else if !offline_mode && user_ref.relays.created_at.eq(&0) {
        println!(
            "cannot find your relay list. consider using another nostr client to create one to enhance your nostr experience."
        );
    }
    println!("logged in as {}", user_ref.metadata.name);
    Ok(())
}

fn get_keys_without_prompts(
    git_repo: &Repo,
    nsec: &Option<String>,
    password: &Option<String>,
    save_local: bool,
) -> Result<nostr::Keys> {
    if let Some(nsec) = nsec {
        get_keys_from_nsec(git_repo, nsec, password, save_local)
    } else if let Some(password) = password {
        get_keys_with_password(git_repo, password)
    } else if !save_local {
        get_keys_with_git_config_nsec_without_prompts(git_repo)
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
        git_repo
            .save_git_config_item("nostr.nsec", &s, false)
            .context("failed to save encrypted nsec in local git config nostr.nsec")?;
        git_repo.save_git_config_item("nostr.npub", &keys.public_key().to_bech32()?, false)?;
    }
    Ok(keys)
}

fn get_keys_with_password(git_repo: &Repo, password: &str) -> Result<nostr::Keys> {
    decrypt_key(
        &git_repo
            .get_git_config_item("nostr.nsec", false)
            .context("failed get git config")?
            .context("git config item nostr.nsec doesn't exist so cannot decrypt it")?,
        password,
    )
    .context("failed to decrypt stored nsec key with provided password")
}

fn get_keys_with_git_config_nsec_without_prompts(git_repo: &Repo) -> Result<nostr::Keys> {
    let nsec = &git_repo
        .get_git_config_item("nostr.nsec", false)
        .context("failed get git config")?
        .context("git config item nostr.nsec doesn't exist")?;
    if nsec.contains("ncryptsec") {
        bail!("git config item nostr.nsec is an ncryptsec")
    }
    nostr::Keys::from_str(nsec).context("invalid nsec parameter")
}

async fn fresh_login(
    git_repo: &Repo,
    #[cfg(test)] client: Option<&MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    always_save: bool,
) -> Result<(nostr::Keys, UserRef)> {
    // prompt for nsec
    let mut prompt = "login with nsec";
    let keys = loop {
        match nostr::Keys::from_str(
            &Interactor::default()
                .input(PromptInputParms::default().with_prompt(prompt))
                .context("failed to get nsec input from interactor")?,
        ) {
            Ok(key) => {
                break key;
            }
            Err(_) => {
                prompt = "invalid nsec. try again with nsec (or hex private key)";
            }
        }
    };
    // lookup profile
    // save keys
    if let Err(error) = save_keys(git_repo, &keys, always_save) {
        println!("{error}");
    }
    let user_ref = get_user_details(&keys.public_key(), client, git_repo).await?;
    print_logged_in_as(&user_ref, client.is_none())?;
    Ok((keys, user_ref))
}

fn save_keys(git_repo: &Repo, keys: &nostr::Keys, always_save: bool) -> Result<()> {
    let store = always_save
        || Interactor::default()
            .confirm(PromptConfirmParms::default().with_prompt("save login details?"))?;

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

    if store {
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

        if let Err(error) = git_repo.save_git_config_item("nostr.nsec", &nsec_string, global) {
            if global {
                println!("failed to edit global git config instead");
                if Interactor::default().confirm(
                    PromptConfirmParms::default()
                        .with_prompt("save in repository git config?")
                        .with_default(true),
                )? {
                    git_repo.save_git_config_item("nostr.nsec", &nsec_string, false)?;
                    git_repo.save_git_config_item("nostr.npub", &npub, false)?;
                }
            } else {
                bail!(error)
            }
        } else {
            git_repo.save_git_config_item("nostr.npub", &npub, global)?;
        };
    };
    Ok(())
}

fn get_config_item(git_repo: &Repo, name: &str) -> Result<String> {
    git_repo
        .get_git_config_item(name, false)
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
            event.created_at.as_u64()
        } else {
            0
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
            event.created_at.as_u64()
        } else {
            0
        },
    }
}

async fn get_user_details(
    public_key: &PublicKey,
    #[cfg(test)] client: Option<&crate::client::MockConnect>,
    #[cfg(not(test))] client: Option<&Client>,
    git_repo: &Repo,
) -> Result<UserRef> {
    if client.is_some() {
        println!("searching for profile and relay updates...");
    }
    let database = SQLiteDatabase::open(if std::env::var("NGITTEST").is_err() {
        get_dirs()?.config_dir().join("cache.sqlite")
    } else {
        git_repo.get_path()?.join(".git/test-global-cache.sqlite")
    })
    .await?;
    let mut events: Vec<nostr::Event> = vec![];
    let filters = vec![
        nostr::Filter::default()
            .author(*public_key)
            .kind(Kind::Metadata),
        nostr::Filter::default()
            .author(*public_key)
            .kind(Kind::RelayList),
    ];
    if let Ok(cached_events) = database.query(filters.clone(), Order::Asc).await {
        for event in cached_events {
            events.push(event);
        }
    }
    let mut relays_to_search = if let Some(client) = client {
        client.get_fallback_relays().clone()
    } else {
        vec![]
    };
    let mut relays_searched = vec![];
    let user_ref = loop {
        if let Some(client) = client {
            for event in client
                .get_events(relays_to_search.clone(), filters.clone())
                .await
                .unwrap_or(vec![])
            {
                let _ = database.save_event(&event).await;
                events.push(event);
            }
        }

        #[allow(clippy::clone_on_copy)]
        let user_ref = UserRef {
            public_key: public_key.clone(),
            metadata: extract_user_metadata(public_key, &events)?,
            relays: extract_user_relays(public_key, &events),
        };

        if client.is_none() {
            break user_ref;
        }
        for r in &relays_to_search {
            relays_searched.push(r.clone());
        }

        relays_to_search = user_ref
            .relays
            .write()
            .iter()
            .filter(|r| !relays_searched.iter().any(|or| r.eq(&or)))
            .map(std::clone::Clone::clone)
            .collect();
        if !relays_to_search.is_empty() {
            continue;
        }
        break user_ref;
    };
    Ok(user_ref)
}
