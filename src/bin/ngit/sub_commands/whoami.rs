use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result};
use ngit::{
    client::Params,
    login::{
        SignerInfo, SignerInfoSource, credential_store,
        existing::{
            ResolvedSelection, configured_alias_names, configured_signer_inventory,
            resolve_selection_with_probe, resolve_selector_npub, signer_is_available,
        },
        user::get_user_details,
    },
};
use nostr::prelude::{Keys, PublicKey, ToBech32};
use serde::Serialize;

use crate::{
    client::{Client, Connect, finish_fetch_progress},
    git::{Repo, RepoActions},
};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// Use local cache only, skip network fetch
    #[arg(long, action)]
    pub offline: bool,
}

#[derive(Debug, Serialize)]
struct WhoamiJson {
    socket: ngit::signer_socket::SocketStatus,
    accounts: Vec<AccountJson>,
    unavailable_accounts: Vec<UnavailableAccountJson>,
    unavailable_profiles: Vec<UnavailableProfileJson>,
    unavailable_logins: Vec<UnavailableLoginJson>,
}

#[derive(Debug, Serialize)]
struct UnavailableLoginJson {
    scope: &'static str,
    error: String,
}

#[derive(Debug, Serialize)]
struct UnavailableAccountJson {
    npub: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct UnavailableProfileJson {
    profile: String,
    npub: String,
    #[serde(rename = "type")]
    kind: &'static str,
    error: String,
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
    signers: Vec<SignerJson>,
}

#[derive(Debug, Serialize)]
struct SelectorJson {
    #[serde(rename = "type")]
    kind: &'static str,
    value: String,
}

#[derive(Debug, Serialize)]
struct SignerJson {
    #[serde(rename = "type")]
    kind: &'static str,
    npub_default: bool,
    aliases: Vec<String>,
    scopes: Vec<String>,
    active: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum SignerKey {
    Socket,
    Local,
    Remote {
        bunker_uri: String,
        app_public_key: PublicKey,
    },
}

struct SignerConnection {
    key: SignerKey,
    kind: &'static str,
    npub_default: bool,
    aliases: BTreeSet<String>,
    scopes: Vec<String>,
    active: bool,
}

struct Account {
    name: String,
    npub: String,
    nip05: Option<String>,
    aliases: Vec<String>,
    scopes: Vec<String>,
    active: bool,
    selectors: Vec<SelectorJson>,
    signers: Vec<SignerConnection>,
}

#[derive(Default)]
struct LoginScopes {
    resolved: BTreeMap<&'static str, ResolvedSelection>,
    unavailable: Vec<UnavailableLoginJson>,
}

impl LoginScopes {
    fn active(&self) -> Option<&str> {
        self.npub(self.active_label()?)
    }

    fn npub(&self, scope: &str) -> Option<&str> {
        self.resolved
            .get(scope)
            .map(|selected| selected.npub.as_str())
    }

    fn active_label(&self) -> Option<&'static str> {
        for label in ["local", "global", "system"] {
            if self.unavailable.iter().any(|login| login.scope == label) {
                return None;
            }
            if self.resolved.contains_key(label) {
                return Some(label);
            }
        }
        None
    }

    async fn read(
        &mut self,
        repo: Option<&Repo>,
        source: SignerInfoSource,
        label: &'static str,
        probe: &mut ngit::signer_socket::SocketProbe,
    ) {
        match configured_signer_inventory(&repo, source, probe).await {
            Ok(Some(resolved)) => {
                self.resolved.insert(label, resolved);
            }
            Ok(None) => {}
            Err(error) => {
                self.unavailable.push(UnavailableLoginJson {
                    scope: label,
                    error: format!("{error:#}"),
                });
            }
        }
    }

