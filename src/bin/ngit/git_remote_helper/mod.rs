//! Implementation of the `git-remote-nostr` remote helper.
//!
//! Git discovers remote helpers by executable name, so a separate
//! `git-remote-nostr` executable still exists — but it is a thin
//! launcher (`src/bin/git_remote_nostr.rs`) that re-invokes `ngit`
//! with the hidden [`INTERNAL_COMMAND`] entry point handled here.
#![allow(clippy::module_name_repetitions)]

use core::str;
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use client::{
    Connect, FetchReport, PrivateRelayProbeDecision, consolidate_fetch_outcome,
    get_repo_ref_from_cache, is_verbose, private_relay_probe_decision,
    save_repository_privacy_to_git_config, warn_if_invited_as_maintainer,
};
use git::{RepoActions, nostr_url::NostrUrlDecoded};
use ngit::{
    client::{self, Client, Params},
    git::{self, Repo, utils::set_git_timeout},
    git_http_auth::{clear_private_git_auth, prepare_private_git_auth},
    login::{
        SignerInfo,
        existing::load_existing_login,
        user::{
            PrivateGitRelayDiscovery, discover_private_git_relay_list,
            refresh_user_and_private_git_relays,
        },
    },
    relay_information::discover_private_repository_relays,
    signer::NgitSigner,
    utils::read_line,
};
use nostr::nips::nip19::Nip19Coordinate;

/// Hidden entry point through which the `git-remote-nostr` launcher
/// re-invokes `ngit`. Deliberately absent from the clap CLI so it
/// never appears in user-facing help.
pub const INTERNAL_COMMAND: &str = "__git-remote-nostr";

/// Read a config value supplied by the invoking Git command (for example,
/// `git -c nostr.signer=alice push`). Git passes `-c` values to remote helpers
/// through its command-config environment; asking Git to decode that internal
/// representation avoids duplicating Git's quoting rules here.
fn command_config_value(key: &str) -> Result<Option<String>> {
    if std::env::var_os("GIT_CONFIG_PARAMETERS").is_none()
        && std::env::var_os("GIT_CONFIG_COUNT").is_none()
    {
        return Ok(None);
    }

    command_config_value_from(&mut Command::new("git"), key)
}

fn command_config_value_from(command: &mut Command, key: &str) -> Result<Option<String>> {
    let output = command
        .args(["config", "--null", "--show-scope", "--get", key])
        .output()
        .context("failed to read Git command-scoped config")?;

    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        bail!(
            "failed to read Git command-scoped config: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut fields = output.stdout.split(|byte| *byte == 0);
    let scope = fields.next().unwrap_or_default();
    let value = fields.next().unwrap_or_default();
    if scope != b"command" {
        return Ok(None);
    }

    Ok(Some(
        std::str::from_utf8(value)
            .context("Git command-scoped config is not valid UTF-8")?
            .to_string(),
    ))
}

#[derive(Default, Clone)]
struct PushOptions {
    title: Option<String>,
    description: Option<String>,
    git_server: Option<String>,
    git_server_extras: Vec<String>,
    force_with_lease: HashMap<String, Option<String>>,
    proposal: ProposalOptions,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ProposalOptions {
    pub target_branch: Option<String>,
    pub base: Option<String>,
}

fn parse_cas_option(value: &str) -> Result<(String, Option<String>)> {
    let (ref_name, expected) = value
        .split_once(':')
        .context("force-with-lease value must be <ref>:<expected-oid>")?;
    if !ref_name.starts_with("refs/") {
        bail!("force-with-lease ref must start with refs/");
    }
    if !expected.is_empty()
        && (expected.len() != 40 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("force-with-lease expected object ID must be 40 hexadecimal characters");
    }
    let expected = if expected.is_empty() || expected.bytes().all(|byte| byte == b'0') {
        None
    } else {
        Some(expected.to_ascii_lowercase())
    };
    Ok((ref_name.to_string(), expected))
}

/// Strip git's c-style quoting from a push-option value.
///
/// When a push-option value contains special characters (like
/// backslashes), git wraps the entire `key=value` string in double
/// quotes and doubles every backslash. This function reverses that:
/// it strips the surrounding quotes and un-doubles backslashes.
///
/// If the string is not quoted, it is returned unchanged.
fn strip_git_quoting(s: &str) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        let inner = &s[1..s.len() - 1];
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next) = chars.peek() {
                    chars.next();
                    result.push(next);
                } else {
                    result.push(c);
                }
            } else {
                result.push(c);
            }
        }
        result
    } else {
        s.to_string()
    }
}

