use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use ngit::{
    git::{Repo, RepoActions},
    login::{credential_store, user},
};
use zeroize::Zeroizing;

use super::logout::SubCommandArgs;

const LOGIN_KEYS: [&str; 5] = [
    "nostr.nsec",
    "nostr.npub",
    "nostr.signer",
    "nostr.bunker-uri",
    "nostr.bunker-app-key",
];

struct ConfigFile {
    path: PathBuf,
    values: BTreeMap<String, Vec<Zeroizing<String>>>,
}

pub(super) async fn launch(args: &SubCommandArgs, repo: Option<&Repo>) -> Result<()> {
    reject_environment_login()?;
    let configs = config_files(repo)?;
    let mut entries = credential_store::all_entries()?;
    let mut remote_keys = BTreeSet::new();
    let mut plaintext_keys = Vec::new();
    for config in &configs {
        if let Some(values) = config.values.get("nostr.bunker-app-key") {
            for value in values {
                if credential_store::parse_pointer(value).is_some() {
                    remote_keys.insert(value.to_string());
                }
            }
        }
        if let Some(values) = config.values.get("nostr.nsec") {
            for value in values {
                if credential_store::parse_pointer(value).is_some() {
                    entries.push(value.to_string());
                } else if !value.is_empty() {
                    plaintext_keys.push(value.as_str());
                }
            }
        }
    }
    entries.extend(remote_keys.iter().cloned());
    entries.sort();
    entries.dedup();
    let has_identity_keys = !plaintext_keys.is_empty()
        || entries.iter().any(|entry| {
            credential_store::parse_pointer(entry).is_some() && !remote_keys.contains(entry)
        });
    if has_identity_keys && !args.forget && !args.keep_credentials {
        bail!(
            "identity private keys would be removed; nothing changed. Use `ngit account logout --all --forget` to delete them, or --keep-credentials to retain saved credentials"
        );
    }
    // Validate every plaintext key before writing anything. Encrypted keys
    // cannot be migrated without decrypting them; do not discard them silently.
    let preserved_keys = if args.keep_credentials {
        plaintext_keys.iter().map(|value| nostr::prelude::Keys::parse(value)
            .context("cannot retain an encrypted or invalid Git-config key automatically; export it first, then use logout --all --forget"))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    for key in preserved_keys {
        credential_store::store(&key, credential_store::SecretStorage::Auto)?;
    }
    if !args.keep_credentials {
        credential_store::forget_entries(&entries)?;
    }
    for config in &configs {
        for key in config.values.keys().filter(|key| is_login_key(key)) {
            let status = Command::new("git")
                .args(["config", "--file"])
                .arg(&config.path)
                .args(["--no-includes", "--unset-all", key])
                .output()
                .context("failed to launch Git to clear login configuration")?;
            if !status.status.success() && status.status.code() != Some(5) {
                bail!(
                    "logout is incomplete: failed to remove {key} from {}; check file permissions and retry logout --all",
                    config.path.display()
                );
            }
        }
    }
    // Verify the on-disk result, including conditional includes currently
    // inactive in this repository. Never advertise a partial sweep as success.
    if config_files(repo)?
        .iter()
        .any(|file| file.values.keys().any(|key| is_login_key(key)))
    {
        bail!("logout is incomplete: login configuration remains; retry logout --all");
    }
    user::wipe_private_git_relay_list_cache()
        .context("logins cleared, but private relay cache cleanup failed")?;
    eprintln!("cleared all login selections in this repository and non-local Git configuration");
    eprintln!(
        "other repositories were not searched; their local credentials and selections may remain"
    );
    if args.keep_credentials {
        eprintln!("saved credentials retained; inspect them with `ngit account whoami --offline`");
    } else {
        eprintln!(
            "removed all known saved ngit credentials from the credential stores; remote permissions are unchanged"
        );
    }
    super::logout::list_saved_profiles().await
}

fn is_login_key(key: &str) -> bool {
    LOGIN_KEYS.contains(&key) || key.starts_with("nostr.signer-alias.")
}

fn reject_environment_login() -> Result<()> {
    let count = std::env::var("GIT_CONFIG_COUNT")
        .unwrap_or_default()
        .parse::<usize>()
        .unwrap_or(0);
    if (0..count).any(|index| {
        std::env::var(format!("GIT_CONFIG_KEY_{index}"))
            .is_ok_and(|key| is_login_key(&key.to_ascii_lowercase()))
    }) {
        bail!(
            "login is supplied through GIT_CONFIG_COUNT; remove the environment override before logout --all"
        );
    }
    if std::env::var("GIT_CONFIG_PARAMETERS")
        .is_ok_and(|value| value.to_ascii_lowercase().contains("nostr."))
    {
        bail!(
            "Nostr configuration is supplied through GIT_CONFIG_PARAMETERS; remove the environment override before logout --all"
        );
    }
    Ok(())
}

fn config_files(repo: Option<&Repo>) -> Result<Vec<ConfigFile>> {
    let mut pending = Vec::new();
    if let Some(path) = std::env::var_os("GIT_CONFIG_GLOBAL").filter(|value| !value.is_empty()) {
        pending.push(PathBuf::from(path));
    } else {
        for path in [git2::Config::find_global(), git2::Config::find_xdg()] {
            add_config_path(&mut pending, path)?;
        }
    }
    let no_system = std::env::var("GIT_CONFIG_NOSYSTEM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(no_system.as_str(), "1" | "true" | "yes" | "on") {
        if let Some(path) = std::env::var_os("GIT_CONFIG_SYSTEM").filter(|value| !value.is_empty())
        {
            pending.push(PathBuf::from(path));
        } else {
            add_config_path(&mut pending, git2::Config::find_system())?;
        }
    }
    if let Some(repo) = repo {
        let git = git2::Repository::open(repo.get_path()?)?;
        pending.push(git.commondir().join("config"));
        pending.push(git.path().join("config.worktree"));
    }
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        if !path.try_exists()? {
            continue;
        }
        let path = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        if !seen.insert(path.clone()) {
            continue;
        }
        let values = read_config(&path)?;
        for (key, values) in &values {
            if key == "include.path"
                || (key.starts_with("includeif.") && key.rsplit('.').next() == Some("path"))
            {
                for value in values {
                    pending.push(include_path(&path, value)?);
                }
            }
        }
        files.push(ConfigFile { path, values });
    }
    Ok(files)
}