    fn labels_for(&self, npub: &str) -> Vec<String> {
        ["local", "global", "system"]
            .into_iter()
            .filter(|label| self.npub(label) == Some(npub))
            .map(str::to_string)
            .collect()
    }
}

struct Inventory {
    unavailable_profiles: Vec<UnavailableProfileJson>,
    probe: ngit::signer_socket::SocketProbe,
    selections: BTreeMap<(String, bool), std::result::Result<ResolvedSelection, String>>,
}

impl Inventory {
    fn new() -> Self {
        Self {
            probe: ngit::signer_socket::SocketProbe::inventory(),
            selections: BTreeMap::new(),
            unavailable_profiles: Vec::new(),
        }
    }

    async fn select(
        &mut self,
        repo: Option<&Repo>,
        selector: &str,
        allow_name: bool,
    ) -> Result<ResolvedSelection> {
        let key = (selector.to_string(), allow_name);
        if !self.selections.contains_key(&key) {
            let selected =
                resolve_selection_with_probe(&repo, selector, &None, allow_name, &mut self.probe)
                    .await
                    .map_err(|error| format!("{error:#}"));
            self.selections.insert(key.clone(), selected);
        }
        self.selections[&key].clone().map_err(anyhow::Error::msg)
    }

    async fn aliases(
        &mut self,
        repo: Option<&Repo>,
        candidate_npubs: &mut BTreeSet<String>,
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for alias in configured_alias_names(&repo)? {
            match self.select(repo, &alias, false).await {
                Ok(resolved) => {
                    candidate_npubs.insert(resolved.npub.clone());
                    aliases.entry(resolved.npub).or_default().insert(alias);
                }
                Err(error) => {
                    // A failed probe must not hide a saved public socket binding.
                    if let Ok(target) = resolve_selector_npub(&repo, &alias) {
                        if target.credential.as_deref() != Some(credential_store::SOCKET_BINDING) {
                            continue;
                        }
                        candidate_npubs.insert(target.npub.clone());
                        self.unavailable_profiles.push(UnavailableProfileJson {
                            profile: alias,
                            npub: target.npub,
                            kind: "socket",
                            error: format!("{error:#}"),
                        });
                    }
                }
            }
        }
        Ok(aliases)
    }

