#![cfg_attr(not(test), warn(clippy::pedantic))]
#![allow(clippy::large_futures)]
// better solution to dead_code error on multiple binaries than https://stackoverflow.com/a/66196291
#![allow(dead_code)]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use core::str;
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, Stdin},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use auth_git2::GitAuthenticator;
#[cfg(not(test))]
use client::Connect;
use client::{
    fetching_with_report, get_repo_ref_from_cache, get_state_from_cache, sign_event, STATE_KIND,
};
use git::RepoActions;
use git2::{Oid, Repository};
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{EventBuilder, PublicKey, Tag, Url};
use nostr_signer::NostrSigner;
use repo_ref::RepoRef;
use repo_state::RepoState;
use sub_commands::send::send_events;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::git::Repo;

mod cli;
mod cli_interactor;
mod client;
mod config;
mod git;
mod key_handling;
mod login;
mod repo_ref;
mod repo_state;
mod sub_commands;

#[tokio::main]
async fn main() -> Result<()> {
    let args = env::args();
    let args = args.skip(1).take(2).collect::<Vec<_>>();

    let ([_, nostr_remote_url] | [nostr_remote_url]) = args.as_slice() else {
        bail!("invalid arguments - no url");
    };
    if env::args().nth(1).as_deref() == Some("--version") {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("v{VERSION}");
        return Ok(());
    }

    let git_repo = Repo::from_path(&PathBuf::from(
        std::env::var("GIT_DIR").context("git should set GIT_DIR when remote helper is called")?,
    ))?;
    let git_repo_path = git_repo.get_path()?;

    #[cfg(not(test))]
    let client = Client::default();
    #[cfg(test)]
    let client = <MockConnect as std::default::Default>::default();

    let repo_coordinates =
        nostr_git_url_to_repo_coordinates(nostr_remote_url).context("invalid nostr url")?;

    fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &repo_coordinates).await?;

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        let tokens = read_line(&stdin, &mut line)?;

        match tokens.as_slice() {
            ["capabilities"] => {
                println!("option");
                println!("push");
                println!("fetch");
                println!();
            }
            ["option", "verbosity"] => {
                println!("ok");
            }
            ["option", ..] => {
                println!("unsupported");
            }
            ["fetch", oid, _refstr] => {
                fetch(&git_repo.git_repo, &repo_ref, &stdin, oid)?;
            }
            ["push", refspec] => {
                push(
                    &git_repo,
                    &repo_ref,
                    nostr_remote_url,
                    &stdin,
                    refspec,
                    &client,
                )
                .await?;
            }
            ["list"] => {
                list(&git_repo, &repo_ref, false).await?;
            }
            ["list", "for-push"] => {
                list(&git_repo, &repo_ref, true).await?;
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

/// Read one line from stdin, and split it into tokens.
pub(crate) fn read_line<'a>(stdin: &io::Stdin, line: &'a mut String) -> io::Result<Vec<&'a str>> {
    line.clear();

    let read = stdin.read_line(line)?;
    if read == 0 {
        return Ok(vec![]);
    }
    let line = line.trim();
    let tokens = line.split(' ').filter(|t| !t.is_empty()).collect();

    Ok(tokens)
}

fn nostr_git_url_to_repo_coordinates(url: &str) -> Result<HashSet<Coordinate>> {
    let mut repo_coordinattes = HashSet::new();
    let url = Url::parse(url)?;

    if url.scheme().ne("nostr") {
        bail!("nostr git url must start with nostr://")
    }

    if let Ok(coordinate) = Coordinate::parse(url.domain().context("no naddr")?) {
        if coordinate.kind.eq(&nostr_sdk::Kind::GitRepoAnnouncement) {
            repo_coordinattes.insert(coordinate);
            return Ok(repo_coordinattes);
        }
        bail!("naddr doesnt point to a git repository announcement");
    }

    if let Some(domain) = url.domain() {
        if let Ok(public_key) = PublicKey::parse(domain) {
            if url.path().is_empty() {
                bail!(
                    "nostr git url should include the repo identifier eg nostr://npub123/the-repo-identifer"
                );
            }
            let mut relays = vec![];
            for (name, value) in url.query_pairs() {
                if name.contains("relay") {
                    let mut decoded = urlencoding::decode(&value)
                        .context("could not parse relays in nostr git url")?
                        .to_string();
                    if !decoded.starts_with("ws://") && !decoded.starts_with("wss://") {
                        decoded = format!("wss://{decoded}");
                    }
                    let url =
                        Url::parse(&decoded).context("could not parse relays in nostr git url")?;
                    relays.push(url.to_string());
                }
            }
            repo_coordinattes.insert(Coordinate {
                identifier: "ngit".to_string(),
                public_key,
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays,
            });
            return Ok(repo_coordinattes);
        }
    }
    bail!(
        "nostr git url must be in format nostr://naddr123 or nostr://npub123/identifer?relay=wss://relay-example.com&relay1=wss://relay-example.org"
    );
}

