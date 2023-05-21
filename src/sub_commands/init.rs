use std::{str::FromStr, fs::{OpenOptions, File}, io::Write};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use clap::{Args};
use indicatif::ProgressBar;
use nostr_sdk::prelude::*;

use crate::{config::{load_config, save_conifg}, groups::{init::{InitializeGroup}, group::{Group}}, repos::{init::InitializeRepo, repo::Repo}, utils::{save_event, create_client, get_or_generate_keys}, cli_helpers::select_relays, repo_config::RepoConfig};

#[derive(Args)]
struct InitRepo {
    // Repo Name
    #[arg(short, long)]
    name: String,
    /// Admin Group ID
    #[arg(long)]
    admin_group_id: Option<String>,
    /// Relays
    #[arg(short, long)]
    relays: Option<String>,
}

#[derive(Args)]
pub struct InitSubCommand {
    /// Repo Name
    #[arg(short, long)]
    name: Option<String>,
}

pub fn create_and_broadcast_init(
    relays: Vec<String>,
    sub_command_args: &InitSubCommand,
) -> Result<()> {
    
    let mut cfg = load_config();

    let repo_dir_path = std::env::current_dir().unwrap();
    
    // check for potential problems
    let ngit_path = repo_dir_path.clone().join(".ngit");
    if ngit_path.is_dir() && (
        !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("ngit already initialized! Do you want overwrite it with a fresh repoisotry?")
            .default(false)
            .interact()
            .unwrap()
        || !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Are you sure?")
            .default(false)
            .interact()
            .unwrap()
    ) { panic!("aborted as ngit repository already exists."); };

    let git_path = repo_dir_path.clone().join(".git");
    if git_path.is_dir() && (
        !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("git has already been initialized here. For this alpha ngit prototype its best to start with a fresh repository. Continue anyway?")
            .default(false)
            .interact()
            .unwrap()
    ) { panic!("aborted as git repository already initialized."); };

    // collect information from user

    let dir_name: String =String::from(repo_dir_path.file_name().unwrap().to_string_lossy());
    let repo_name: String = match &sub_command_args.name {
        Some(name) => name.clone(),
        None => {
            Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Repo Name")
                .default(dir_name)
                .interact_text()
                .unwrap()
        },
    };    
    let repo_relays = select_relays(&mut cfg, &relays)?;

    let mut repo_group_members: Vec<String> = vec![];
    loop {
        if Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Would you like add other maintainers now?")
            .default(false)
            .interact()
            .unwrap()
        {
            let member_key_input: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("npub or hex (n to abort)")
                .interact_text()
                .unwrap()
            ;
            if member_key_input.starts_with("npub") {
                match XOnlyPublicKey::from_bech32(member_key_input) {
                    Ok(k) => { repo_group_members.push(k.to_string()); },
                    Err(e) => { println!("{}",e) },
                }}
            else {
                match XOnlyPublicKey::from_str(member_key_input.as_str()) {
                    Ok(k) => { repo_group_members.push(k.to_string()); },
                    Err(e) => { println!("{}",e) },
                }
            }
        }
        else { break; }
    }

    let keys = get_or_generate_keys(&mut cfg);
    let mut events_to_broadcast: Vec<Event> = vec![];

    // delay adding user as group member so keys are the last thing asked for
    repo_group_members.push(keys.public_key().to_string());

    let admin_group_event = match cfg.default_admin_group_event_serialized {
        None => {
            let new_admin_group = Group::new(
                &InitializeGroup::new()
                    .members(
                        vec![
                            keys.public_key().to_string(),
                        ],
                        vec![],
                    )
                    .relays(&repo_relays),
                &keys,
            ).unwrap();
            cfg.default_admin_group_event_serialized = Some(new_admin_group.events[0].as_json());
            save_conifg(&cfg);
            events_to_broadcast.push(new_admin_group.events[0].clone());
            new_admin_group.events[0].clone()
        },
        Some(admin) => Group::new_from_json_event(admin.clone())
            .expect("default_admin_group_event_serialized in MyConfig to load into Group")
            .events[0].clone(),
    };

    let new_repo_group = Group::new(
        &InitializeGroup::new()
            .name(format!("{repo_name} maintainers (ngit)"))
            .members(
                repo_group_members,
                vec![],
            )
            .relays(&repo_relays)
        ,
        &keys,
    ).unwrap();
    events_to_broadcast.push(new_repo_group.events[0].clone());   

    let new_repo = Repo::new(
        &InitializeRepo::new()
            .name(&repo_name)
            .relays(&repo_relays)
            .maintainers_group(new_repo_group.get_ref())
        ,
        &keys,
    ).unwrap();    
    events_to_broadcast.push(new_repo.events[0].clone());   

    // crate .ngit folder and store the repo and group reference and associated events (?)
    for p in [
        "groups",
        "branches",
        "patches",
        "merges",
        "prs",
        "issues",
        "comments",
    ] { std::fs::create_dir_all(ngit_path.join(p)).unwrap(); }

    // save repo event locally
    save_event(
        ngit_path.join(format!("groups/{}.json",admin_group_event.id.to_string())),
        &admin_group_event,
    ).unwrap();
    
    save_event(
        ngit_path.join(format!("groups/{}.json",new_repo_group.id.to_string())),
        &new_repo_group.events[0],
    ).unwrap();
    save_event(
        ngit_path.join("repo.json"),
        &new_repo.events[0],
    ).unwrap();

    // set repo config
    let mut repo_config = RepoConfig::open(&repo_dir_path);
    for b in ["main", "master"] {
        repo_config.set_mapping(&b.to_string(), &new_repo.events[0].id.to_string());

    }
    repo_config.set_last_branch_ref_update_time(new_repo.events[0].created_at.clone());

    // initialise git
    git2::Repository::init(repo_dir_path.clone()).unwrap();

    // add .gitignore
    let gitignore_path = repo_dir_path.join(".gitignore");
    let mut gitignore_file = if gitignore_path.is_file() {
        OpenOptions::new()
            .write(true)
            .append(true)
            .open(&gitignore_path)
            .expect(".gitignore to open")
    } else {
        File::create(gitignore_path)
            .expect("create and open .gitignore file")
    };
    writeln!(gitignore_file, ".ngit")
        .expect(".ngit added to gitignore");

    let spinner = ProgressBar::new_spinner();
    spinner.set_message("Broadcasting... if this takes 20s+, there was a problem broadcasting to one or more relays even if it says 'Repository Initialised'.");

    let client = create_client(&keys, repo_relays.clone())?;
    for e in &events_to_broadcast { 
        match client.send_event(e.clone()) {
            Ok(_) => (),
            // TODO: this isn't working - if a relay is specified with a type it will wait 30ish secs and then return successful
            Err(e) => { println!("error broadcasting repo event: {}",e); },
        }
        // TODO: better error handling here / reporting. potentially warn if taking a while and report on troublesome relays
    }
    spinner.finish_with_message(format!(
        "Repository Initialised! id: {}",
        Nip19Event::new(
            new_repo.id.clone(),
            vec![&repo_relays[0]],
        )
            .to_bech32()
            .expect("Nip19Event to convert to to_bech32")

    ));
    // println!("Hint: only maintainers can push to master branch and merge PRs but anyone can clone and push a forked branch.");

    // Instructions:
    // 1. make some commits on 'master' or 'main' branch.
    // 2. 'ngit push' will push them to the nostr repo (via a patch).
    // 3. make a branch called 'feature-1'
    // 3. 'ngit push' will create a PR

    // repo
        // repo 
    // PRs

    // patch


    Ok(())
}