    async fn available(&mut self, repo: Option<&Repo>, npub: &str) -> Result<bool> {
        match signer_is_available(&repo, npub) {
            Ok(true) => Ok(true),
            stored => match self.select(repo, npub, false).await {
                Ok(_) => Ok(true),
                Err(error) if ngit::signer_socket::socket_path(&repo)?.is_some() => {
                    stored.and(Err(error))
                }
                Err(_) => stored,
            },
        }
    }
}

pub async fn launch(command_args: &SubCommandArgs, json: bool) -> Result<()> {
    let git_repo = Repo::discover().ok();
    let git_repo_ref = git_repo.as_ref();

    let mut inventory = Inventory::new();
    let scopes = read_login_scopes(git_repo_ref, &mut inventory.probe).await;
    let socket =
        ngit::signer_socket::status_with_probe(&git_repo_ref, &mut inventory.probe).await?;

    let mut candidate_npubs = credential_store::inventory()?.accounts;
    let selected_npubs = scopes
        .resolved
        .values()
        .map(|selected| selected.npub.clone());
    candidate_npubs.extend(selected_npubs);

    let aliases_by_npub = inventory
        .aliases(git_repo_ref, &mut candidate_npubs)
        .await?;

    let mut available_npubs = Vec::new();
    let mut unavailable_accounts = Vec::new();
    for npub in candidate_npubs {
        let availability = if aliases_by_npub.contains_key(&npub)
            || (socket.active && socket.npub.as_deref() == Some(npub.as_str()))
        {
            Ok(true)
        } else {
            inventory.available(git_repo_ref, &npub).await
        };
        let error = match availability {
            Ok(true) => {
                available_npubs.push(npub);
                continue;
            }
            Ok(false) => "no stored signer credential found".to_string(),
            Err(error) => format!("{error:#}"),
        };
        unavailable_accounts.push(UnavailableAccountJson { npub, error });
    }

    let mut accounts = load_accounts(
        command_args,
        git_repo_ref,
        available_npubs,
        aliases_by_npub,
        &scopes,
    )
    .await?;

    add_signers(&mut accounts, git_repo_ref, &scopes, &mut inventory).await?;
    add_selectors(&mut accounts, git_repo_ref, &mut inventory).await;
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

    if socket.selected {
        for account in &mut accounts {
            account.active = socket.active && socket.npub.as_deref() == Some(account.npub.as_str());
            for signer in &mut account.signers {
                signer.active = account.active && signer.kind == "socket";
            }
        }
    }
    if json {
        crate::output::set(WhoamiJson {
            socket,
            accounts: accounts.into_iter().map(Account::into_json).collect(),
            unavailable_accounts,
            unavailable_profiles: inventory.unavailable_profiles,
            unavailable_logins: scopes.unavailable,
        })?;
    } else {
        print_human(&accounts, &scopes, &socket);
        for profile in &inventory.unavailable_profiles {
            eprintln!(
                "warning: profile {} ({}; {}) is unavailable: {}",
                profile.profile, profile.kind, profile.npub, profile.error
            );
        }
        for login in scopes.unavailable {
            eprintln!(
                "warning: {} login is unavailable: {}",
                login.scope, login.error
            );
        }
        for account in unavailable_accounts {
            eprintln!(
                "warning: inventory account {} is unavailable: {}",
                account.npub, account.error
            );
        }
    }
    Ok(())
}

async fn read_login_scopes(
    git_repo_ref: Option<&Repo>,
    probe: &mut ngit::signer_socket::SocketProbe,
) -> LoginScopes {
    let mut scopes = LoginScopes::default();
    scopes
        .read(git_repo_ref, SignerInfoSource::GitLocal, "local", probe)
        .await;
    if std::env::var("NGITTEST").is_err() {
        scopes
            .read(git_repo_ref, SignerInfoSource::GitGlobal, "global", probe)
            .await;
        scopes
            .read(git_repo_ref, SignerInfoSource::GitSystem, "system", probe)
            .await;
    }

    scopes
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
            signers: Vec::new(),
        });
    }

    if let Some(client) = client {
        client.disconnect().await?;
    }
    Ok(accounts)
}

async fn add_signers(
    accounts: &mut [Account],
    git_repo: Option<&Repo>,
    scopes: &LoginScopes,
    inventory: &mut Inventory,
) -> Result<()> {
    for account in accounts {
        for alias in account.aliases.clone() {
            if let Ok(resolved) = inventory.select(git_repo, &alias, false).await {
                account.add_signer(&resolved.signer_info, Some(&alias), false, None, false)?;
            }
        }

        if let Ok(resolved) = inventory.select(git_repo, &account.npub, false).await {
            account.add_signer(&resolved.signer_info, None, true, None, false)?;
        }

        for (label, selected) in &scopes.resolved {
            if selected.npub == account.npub {
                account.add_signer(
                    &selected.signer_info,
                    selected.alias.as_deref(),
                    false,
                    Some(label),
                    scopes.active_label() == Some(*label),
                )?;
            }
        }

        account.signers.sort_by(|a, b| {
            (
                !a.active,
                a.scopes.is_empty(),
                !a.npub_default,
                a.kind,
                &a.aliases,
            )
                .cmp(&(
                    !b.active,
                    b.scopes.is_empty(),
                    !b.npub_default,
                    b.kind,
                    &b.aliases,
                ))
        });
    }
    Ok(())
}

async fn add_selectors(
    accounts: &mut [Account],
    git_repo: Option<&Repo>,
    inventory: &mut Inventory,
) {
    for account in accounts {
        // Ask the selector itself before advertising a mutable profile name.
        // This applies NIP-01 profile ordering, ambiguity checks, alias
        // precedence, and credential validation exactly as the copied command
        // will.
        let profile_is_unique = account.name != account.npub
            && inventory
                .select(git_repo, &account.name, true)
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
        if account.signers.iter().any(|signer| signer.npub_default) {
            account.selectors.push(selector("npub", &account.npub));
        }
    }
}