/// Decode escape sequences in push-option values.
///
/// Git push-options are transmitted one per line, so literal newlines
/// cannot appear in a value. To support multiline titles and
/// descriptions users can write the two-character sequence `\n` which
/// this function converts to a real newline. A literal backslash
/// before `n` can be preserved by doubling it (`\\n`).
///
/// # Examples
/// ```text
/// "first line\\nsecond line"  -> "first line\nsecond line"
/// "keep \\\\n literal"        -> "keep \\n literal"
/// "no escapes here"           -> "no escapes here"
/// ```
fn decode_push_option_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    result.push('\n');
                }
                Some('\\') => {
                    chars.next();
                    result.push('\\');
                }
                _ => result.push(c),
            }
        } else {
            result.push(c);
        }
    }
    result
}

impl PushOptions {
    fn validate(&self) -> Result<Option<(String, String)>> {
        match (&self.title, &self.description) {
            (Some(t), Some(d)) => Ok(Some((t.clone(), d.clone()))),
            (Some(_), None) => bail!(
                "error: 'title' push-option provided without 'description'. Both title and description are required together, or neither to use defaults."
            ),
            (None, Some(_)) => bail!(
                "error: 'description' push-option provided without 'title'. Both title and description are required together, or neither to use defaults."
            ),
            (None, None) => Ok(None),
        }
    }
}

fn apply_ngit_push_option(push_options: &mut PushOptions, key: &str, value: &str) -> bool {
    match key {
        "title" => push_options.title = Some(decode_push_option_escapes(value)),
        "description" => {
            push_options.description = Some(decode_push_option_escapes(value));
        }
        "git-server" => push_options.git_server = Some(value.to_string()),
        "target-branch" => push_options.proposal.target_branch = Some(value.to_string()),
        "base" => push_options.proposal.base = Some(value.to_string()),
        _ => return false,
    }
    true
}

mod fetch;
mod list;
pub(crate) mod push;