fn add_config_path(
    paths: &mut Vec<PathBuf>,
    result: std::result::Result<PathBuf, git2::Error>,
) -> Result<()> {
    match result {
        Ok(path) => paths.push(path),
        Err(error) if error.code() == git2::ErrorCode::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn include_path(source: &Path, value: &str) -> Result<PathBuf> {
    if let Some(relative) = value.strip_prefix("~/") {
        return Ok(directories::BaseDirs::new()
            .context("cannot resolve home directory for Git include")?
            .home_dir()
            .join(relative));
    }
    if value.starts_with('~') || value.starts_with("%(") {
        bail!(
            "cannot resolve Git include path in {}; expand it before logout --all",
            source.display()
        );
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        source
            .parent()
            .context("Git config has no parent directory")?
            .join(path)
    })
}

fn read_config(path: &Path) -> Result<BTreeMap<String, Vec<Zeroizing<String>>>> {
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(path)
        .args(["--no-includes", "--null", "--list"])
        .output()
        .context("failed to launch Git to inspect login configuration")?;
    if !output.status.success() {
        bail!("cannot read Git config {}; nothing cleared", path.display());
    }
    let contents =
        Zeroizing::new(String::from_utf8(output.stdout).context("Git configuration is not UTF-8")?);
    let mut values: BTreeMap<String, Vec<Zeroizing<String>>> = BTreeMap::new();
    for item in contents.split('\0').filter(|item| !item.is_empty()) {
        let (key, value) = item.split_once('\n').unwrap_or((item, ""));
        values
            .entry(key.to_string())
            .or_default()
            .push(Zeroizing::new(value.to_string()));
    }
    Ok(values)
}