async fn list(git_repo: &Repo, repo_ref: &RepoRef, for_push: bool) -> Result<()> {
    if let Ok(repo_state) = get_state_from_cache(git_repo.get_path()?, repo_ref).await {
        for (name, value) in &repo_state.state {
            if value.starts_with("ref: ") {
                if !for_push {
                    println!("{} {name}", value.replace("ref: ", "@"));
                }
            } else {
                println!("{value} {name}");
            }
        }
    } else {
        let git_server_remote_url = repo_ref
            .git_server
            .first()
            .context("no git server listed in nostr repository announcement")?;
        let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_remote_url)?;
        git_server_remote.connect(git2::Direction::Fetch)?;

        for head in git_server_remote.list()? {
            if let Some(symbolic_reference) = head.symref_target() {
                if !for_push {
                    println!("@{} {}", symbolic_reference, head.name());
                }
            } else {
                println!("{} {}", head.oid(), head.name());
            }
        }
        git_server_remote.disconnect()?;
    }
    println!();
    Ok(())
}

fn fetch(git_repo: &Repository, repo_ref: &RepoRef, stdin: &Stdin, oid: &str) -> Result<()> {
    let oids = get_oids_from_fetch_batch(stdin, oid)?;

    let mut errors = HashMap::new();
    for git_server_url in &repo_ref.git_server {
        if let Err(e) = fetch_from_git_server(git_repo, &oids, git_server_url) {
            errors.insert(git_server_url.to_string(), e);
        } else {
            println!();
            return Ok(());
        }
    }
    bail!(
        "failed to fetch objects in nostr state event from:\r\n{}",
        errors
            .iter()
            .map(|(url, error)| format!("{url}: {error}"))
            .collect::<Vec<String>>()
            .join("\r\n")
    );
}

fn fetch_from_git_server(
    git_repo: &Repository,
    oids: &[String],
    git_server_url: &str,
) -> Result<()> {
    let mut git_server_remote = git_repo.remote_anonymous(git_server_url)?;
    git_server_remote.connect(git2::Direction::Fetch)?;
    git_server_remote.download(oids, None)?;
    git_server_remote.disconnect()?;
    Ok(())
}

async fn push(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    nostr_remote_url: &str,
    stdin: &Stdin,
    initial_refspec: &str,
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
) -> Result<()> {
    // if no state events - create from first git server listed
    let refspecs = get_refspecs_from_push_batch(stdin, initial_refspec)?;
    let git_server_url = repo_ref
        .git_server
        .first()
        .context("no git server listed in nostr repository announcement")?;
    let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_url)?;

    let auth = GitAuthenticator::default();
    let git_config = git_repo.git_repo.config()?;
    let mut push_options = git2::PushOptions::new();
    let mut remote_callbacks = git2::RemoteCallbacks::new();
    remote_callbacks.credentials(auth.credentials(&git_config));
    remote_callbacks.push_update_reference(|name, error| {
        if let Some(error) = error {
            println!("error {name} {error}");
        } else {
            if let Some(refspec) = refspecs
                .iter()
                .find(|r| r.contains(format!(":{name}").as_str()))
            {
                if let Err(e) =
                    update_remote_refs_pushed(&git_repo.git_repo, refspec, nostr_remote_url)
                        .context("could not update remote_ref locally")
                {
                    return Err(git2::Error::from_str(e.to_string().as_str()));
                }
            }
            println!("ok {name}",);
        }
        Ok(())
    });
    push_options.remote_callbacks(remote_callbacks);
    git_server_remote.push(&refspecs, Some(&mut push_options))?;
    git_server_remote.disconnect()?;

    // TODO check whether push was succesful before proceeding - geting outcome from
    // callback isn't straightforward

    let new_state = generate_updated_state(git_repo, repo_ref, &refspecs).await?;

    // TODO enable interactive login
    let (signer, user_ref) = login::launch(
        git_repo,
        &None,
        &None,
        &None,
        &None,
        Some(client),
        false,
        true,
    )
    .await?;
    let new_repo_state = RepoState::build(repo_ref.identifier.clone(), new_state, &signer).await?;

    send_events(
        client,
        git_repo.get_path()?,
        vec![new_repo_state.event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        false,
        true,
    )
    .await?;

    // silently push to any other git servers
    for (i, git_server_url) in repo_ref.git_server.iter().enumerate() {
        // we have already pushed to the first one
        if i.gt(&0) {
            if let Ok(mut git_server_remote) = git_repo.git_repo.remote_anonymous(git_server_url) {
                let auth = GitAuthenticator::default();
                let git_config = git_repo.git_repo.config()?;
                let mut push_options = git2::PushOptions::new();
                let mut remote_callbacks = git2::RemoteCallbacks::new();
                remote_callbacks.credentials(auth.credentials(&git_config));
                push_options.remote_callbacks(remote_callbacks);
                let _ = git_server_remote.push(&refspecs, Some(&mut push_options));
                let _ = git_server_remote.disconnect();
            }
        }
    }
    // todo report on errors

    println!();
    Ok(())
}