/// Run the remote-helper protocol. `args` are the arguments following
/// [`INTERNAL_COMMAND`], matching what git passes to a remote helper:
/// `<remote-name> <url>`, a single `<url>`, or `--version`.
///
/// Dispatched from `main` before any normal ngit startup output
/// (update notices, skill notices, clap output) — stray stdout would
/// corrupt git's remote-helper protocol.
#[allow(clippy::too_many_lines)]
pub async fn run(args: &[String]) -> Result<()> {
    // Capture this before any libgit2-backed operation. libgit2 does not read
    // the command-config environment that Git forwards to remote helpers.
    let command_signer =
        command_config_value("nostr.signer")?.map(|selector| SignerInfo::Selection { selector });

    if std::env::var("NGITTEST").is_ok() {
        std::env::set_var("NGIT_VERBOSE", "1");
    }

    let Some((remote_name, decoded_nostr_url, git_repo)) = process_args(args).await? else {
        return Ok(());
    };

    let git_repo_path = git_repo.get_path()?;
    // an explicit local classification answers the privacy question, so the
    // NIP-11 probes (up to their full timeout on every git operation) are
    // only paid when `nostr.private` is not set yet
    let configured_privacy = git_repo
        .git_repo
        .config()
        .ok()
        .and_then(|config| config.get_bool("nostr.private").ok());
    let nip11_private_relays = if configured_privacy.is_some() {
        vec![]
    } else {
        discover_private_repository_relays(&decoded_nostr_url.coordinate.relays).await
    };
    let repository_is_known_private =
        configured_privacy == Some(true) || !nip11_private_relays.is_empty();

    let _ = set_git_timeout(Some(&git_repo));
    let _ = ngit::version_check::print_update_notice_if_available(Some(git_repo_path)).await;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    client.nip42_register_private_repo_relays(nip11_private_relays.clone());

    let login = match load_existing_login(
        &Some(&git_repo),
        &command_signer,
        &None,
        &None,
        None,
        true,
        false,
        false,
    )
    .await
    {
        Ok((signer, cached_user_ref, _)) => {
            client.set_signer(signer.clone()).await;
            // A cached NIP-65 list lets the steady-state path refresh public
            // account data and kind 10318 in one REQ on each mailbox relay.
            // If setup of that combined fetch fails, retain the old direct
            // lookup as a degraded fallback.
            let private_discovery = if let Ok((_, private_discovery)) =
                refresh_user_and_private_git_relays(
                    &cached_user_ref.public_key,
                    &client,
                    Some(git_repo_path),
                    &signer,
                )
                .await
            {
                private_discovery
            } else {
                let mut discovery_relays = cached_user_ref.relays.read();
                for relay in cached_user_ref.relays.write() {
                    if !discovery_relays.contains(&relay) {
                        discovery_relays.push(relay);
                    }
                }
                if discovery_relays.is_empty() {
                    discovery_relays.extend(client.get_relay_default_set().iter().cloned());
                }
                discover_private_git_relay_list(&client, discovery_relays, &signer).await
            };
            Some((signer, private_discovery))
        }
        // an explicit `-c nostr.signer=` selection must fail closed rather
        // than silently degrading to anonymous relay access
        Err(error) if command_signer.is_some() => {
            return Err(
                error.context("failed to resolve the signer selected with `git -c nostr.signer`")
            );
        }
        Err(error) if repository_is_known_private => {
            return Err(error
                .context("private repository relay authentication requires a logged-in account"));
        }
        // without a selection, a missing login only means anonymous relay
        // access; a private repository will fail explicitly when it needs
        // authenticated relay or Git transport
        Err(_) => None,
    };
    let signer = login.as_ref().map(|(signer, _)| signer.clone());

    let mut discovery_coordinate = decoded_nostr_url.coordinate.clone();
    let mut private_discovery = login.as_ref().map_or(
        PrivateGitRelayDiscovery::Absent,
        |(_, private_discovery)| private_discovery.clone(),
    );
    if !nip11_private_relays.is_empty() {
        match &mut private_discovery {
            PrivateGitRelayDiscovery::Available(relays) => {
                for relay in nip11_private_relays {
                    if !relays.contains(&relay) {
                        relays.push(relay);
                    }
                }
            }
            PrivateGitRelayDiscovery::Absent | PrivateGitRelayDiscovery::Unavailable(_) => {
                private_discovery = PrivateGitRelayDiscovery::Available(nip11_private_relays);
            }
        }
    }

    let fetch_report = fetching_with_report_for_helper(
        git_repo_path,
        &client,
        &mut discovery_coordinate,
        &private_discovery,
    )
    .await?;

    let mut repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &discovery_coordinate).await?;
    // this is the repository the helper operates on, so its privacy
    // classification may be recorded in the local git config
    save_repository_privacy_to_git_config(git_repo_path, repo_ref.private);
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;
    let _ = ngit::agent_guidance::warn_if_maintainer(&git_repo, &repo_ref).await;

    repo_ref.set_nostr_git_url(decoded_nostr_url.clone());

    let stdin = io::stdin();
    let mut line = String::new();

    let mut list_outputs = None;
    let mut push_options: PushOptions = PushOptions::default();
    loop {
        let tokens = read_line(&stdin, &mut line)?;

        match tokens.as_slice() {
            ["capabilities"] => {
                println!("option");
                println!("push");
                println!("fetch");
                println!("push-options");
                println!();
            }
            ["option", "verbosity"] => {
                println!("ok");
            }
            ["option", "push-option", rest @ ..] => {
                let option = strip_git_quoting(&rest.join(" "));
                let handled_by_ngit = if let Some((key, value)) = option.split_once('=') {
                    apply_ngit_push_option(&mut push_options, key, value)
                } else {
                    false
                };
                if !handled_by_ngit {
                    push_options.git_server_extras.push(option);
                }
                println!("ok");
            }
            ["option", "cas", value] => match parse_cas_option(value) {
                Ok((ref_name, expected)) => {
                    push_options.force_with_lease.insert(ref_name, expected);
                    println!("ok");
                }
                Err(error) => println!("error {error}"),
            },
            ["option", ..] => {
                println!("unsupported");
            }
            ["fetch", oid, refstr] => {
                refresh_private_git_auth(&repo_ref, signer.as_ref()).await?;
                fetch::run_fetch(&git_repo, &repo_ref, &stdin, oid, refstr, signer.as_ref())
                    .await?;
            }
            ["push", refspec] => {
                refresh_private_git_auth(&repo_ref, signer.as_ref()).await?;
                let title_description = push_options.validate()?;
                push::run_push(
                    &git_repo,
                    &repo_ref,
                    &stdin,
                    refspec,
                    &mut client,
                    remote_name.as_deref(),
                    list_outputs.clone(),
                    title_description,
                    push_options.git_server_extras.clone(),
                    push_options.git_server.clone(),
                    push_options.proposal.clone(),
                    &push_options.force_with_lease,
                    signer.as_ref(),
                    command_signer.as_ref(),
                )
                .await?;
                push_options = PushOptions::default();
            }
            ["list"] => {
                refresh_private_git_auth(&repo_ref, signer.as_ref()).await?;
                list_outputs = Some(
                    list::run_list(&git_repo, &repo_ref, false, &fetch_report, signer.as_ref())
                        .await?,
                );
            }
            ["list", "for-push"] => {
                refresh_private_git_auth(&repo_ref, signer.as_ref()).await?;
                list_outputs = Some(
                    list::run_list(&git_repo, &repo_ref, true, &fetch_report, signer.as_ref())
                        .await?,
                );
            }
            [] => {
                return Ok(());
            }
            _ => {
                bail!(format!("unknown command: {}", line.trim().to_owned()));
            }
        }
    }
}