fn selector(kind: &'static str, value: &str) -> SelectorJson {
    SelectorJson {
        kind,
        value: value.to_string(),
    }
}

fn default_account<'a>(accounts: &'a [Account], scopes: &LoginScopes) -> Option<&'a Account> {
    // An unavailable global selection blocks system fallback, just as signing does.
    if scopes
        .unavailable
        .iter()
        .any(|login| login.scope == "global")
    {
        return None;
    }
    let npub = scopes.npub("global").or(scopes.npub("system"))?;
    accounts.iter().find(|account| account.npub == npub)
}

fn scope_label(scope: &str) -> &str {
    match scope {
        "local" => "repository override",
        "global" => "default",
        "system" => "system default",
        other => other,
    }
}

fn print_active_account(accounts: &[Account], scopes: &LoginScopes) {
    if let Some(account) = accounts.iter().find(|account| account.active) {
        if scopes.active_label() == Some("local") {
            println!("This repository: {} (repository override)", account.name);
        } else {
            println!(
                "Logged in as {} ({})",
                account.name,
                scope_label(scopes.active_label().unwrap_or("global"))
            );
        }
    } else if let Some(login) = scopes.unavailable.first() {
        println!("{} login is unavailable.", scope_label(login.scope));
    } else if scopes.active().is_some() {
        println!("The selected account is unavailable.");
    } else {
        println!("Not logged in.");
    }
    if scopes.npub("local").is_some()
        || scopes
            .unavailable
            .iter()
            .any(|login| login.scope == "local")
    {
        if let Some(default) = default_account(accounts, scopes) {
            println!("Default elsewhere: {}", default.name);
        }
    }
    println!();
}

