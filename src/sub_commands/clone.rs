use std::{path::PathBuf, env::current_dir};
use dialoguer::{theme::ColorfulTheme, Input};
use clap::{Args};
use git2::{Repository};
use nostr_sdk::{prelude::*, blocking::Client};

use crate::{config::{load_config, MyConfig}, repos::{repo::Repo}, utils::{save_event, create_client, get_stored_keys, load_event}, cli_helpers::select_relays, fetch_pull_push::fetch_pull_push, funcs::find_select_recent_repos::find_select_recent_repos};

#[derive(Args)]
struct Clone {
    /// Repo nevent
    #[arg(short, long)]
    repo: String,
}

#[derive(Args)]
pub struct CloneSubCommand {
    /// Repo nevent
    #[arg(short, long)]
    repo: Option<String>,
}

pub fn clone_from_relays(
    relays: Vec<String>,
    sub_command_args: &CloneSubCommand,
) {

    let mut cfg = load_config();

    let keys = match get_stored_keys(&mut cfg) {
        None => Keys::generate(),
        Some(k) => k.clone(),
    };

    let (repo_id, selected_relays, client) = match &sub_command_args.repo {
        Some(name) => {
            let (repo_id, selected_relays) = select_repo_id_and_relays(
                &Some(name.clone()),
                &relays,
                &mut cfg,
            );
            let client = create_client(&keys, selected_relays.clone())
                .expect("create_client returns client");
            (repo_id, selected_relays, client)
        },
        None => {
            let selected_relays = if relays.is_empty() {
                select_relays(&mut cfg, &relays)
                    .expect("select_relays() never to error")
            } else { relays.clone() };
            let client = create_client(&keys, selected_relays.clone())
                .expect("create_client returns client");
            // find repository
            let repo_id = find_select_recent_repos(&client);
            (repo_id, selected_relays, client)
        }
    };
    // find repo
    let repo = get_repo(
        &mut cfg,
        &client,
        &selected_relays,
        repo_id,
    );
    println!("Repo found!...");

    // setup directory
    let (repo_dir_path, _git_repo) = setup_dir(&repo);

    // add relays specified in repo
    client.add_relays(repo.relays.clone().iter().map(|url| (url, None)).collect())
        .expect("relays specified in repo to be added to client");

    fetch_pull_push(
        None,
        true,
        false,
        None,
        true,
        Some(repo_dir_path),
        Some(client),

    );
}

fn get_repo(
    cfg:&mut MyConfig,
    client: &Client,
    relays:&Vec<String>,
    repo_id: EventId,
) -> Repo {
    let json_path = current_dir().unwrap().join(".ngit/repo.json");

    if json_path.exists() {
        Repo::new_from_event(
            load_event(json_path)
                .expect("load_event to load repo from repo.json that exists")
        )
            .expect("new_from_event to load repo event gathered from repo.json")
    } else {
        loop {
            let repo_events = client.get_events_of(
                vec![
                    Filter::new().id(repo_id),
                ],
                None,
            )
                .expect("get_events_of to not return an error");

            match repo_events.iter().find(|e|
                e.id == repo_id
            ) {
                None => {
                    println!("repository is not on selected relays. add more relays. {:?}",relays);
                    let new_relays = select_relays(cfg, relays)
                        .expect("select_relays not to error");
                    client.add_relays(new_relays.iter().map(|url| (url, None)).collect())
                        .expect("extra relays to find repository to be added to client");
                },
                Some(e) => {
                    break Repo::new_from_event(e.clone())
                        .expect(format!("new_from_event to return Repo from repo_event: {:?}",e).as_str());
                },
            }
        }
    }
}

fn select_repo_id_and_relays(
    repo_string_param:&Option<String>,
    relays:&Vec<String>,
    cfg:&mut MyConfig,
) -> (EventId, Vec<String>) {

    // get repo_id and selected_relays
    let mut selected_relays:Vec<String> = vec![];
    let _repo_id: EventId;
    loop {
        let repo_string = match repo_string_param {
            None => {
                let response: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Repo nevent note or hex")
                .report(true)
                .interact_text()
                .unwrap();
                response
            }
            Some(ref r) => { r.clone() },
        };
        let repo_id = match Nip19Event::from_bech32(&repo_string.clone()) {
            Ok(n19) => { selected_relays = n19.relays; n19.event_id }
            Err(_) => {
                match EventId::from_bech32(repo_string.clone()) {
                    Ok(id) => { id }
                    Err(_) => {
                        match EventId::from_hex(repo_string.clone()) {
                            Ok(id) => { id }
                            Err(_) => {
                                println!("not a valid nevent, note or hex string. try again.");
                                continue;
                            }
                        }
                    }
                }
            }
        };
        if selected_relays.is_empty() {
            if relays.is_empty() {
                selected_relays = select_relays(cfg, &relays)
                    .expect("select_relays() never to error");
            }
            else {
                selected_relays = relays.clone();
            }
        }
        break (repo_id, selected_relays);
    }
}

fn setup_dir(repo: &Repo,) -> (PathBuf, Repository) {
    // setup directory
    println!("creating directory...");

    let repo_dir_name = loop {
        let proposed_name = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("clone in sub-folder named")
            .default(match &repo.name {
                None => { "".to_string() },
                Some(name) => { name.clone() }
            })
            .interact_text()
            .unwrap();
        if std::env::current_dir().unwrap().join(&proposed_name).exists() {
            println!("directory already exists. try another one...");
        }
        else { break proposed_name; }
    };
    let repo_dir_path = std::env::current_dir().unwrap().join(&repo_dir_name);
    let ngit_path = repo_dir_path.join(".ngit");
    
    // create .ngit folder and store the repo and group reference and associated events (?)
    for p in [
        "groups",
        "branches",
        "patches",
        "merges",
        "prs",
        "issues",
        "comments",
    ] { std::fs::create_dir_all(ngit_path.join(p)).unwrap(); }

    // initialise git
    let git_repo = git2::Repository::init(repo_dir_path.clone())
        .expect("git repo to be created in a empty repository");

    // save repo.json
    save_event(
        ngit_path.join("repo.json"),
        &repo.events[0],
    )
        .expect("save_event to repo.json to in .ngit directory");
    (repo_dir_path, git_repo)
}
