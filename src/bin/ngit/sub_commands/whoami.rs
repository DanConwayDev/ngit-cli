use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result};
use ngit::{
    client::Params,
    login::{
        SignerInfoSource, credential_store,
        existing::{
            configured_alias_names, configured_signer_npub, resolve_selection, signer_is_available,
        },
        user::get_user_details,
    },
};
use nostr::prelude::{PublicKey, ToBech32};
use serde::Serialize;

use crate::{
    client::{Client, Connect, finish_fetch_progress},
    git::{Repo, RepoActions},
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// use local cache only, skip network fetch
    #[arg(long, action)]
    pub offline: bool,
}

#[derive(Debug, Serialize)]
struct WhoamiJson {
    accounts: Vec<AccountJson>,
}

#[derive(Debug, Serialize)]
struct AccountJson {
    name: String,
    npub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    nip05: Option<String>,
    aliases: Vec<String>,
    scopes: Vec<String>,
    active: bool,
    selectors: Vec<SelectorJson>,
}

#[derive(Debug, Serialize)]
struct SelectorJson {
    #[serde(rename = "type")]
    kind: &'static str,
    value: String,
}

struct Account {
    name: String,
    npub: String,
    nip05: Option<String>,
    aliases: Vec<String>,
    scopes: Vec<String>,
    active: bool,
    selectors: Vec<SelectorJson>,
}

struct LoginScopes {
    local: Option<String>,
    global: Option<String>,
    system: Option<String>,
}

impl LoginScopes {
    fn active(&self) -> Option<&str> {
        self.local
            .as_deref()
            .or(self.global.as_deref())
            .or(self.system.as_deref())
    }

    fn labels_for(&self, npub: &str) -> Vec<String> {
        [
            ("local", self.local.as_deref()),
            ("global", self.global.as_deref()),
            ("system", self.system.as_deref()),
        ]
        .into_iter()
        .filter(|(_, configured)| *configured == Some(npub))
        .map(|(label, _)| label.to_string())
        .collect()
    }
}

pub async fn launch(command_args: &SubCommandArgs, json: bool) -> Result<()> {
    let git_repo = Repo::discover()
        .context("failed to find a git repository")
        .ok();
    let git_repo_ref = git_repo.as_ref();

    let scopes = LoginScopes {
        local: configured_signer_npub(&git_repo_ref, SignerInfoSource::GitLocal).await?,
        global: if std::env::var("NGITTEST").is_err() {
            configured_signer_npub(&git_repo_ref, SignerInfoSource::GitGlobal).await?
        } else {
            None
        },
        system: if std::env::var("NGITTEST").is_err() {
            configured_signer_npub(&git_repo_ref, SignerInfoSource::GitSystem).await?
        } else {
            None
        },
    };

    let credential_inventory = credential_store::inventory()?;
    let mut candidate_npubs = credential_inventory.accounts;
    candidate_npubs.extend(scopes.local.iter().cloned());
    candidate_npubs.extend(scopes.global.iter().cloned());
    candidate_npubs.extend(scopes.system.iter().cloned());

    // Resolve each known alias through the ordinary selector path. This both
    // applies normal OS/file/local/global/system precedence and prevents a
    // shadowed lower-priority mapping from being advertised for the wrong
    // account.
    let mut aliases_by_npub: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for alias in configured_alias_names(&git_repo_ref)? {
        let Ok(resolved) = resolve_selection(&git_repo_ref, &alias, &None, false).await else {
            continue;
        };
        candidate_npubs.insert(resolved.npub.clone());
        aliases_by_npub
            .entry(resolved.npub.clone())
            .or_default()
            .insert(alias.clone());
    }

    let mut available_npubs = Vec::new();
    for npub in candidate_npubs {
        if signer_is_available(&git_repo_ref, &npub)? {
            available_npubs.push(npub);
        }
    }

    let mut accounts = load_accounts(
        command_args,
        git_repo_ref,
        available_npubs,
        aliases_by_npub,
        &scopes,
    )
    .await?;

    add_selectors(&mut accounts, git_repo_ref).await;
    accounts.sort_by(|a, b| {
        (
            !a.active,
            !a.scopes.iter().any(|scope| scope == "local"),
            &a.name,
            &a.npub,
        )
            .cmp(&(
                !b.active,
                !b.scopes.iter().any(|scope| scope == "local"),
                &b.name,
                &b.npub,
            ))
    });

    if json {
        crate::output::set(WhoamiJson {
            accounts: accounts.into_iter().map(Account::into_json).collect(),
        })?;
    } else {
        print_human(&accounts);
    }
    Ok(())
}