fn print_human(
    accounts: &[Account],
    scopes: &LoginScopes,
    socket: &ngit::signer_socket::SocketStatus,
) {
    if !socket.active && !socket.selected {
        print_active_account(accounts, scopes);
    }
    if socket.active || socket.selected || socket.state == "available" || socket.error.is_some() {
        println!("{}", socket.message());
    }
    if accounts.is_empty() {
        if socket.active {
            println!(
                "No usable saved accounts; the socket account is managed by your signer application."
            );
        } else {
            println!("No usable saved accounts. Log in: ngit account login");
        }
        return;
    }

    println!("Saved accounts:");
    for (index, account) in accounts.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let status = if account.active {
            "active here"
        } else {
            "not active here"
        };
        println!("{} [{status}]", account.name);
        println!("  npub: {}", account.npub);
        if let Some(nip05) = &account.nip05 {
            println!("  nip05: {nip05}");
        }
        for signer in &account.signers {
            let mut details: Vec<String> = signer
                .scopes
                .iter()
                .map(|scope| scope_label(scope).to_string())
                .collect();
            if !signer.aliases.is_empty() {
                details.push(format!(
                    "profiles: {}",
                    signer
                        .aliases
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if signer.active {
                details.push("active here".to_string());
            }
            let suffix = if details.is_empty() {
                String::new()
            } else {
                format!(" · {}", details.join(" · "))
            };
            println!(
                "  {}{suffix}",
                match signer.kind {
                    "local-key" => "identity key",
                    "socket" => "local signer socket",
                    _ => "remote signer",
                }
            );
        }
        if account.name != account.npub
            && !account
                .selectors
                .iter()
                .any(|selector| selector.kind == "profile")
        {
            println!(
                "  Display name cannot select this account; use a listed profile or selectable npub."
            );
        }
        if !account.signers.iter().any(|signer| signer.npub_default) {
            println!("  To log in, select a named profile; this npub has no default signer.");
        }
    }

    println!();
    println!("Switch default:       ngit account login ACCOUNT");
    println!("Use in this repo:     ngit account login --local ACCOUNT");
    println!("Use once:             ngit --signer ACCOUNT <command>");
    println!("Log out:              ngit account logout");
    println!();
    println!("ACCOUNT: a listed profile name, selectable npub, or unambiguous display name.");
    print_logout_guidance(accounts, scopes);
    println!();
    println!("Remove one profile:   ngit account logout --profile NAME");
    println!("Remove entire account: ngit account logout NPUB");
    println!(
        "Identity-key deletion requires --forget; use --keep-credentials to retain credentials."
    );
    println!("More options:         ngit account --help");
}

impl Account {
    fn add_signer(
        &mut self,
        signer_info: &SignerInfo,
        alias: Option<&str>,
        npub_default: bool,
        scope: Option<&str>,
        active: bool,
    ) -> Result<()> {
        let (key, kind) = match signer_info {
            SignerInfo::Socket { .. } => (SignerKey::Socket, "socket"),
            SignerInfo::Nsec { .. } => (SignerKey::Local, "local-key"),
            SignerInfo::Bunker {
                bunker_uri,
                bunker_app_key,
                ..
            } => (
                SignerKey::Remote {
                    bunker_uri: bunker_uri.clone(),
                    app_public_key: Keys::parse(bunker_app_key)
                        .context("configured remote signer has an invalid app key")?
                        .public_key(),
                },
                "remote-signer",
            ),
            SignerInfo::Selection { .. } => {
                anyhow::bail!("internal error: unresolved account signer selection")
            }
        };
        let index = self
            .signers
            .iter()
            .position(|connection| connection.key == key)
            .unwrap_or_else(|| {
                self.signers.push(SignerConnection {
                    key,
                    kind,
                    npub_default: false,
                    aliases: BTreeSet::new(),
                    scopes: Vec::new(),
                    active: false,
                });
                self.signers.len() - 1
            });
        let connection = &mut self.signers[index];
        connection.npub_default |= npub_default;
        if let Some(alias) = alias {
            connection.aliases.insert(alias.to_string());
        }
        if let Some(scope) = scope {
            if !connection.scopes.iter().any(|existing| existing == scope) {
                connection.scopes.push(scope.to_string());
            }
        }
        connection.active |= active;
        Ok(())
    }
}

fn print_logout_guidance(accounts: &[Account], scopes: &LoginScopes) {
    if scopes.npub("local").is_none()
        && !scopes
            .unavailable
            .iter()
            .any(|login| login.scope == "local")
    {
        return;
    }
    println!("Logout removes this repository's login and its credentials.");
    if let Some(default) = default_account(accounts, scopes) {
        println!(
            "The remaining default account ({}) becomes active here, if available.",
            default.name
        );
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
            signers: self
                .signers
                .into_iter()
                .map(|signer| SignerJson {
                    kind: signer.kind,
                    npub_default: signer.npub_default,
                    aliases: signer.aliases.into_iter().collect(),
                    scopes: signer.scopes,
                    active: signer.active,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    fn account(npub: &str) -> Account {
        Account {
            name: npub.to_string(),
            npub: npub.to_string(),
            nip05: None,
            aliases: vec![],
            scopes: vec![],
            active: false,
            selectors: vec![],
            signers: vec![],
        }
    }

    #[test]
    fn default_guidance_respects_global_precedence_and_unavailable_selections() {
        // Inventory order must not choose the system account ahead of global.
        let accounts = vec![account("system"), account("global")];
        let mut scopes = LoginScopes::default();
        for scope in ["local", "global", "system"] {
            scopes.resolved.insert(
                scope,
                ResolvedSelection {
                    signer_info: SignerInfo::Selection {
                        selector: scope.to_string(),
                    },
                    npub: scope.to_string(),
                    alias: None,
                },
            );
        }
        assert_eq!(
            default_account(&accounts, &scopes).map(|a| a.npub.as_str()),
            Some("global")
        );
        scopes.resolved.remove("global");
        scopes.unavailable.push(UnavailableLoginJson {
            scope: "global",
            error: "missing credential".to_string(),
        });
        assert!(default_account(&accounts, &scopes).is_none());
        scopes.unavailable.clear();
        assert_eq!(
            default_account(&accounts, &scopes).map(|a| a.npub.as_str()),
            Some("system")
        );
    }
}