async fn generate_updated_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    refspecs: &Vec<String>,
) -> Result<HashMap<String, String>> {
    let new_state = {
        if let Ok(mut repo_state) = get_state_from_cache(git_repo.get_path()?, repo_ref).await {
            for refspec in refspecs {
                let (from, to) = refspec_to_from_to(refspec)?;
                if from.is_empty() {
                    // delete
                    repo_state.state.remove(to);
                } else {
                    // add or update
                    repo_state.state.insert(
                        to.to_string(),
                        reference_to_ref_value(&git_repo.git_repo, to).unwrap(),
                    );
                }
            }
            repo_state.state
        } else {
            let mut state = HashMap::new();
            let git_server_url = repo_ref
                .git_server
                .first()
                .context("no git server listed in nostr repository announcement")?;
            let mut git_server_remote = git_repo.git_repo.remote_anonymous(git_server_url)?;
            git_server_remote.connect(git2::Direction::Fetch)?;
            for head in git_server_remote.list()? {
                state.insert(
                    head.name().to_string(),
                    if let Some(symbolic_ref) = head.symref_target() {
                        format!("ref: {symbolic_ref}")
                    } else {
                        head.oid().to_string()
                    },
                );
            }
            git_server_remote.disconnect()?;
            state
        }
    };
    Ok(new_state)
}

fn update_remote_refs_pushed(
    git_repo: &Repository,
    refspec: &str,
    nostr_remote_url: &str,
) -> Result<()> {
    let (from, _) = refspec_to_from_to(refspec)?;

    let target_ref_name = refspec_remote_ref_name(git_repo, refspec, nostr_remote_url)?;

    if from.is_empty() {
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.delete()?;
        }
    } else {
        let commit = reference_to_commit(git_repo, from)
            .context(format!("cannot get commit of reference {from}"))?;
        if let Ok(mut remote_ref) = git_repo.find_reference(&target_ref_name) {
            remote_ref.set_target(commit, "updated by nostr remote helper")?;
        } else {
            git_repo.reference(
                &target_ref_name,
                commit,
                false,
                "created by nostr remote helper",
            )?;
        }
    }
    Ok(())
}

fn refspec_to_from_to(refspec: &str) -> Result<(&str, &str)> {
    if !refspec.contains(':') {
        bail!(
            "refspec should contain a colon (:) but consists of: {}",
            refspec
        );
    }
    let parts = refspec.split(':').collect::<Vec<&str>>();
    Ok((parts.first().unwrap(), parts.get(1).unwrap()))
}

fn refspec_remote_ref_name(
    git_repo: &Repository,
    refspec: &str,
    nostr_remote_url: &str,
) -> Result<String> {
    let (_, to) = refspec_to_from_to(refspec)?;
    let nostr_remote = git_repo
        .find_remote(&get_remote_name_by_url(git_repo, nostr_remote_url)?)
        .context("we should have just located this remote")?;
    Ok(format!(
        "refs/remotes/{}/{}",
        nostr_remote.name().context("remote should have a name")?,
        to.replace("refs/heads/", ""), // TODO only replace if it begins with this
    ))
}

fn reference_to_commit(git_repo: &Repository, reference: &str) -> Result<Oid> {
    Ok(git_repo
        .find_reference(reference)
        .context(format!("cannot find reference: {reference}"))?
        .peel_to_commit()
        .context(format!("cannot get commit from reference: {reference}"))?
        .id())
}