async fn load_accounts(
    command_args: &SubCommandArgs,
    git_repo: Option<&Repo>,
    available_npubs: Vec<String>,
    mut aliases_by_npub: BTreeMap<String, BTreeSet<String>>,
    scopes: &LoginScopes,
) -> Result<Vec<Account>> {
    let client = if command_args.offline {
        None
    } else {
        Some(Client::new(Params::with_git_config_relay_defaults(
            &git_repo,
        )))
    };
    let git_repo_path = git_repo.and_then(|repo| repo.get_path().ok());
    if let Some(client) = client.as_ref() {
        let public_keys = available_npubs
            .iter()
            .map(|npub| PublicKey::parse(npub))
            .collect::<std::result::Result<HashSet<_>, _>>()
            .context("account inventory contains an invalid npub")?;
        if !public_keys.is_empty() {
            if let Ok((reports, progress_reporter)) = client
                .fetch_all(git_repo_path, None, &public_keys, &HashSet::new(), false)
                .await
            {
                finish_fetch_progress(&reports, progress_reporter)?;
            }
        }
    }
    let mut accounts = Vec::with_capacity(available_npubs.len());
    for npub in available_npubs {
        let public_key =
            PublicKey::parse(&npub).context("account inventory contains an invalid npub")?;
        // Profile refresh happens once for the whole inventory above. Cache
        // misses still produce an npub display fallback.
        let user = get_user_details(&public_key, None, git_repo_path, true, false).await?;
        let canonical_npub = user.public_key.to_bech32()?;
        accounts.push(Account {
            name: user.metadata.name,
            npub: canonical_npub.clone(),
            nip05: user.metadata.nip05,
            aliases: aliases_by_npub
                .remove(&canonical_npub)
                .unwrap_or_default()
                .into_iter()
                .collect(),
            scopes: scopes.labels_for(&canonical_npub),
            active: scopes.active() == Some(canonical_npub.as_str()),
            selectors: Vec::new(),
        });
    }

    if let Some(client) = client {
        client.disconnect().await?;
    }
    Ok(accounts)
}

async fn add_selectors(accounts: &mut [Account], git_repo: Option<&Repo>) {
    for account in accounts {
        // Ask the selector itself before advertising a mutable profile name.
        // This applies NIP-01 profile ordering, ambiguity checks, alias
        // precedence, and credential validation exactly as the copied command
        // will.
        let profile_is_unique = account.name != account.npub
            && resolve_selection(&git_repo, &account.name, &None, true)
                .await
                .is_ok_and(|resolved| resolved.npub == account.npub);
        if profile_is_unique {
            account.selectors.push(selector("profile", &account.name));
        }
        for alias in &account.aliases {
            if !account
                .selectors
                .iter()
                .any(|selector| selector.value.eq_ignore_ascii_case(alias))
            {
                account.selectors.push(selector("alias", alias));
            }
        }
        account.selectors.push(selector("npub", &account.npub));
    }
}

fn selector(kind: &'static str, value: &str) -> SelectorJson {
    SelectorJson {
        kind,
        value: value.to_string(),
    }
}

fn print_human(accounts: &[Account]) {
    if accounts.is_empty() {
        println!("no accounts available");
        println!();
        println!("use `ngit account login` to log in");
        return;
    }

    println!("available accounts:");
    for (index, account) in accounts.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let mut badges = account.scopes.clone();
        if account.active {
            badges.push("active".to_string());
        }
        if badges.is_empty() {
            println!("{}", account.name);
        } else {
            println!("{} [{}]", account.name, badges.join(", "));
        }
        println!("  npub: {}", account.npub);
        if let Some(nip05) = &account.nip05 {
            println!("  nip05: {nip05}");
        }
        if account.aliases.is_empty() {
            println!("  aliases: none");
        } else {
            println!("  aliases: {}", account.aliases.join(", "));
        }
        if account.name != account.npub
            && !account
                .selectors
                .iter()
                .any(|selector| selector.kind == "profile")
        {
            println!("  account name is ambiguous; use the npub or an alias as ACCOUNT");
        }
    }

    println!();
    println!("ACCOUNT can be any full npub or listed alias above, or your exact Nostr");
    println!("profile name.");
    println!();
    println!("commands:");
    println!("  ngit --signer ACCOUNT <command>        one ngit command");
    println!("  git -c nostr.signer=ACCOUNT <command>  one Git command");
    println!("  ngit account login ACCOUNT             set global default");
    println!("  ngit account login --local ACCOUNT     set repository default");
    println!("  ngit account login ACCOUNT --alias ALIAS  add alias; set global default");
    print_logout_guidance(accounts);
}

fn print_logout_guidance(accounts: &[Account]) {
    if !accounts
        .iter()
        .any(|account| account.scopes.iter().any(|scope| scope == "local"))
    {
        return;
    }

    let fallback = accounts.iter().find_map(|account| {
        account
            .scopes
            .iter()
            .find(|scope| matches!(scope.as_str(), "global" | "system"))
            .map(|scope| (account.name.as_str(), scope.as_str()))
    });
    match fallback {
        Some((name, scope)) => {
            println!("  ngit account logout                    remove local default");
            println!("                                         activate {scope} {name}");
        }
        None => {
            println!("  ngit account logout                    remove local; no active account");
        }
    }
}

impl Account {
    fn into_json(self) -> AccountJson {
        AccountJson {
            name: self.name,
            npub: self.npub,
            nip05: self.nip05,
            aliases: self.aliases,
            scopes: self.scopes,
            active: self.active,
            selectors: self.selectors,
        }
    }
}