async fn refresh_private_git_auth(
    repo_ref: &ngit::repo_ref::RepoRef,
    signer: Option<&std::sync::Arc<NgitSigner>>,
) -> Result<()> {
    clear_private_git_auth();
    if repo_ref.private {
        prepare_private_git_auth(
            &repo_ref.git_server,
            signer.context("private repository Git access requires a logged-in account")?,
        )
        .await?;
    }
    Ok(())
}

async fn process_args(args: &[String]) -> Result<Option<(Option<String>, NostrUrlDecoded, Repo)>> {
    if args.first().map(String::as_str) == Some("--version") {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("v{VERSION}");
        return Ok(None);
    }

    let args = args.iter().take(2).collect::<Vec<_>>();

    let (remote_name, nostr_remote_url) = match args.as_slice() {
        [remote_name, nostr_remote_url] => (Some((*remote_name).clone()), *nostr_remote_url),
        [nostr_remote_url] => (None, *nostr_remote_url),
        _ => {
            println!("nostr plugin for git");
            println!("Usage:");
            println!(
                " - clone a nostr repository, or add as a remote, by using the url format nostr://npub123/identifier"
            );
            println!(
                " - remote branches beginning with `pr/` are open PRs from contributors; `ngit list` can be used to view all PRs"
            );
            println!(
                " - to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options"
            );
            println!(" - set PR title/description via push options:");
            println!("     git push -o 'title=My PR' -o 'description=Details' -u origin pr/branch");
            println!("   for multiline descriptions, use \\n:");
            println!(
                "     git push -o 'title=My PR' -o 'description=line1\\n\\nline2' -u origin pr/branch"
            );
            println!("- publish a repository to nostr with `ngit init`");
            return Ok(None);
        }
    };

    let git_repo = Repo::from_path(&PathBuf::from(
        std::env::var("GIT_DIR").context("git should set GIT_DIR when remote helper is called")?,
    ))?;

    let decoded_nostr_url = NostrUrlDecoded::parse_and_resolve(nostr_remote_url, &Some(&git_repo))
        .await
        .context("invalid nostr url")?;

    Ok(Some((remote_name, decoded_nostr_url, git_repo)))
}