// this maybe a commit id or a ref: pointer
fn reference_to_ref_value(git_repo: &Repository, reference: &str) -> Result<String> {
    let reference_obj = git_repo
        .find_reference(reference)
        .context(format!("cannot find reference: {reference}"))?;
    if let Some(symref) = reference_obj.symbolic_target() {
        Ok(symref.to_string())
    } else {
        Ok(reference_obj
            .peel_to_commit()
            .context(format!("cannot get commit from reference: {reference}"))?
            .id()
            .to_string())
    }
}

fn get_remote_name_by_url(git_repo: &Repository, url: &str) -> Result<String> {
    let remotes = git_repo.remotes()?;
    Ok(remotes
        .iter()
        .find(|r| {
            if let Some(name) = r {
                if let Some(remote_url) = git_repo.find_remote(name).unwrap().url() {
                    url == remote_url
                } else {
                    false
                }
            } else {
                false
            }
        })
        .context("could not find remote with matching url")?
        .context("remote with matching url must be named")?
        .to_string())
}

fn get_oids_from_fetch_batch(stdin: &Stdin, initial_oid: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut oids = vec![initial_oid.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["fetch", oid, _refstr] => {
                oids.push((*oid).to_string());
            }
            [] => break,
            _ => bail!(
                "after a `fetch` command we are only expecting another fetch or an empty line"
            ),
        }
    }
    Ok(oids)
}

fn get_refspecs_from_push_batch(stdin: &Stdin, initial_refspec: &str) -> Result<Vec<String>> {
    let mut line = String::new();
    let mut refspecs = vec![initial_refspec.to_string()];
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["push", spec] => {
                refspecs.push((*spec).to_string());
            }
            [] => break,
            _ => {
                bail!("after a `push` command we are only expecting another push or an empty line")
            }
        }
    }
    Ok(refspecs)
}

impl RepoState {
    pub async fn build(
        identifier: String,
        state: HashMap<String, String>,
        signer: &NostrSigner,
    ) -> Result<RepoState> {
        let mut tags = vec![Tag::identifier(identifier.clone())];
        for (name, value) in &state {
            tags.push(Tag::custom(
                nostr_sdk::TagKind::Custom(name.into()),
                vec![value.clone()],
            ));
        }
        let event = sign_event(EventBuilder::new(STATE_KIND, "", tags), signer).await?;
        Ok(RepoState {
            identifier,
            state,
            event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod nostr_git_url_to_repo_coordinates {
        use nostr_sdk::PublicKey;

        use super::*;

        fn get_model_coordinate(relays: bool) -> Coordinate {
            Coordinate {
                identifier: "ngit".to_string(),
                public_key: PublicKey::parse(
                    "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                )
                .unwrap(),
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays: if relays {
                    vec!["wss://nos.lol/".to_string()]
                } else {
                    vec![]
                },
            }
        }

        #[test]
        fn from_naddr() -> Result<()> {
            assert_eq!(
                nostr_git_url_to_repo_coordinates(
                    "nostr://naddr1qqzxuemfwsqs6amnwvaz7tmwdaejumr0dspzpgqgmmc409hm4xsdd74sf68a2uyf9pwel4g9mfdg8l5244t6x4jdqvzqqqrhnym0k2qj"
                )?,
                HashSet::from([get_model_coordinate(true)]),
            );
            Ok(())
        }
        mod from_npub_slah_identifier {
            use super::*;

            #[test]
            fn without_relay() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                    )?,
                    HashSet::from([get_model_coordinate(false)]),
                );
                Ok(())
            }

            #[test]
            fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay=nos.lol"
                    )?,
                    HashSet::from([get_model_coordinate(true)]),
                );
                Ok(())
            }

            #[test]
            fn with_encoded_relay() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(&format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}",
                        urlencoding::encode("wss://nos.lol")
                    ))?,
                    HashSet::from([get_model_coordinate(true)]),
                );
                Ok(())
            }
            #[test]
            fn with_multiple_encoded_relays() -> Result<()> {
                assert_eq!(
                    nostr_git_url_to_repo_coordinates(&format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}&relay1={}",
                        urlencoding::encode("wss://nos.lol"),
                        urlencoding::encode("wss://relay.damus.io"),
                    ))?,
                    HashSet::from([Coordinate {
                        identifier: "ngit".to_string(),
                        public_key: PublicKey::parse(
                            "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                        )
                        .unwrap(),
                        kind: nostr_sdk::Kind::GitRepoAnnouncement,
                        relays: vec![
                            "wss://nos.lol/".to_string(),
                            "wss://relay.damus.io/".to_string(),
                        ],
                    }]),
                );
                Ok(())
            }
        }
    }
}
