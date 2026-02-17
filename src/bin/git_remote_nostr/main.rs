#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures, clippy::module_name_repetitions)]
// better solution to dead_code error on multiple binaries than https://stackoverflow.com/a/66196291
#![allow(dead_code)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use core::str;
use std::{
    collections::HashSet,
    env, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use client::{Connect, consolidate_fetch_reports, get_repo_ref_from_cache, is_verbose};
use git::{RepoActions, nostr_url::NostrUrlDecoded};
use ngit::{
    client::{self, Params},
    git::{self, utils::set_git_timeout},
    login::existing::load_existing_login,
    utils::read_line,
};
use nostr::nips::nip19::Nip19Coordinate;

use crate::{client::Client, git::Repo};

#[derive(Default, Clone)]
struct PushOptions {
    title: Option<String>,
    description: Option<String>,
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

mod fetch;
mod list;
mod push;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("NGITTEST").is_ok() {
        std::env::set_var("NGIT_VERBOSE", "1");
    }

    let Some((decoded_nostr_url, git_repo)) = process_args().await? else {
        return Ok(());
    };

    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    if let Ok((signer, _, _)) = load_existing_login(
        &Some(&git_repo),
        &None,
        &None,
        &None,
        None,
        true,
        false,
        false,
    )
    .await
    {
        // signer for to respond to relay auth request
        client.set_signer(signer).await;
    }

    fetching_with_report_for_helper(git_repo_path, &client, &decoded_nostr_url.coordinate).await?;

    let mut repo_ref =
        get_repo_ref_from_cache(Some(git_repo_path), &decoded_nostr_url.coordinate).await?;

    repo_ref.set_nostr_git_url(decoded_nostr_url.clone());

    let _ = set_git_timeout();

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
                if let Some((key, value)) = option.split_once('=') {
                    match key {
                        "title" => {
                            push_options.title = Some(decode_push_option_escapes(value));
                        }
                        "description" => {
                            push_options.description = Some(decode_push_option_escapes(value));
                        }
                        _ => {}
                    }
                }
                println!("ok");
            }
            ["option", ..] => {
                println!("unsupported");
            }
            ["fetch", oid, refstr] => {
                fetch::run_fetch(&git_repo, &repo_ref, &stdin, oid, refstr).await?;
            }
            ["push", refspec] => {
                let title_description = push_options.validate()?;
                push::run_push(
                    &git_repo,
                    &repo_ref,
                    &stdin,
                    refspec,
                    &client,
                    list_outputs.clone(),
                    title_description,
                )
                .await?;
                push_options = PushOptions::default();
            }
            ["list"] => {
                list_outputs = Some(list::run_list(&git_repo, &repo_ref, false).await?);
            }
            ["list", "for-push"] => {
                list_outputs = Some(list::run_list(&git_repo, &repo_ref, true).await?);
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

async fn process_args() -> Result<Option<(NostrUrlDecoded, Repo)>> {
    let args = env::args();
    let args = args.skip(1).take(2).collect::<Vec<_>>();

    if env::args().nth(1).as_deref() == Some("--version") {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("v{VERSION}");
        return Ok(None);
    }

    let ([_, nostr_remote_url] | [nostr_remote_url]) = args.as_slice() else {
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
    };

    let git_repo = Repo::from_path(&PathBuf::from(
        std::env::var("GIT_DIR").context("git should set GIT_DIR when remote helper is called")?,
    ))?;

    let decoded_nostr_url = NostrUrlDecoded::parse_and_resolve(nostr_remote_url, &Some(&git_repo))
        .await
        .context("invalid nostr url")?;

    Ok(Some((decoded_nostr_url, git_repo)))
}

async fn fetching_with_report_for_helper(
    git_repo_path: &Path,
    client: &Client,
    trusted_maintainer_coordinate: &Nip19Coordinate,
) -> Result<()> {
    let term = console::Term::stderr();
    let verbose = is_verbose();
    if verbose {
        term.write_line("nostr: fetching...")?;
    }
    let (relay_reports, progress_reporter) = client
        .fetch_all(
            Some(git_repo_path),
            Some(trusted_maintainer_coordinate),
            &HashSet::new(),
        )
        .await?;
    let has_errors = relay_reports.iter().any(std::result::Result::is_err);
    if !has_errors || !verbose {
        let _ = progress_reporter.clear();
    }
    let report = consolidate_fetch_reports(relay_reports);
    if report.to_string().is_empty() {
        if verbose {
            term.write_line("nostr: no updates")?;
        }
    } else {
        term.write_line(&format!("nostr updates: {report}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