async fn fetching_with_report_for_helper(
    git_repo_path: &Path,
    client: &Client,
    selected_maintainer_coordinate: &mut Nip19Coordinate,
    private_discovery: &PrivateGitRelayDiscovery,
) -> Result<FetchReport> {
    let term = console::Term::stderr();
    let verbose = is_verbose();
    if verbose {
        term.write_line("nostr: fetching...")?;
    }
    let cached_repo_ref =
        get_repo_ref_from_cache(Some(git_repo_path), selected_maintainer_coordinate)
            .await
            .ok();
    let private_discovery_unavailable =
        matches!(private_discovery, PrivateGitRelayDiscovery::Unavailable(_));
    if let Some(repo_ref) = cached_repo_ref
        .as_ref()
        .filter(|repo_ref| repo_ref.private || private_discovery_unavailable)
    {
        selected_maintainer_coordinate
            .relays
            .clone_from(&repo_ref.relays);
    }

    // `Unavailable` only arises when the kind-10318 state is unknown for a
    // reason other than a plain relay outage (a signer/decrypt failure, or no
    // discovery relay to ask): outages degrade to `Absent` upstream after the
    // on-disk relay-list cache is consulted, so public repositories keep
    // working through indexer discovery. What remains may hide a private
    // repository, so without a cached announcement discovery fails closed
    // rather than leak the coordinate to public discovery relays.
    if let PrivateGitRelayDiscovery::Unavailable(error) = private_discovery {
        if cached_repo_ref.is_none() {
            bail!("private Git relay discovery is unavailable: {error}");
        }
    }
    let private_probe =
        cached_repo_ref.is_none() && private_discovery.requires_repository_only_probe();
    let mut repository_relays_only = cached_repo_ref
        .as_ref()
        .is_some_and(|repo_ref| repo_ref.private)
        || private_discovery_unavailable
        || private_probe;
    let report = loop {
        let mut private_coordinate = selected_maintainer_coordinate.clone();
        private_coordinate.relays = private_discovery.relays().to_vec();
        let fetch_coordinate = if repository_relays_only && private_probe {
            &private_coordinate
        } else {
            &*selected_maintainer_coordinate
        };
        let (relay_reports, progress_reporter) = client
            .fetch_all(
                Some(git_repo_path),
                Some(fetch_coordinate),
                &HashSet::new(),
                &HashSet::new(),
                repository_relays_only,
            )
            .await?;
        let outcome = consolidate_fetch_outcome(relay_reports);
        if !outcome.had_errors || !verbose {
            let _ = progress_reporter.clear();
        }
        if repository_relays_only && private_probe {
            let discovered_privacy =
                get_repo_ref_from_cache(Some(git_repo_path), selected_maintainer_coordinate)
                    .await
                    .ok()
                    .map(|repo_ref| repo_ref.private);
            let private_probe_completed =
                outcome.all_required_relays_completed(private_discovery.relays().len());
            match private_relay_probe_decision(discovered_privacy, private_probe_completed) {
                PrivateRelayProbeDecision::UsePrivateResult => {
                    if let Ok(repo_ref) =
                        get_repo_ref_from_cache(Some(git_repo_path), selected_maintainer_coordinate)
                            .await
                    {
                        selected_maintainer_coordinate.relays = repo_ref.relays;
                    }
                }
                PrivateRelayProbeDecision::RetryPublicDiscovery => {
                    repository_relays_only = false;
                    continue;
                }
                PrivateRelayProbeDecision::FailClosed => {
                    bail!(
                        "private repository relay probe failed; refusing to query public discovery relays"
                    );
                }
            }
        }
        break outcome.report;
    };
    if report.to_string().is_empty() {
        if verbose {
            term.write_line("nostr: no updates")?;
        }
    } else {
        term.write_line(&format!("nostr updates: {report}"))?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_signer_from_git_command_config() {
        let mut command = Command::new("git");
        command
            .env_remove("GIT_CONFIG_COUNT")
            .env("GIT_CONFIG_PARAMETERS", "'nostr.signer'='Dan Conway'");

        assert_eq!(
            command_config_value_from(&mut command, "nostr.signer").unwrap(),
            Some("Dan Conway".to_string())
        );
    }

    /// A `nostr.signer` from git *config* must never count as a per-command
    /// selection: only that keeps a broken configured signer from failing
    /// read-only commands, which fall back to anonymous relay access.
    #[test]
    fn config_scope_signer_is_not_a_command_selection() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            ["init", "-q", "."].as_slice(),
            ["config", "--local", "nostr.signer", "configured-alice"].as_slice(),
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        let git_in_repo = || {
            let mut command = Command::new("git");
            command
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env_remove("GIT_CONFIG_COUNT");
            command
        };

        // command-config env for an unrelated key must not promote the
        // configured signer to command scope
        let mut command = git_in_repo();
        command.env("GIT_CONFIG_PARAMETERS", "'other.key'='x'");
        assert_eq!(
            command_config_value_from(&mut command, "nostr.signer").unwrap(),
            None
        );

        // an explicit `-c nostr.signer` wins over the configured value
        let mut command = git_in_repo();
        command.env("GIT_CONFIG_PARAMETERS", "'nostr.signer'='cmd-bob'");
        assert_eq!(
            command_config_value_from(&mut command, "nostr.signer").unwrap(),
            Some("cmd-bob".to_string())
        );
    }

    #[test]
    fn parses_force_with_lease_cas_option() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            parse_cas_option(&format!("refs/heads/main:{oid}")).unwrap(),
            ("refs/heads/main".to_string(), Some(oid.to_string()))
        );
    }

    #[test]
    fn parses_force_with_lease_for_missing_ref() {
        assert_eq!(
            parse_cas_option("refs/heads/new:0000000000000000000000000000000000000000").unwrap(),
            ("refs/heads/new".to_string(), None)
        );
    }

    #[test]
    fn decode_backslash_n_to_newline() {
        assert_eq!(
            decode_push_option_escapes(r"first line\nsecond line"),
            "first line\nsecond line"
        );
    }

    #[test]
    fn decode_multiple_newlines() {
        assert_eq!(
            decode_push_option_escapes(r"line1\n\nline3\nline4"),
            "line1\n\nline3\nline4"
        );
    }

    #[test]
    fn decode_double_backslash_n_to_literal_backslash_n() {
        assert_eq!(
            decode_push_option_escapes(r"keep \\n literal"),
            "keep \\n literal"
        );
    }

    #[test]
    fn decode_no_escapes_unchanged() {
        assert_eq!(
            decode_push_option_escapes("no escapes here"),
            "no escapes here"
        );
    }

    #[test]
    fn decode_trailing_backslash_preserved() {
        assert_eq!(decode_push_option_escapes(r"ends with \"), "ends with \\");
    }

    #[test]
    fn decode_backslash_followed_by_other_char_preserved() {
        assert_eq!(decode_push_option_escapes(r"a \t tab"), "a \\t tab");
    }

    #[test]
    fn decode_empty_string() {
        assert_eq!(decode_push_option_escapes(""), "");
    }

    #[test]
    fn decode_mixed_escapes() {
        assert_eq!(
            decode_push_option_escapes(r"line1\nline2\\nstill line2\nline3"),
            "line1\nline2\\nstill line2\nline3"
        );
    }

    #[test]
    fn parses_target_and_base_push_options_without_forwarding_them() {
        let mut options = PushOptions::default();

        assert!(apply_ngit_push_option(
            &mut options,
            "target-branch",
            "release/2.x"
        ));
        assert!(apply_ngit_push_option(
            &mut options,
            "base",
            "nevent1parent"
        ));

        assert_eq!(
            options.proposal.target_branch.as_deref(),
            Some("release/2.x")
        );
        assert_eq!(options.proposal.base.as_deref(), Some("nevent1parent"));
    }

    #[test]
    fn strip_git_quoting_removes_quotes_and_unescapes() {
        // Git sends: "description=First line\\nSecond line"
        // After strip: description=First line\nSecond line
        assert_eq!(
            strip_git_quoting(r#""description=First line\\nSecond line""#),
            r"description=First line\nSecond line"
        );
    }

    #[test]
    fn strip_git_quoting_no_quotes_unchanged() {
        assert_eq!(
            strip_git_quoting("description=plain text"),
            "description=plain text"
        );
    }

    #[test]
    fn strip_git_quoting_then_decode_produces_newlines() {
        // Simulates the full pipeline for a git-quoted push option:
        // User writes: description=line1\n\nline2
        // Git sends:   "description=line1\\n\\nline2"
        let git_quoted = r#""description=line1\\n\\nline2""#;
        let unquoted = strip_git_quoting(git_quoted);
        assert_eq!(unquoted, r"description=line1\n\nline2");
        let (key, value) = unquoted.split_once('=').unwrap();
        assert_eq!(key, "description");
        assert_eq!(decode_push_option_escapes(value), "line1\n\nline2");
    }

    #[test]
    fn strip_git_quoting_preserves_user_double_backslash() {
        // User writes: description=keep \\n literal
        // Git sends:   "description=keep \\\\n literal"
        let git_quoted = r#""description=keep \\\\n literal""#;
        let unquoted = strip_git_quoting(git_quoted);
        assert_eq!(unquoted, r"description=keep \\n literal");
        let (_, value) = unquoted.split_once('=').unwrap();
        assert_eq!(decode_push_option_escapes(value), "keep \\n literal");
    }
}
