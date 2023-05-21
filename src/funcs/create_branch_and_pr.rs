use std::{path::PathBuf};

use dialoguer::{Input, theme::ColorfulTheme, Confirm};
use nostr::{Keys, prelude::{Nip19Event, ToBech32}};
use nostr_sdk::blocking::Client;

use crate::{repos::{repo::Repo, init::InitializeRepo}, branch_refs::BranchRefs, cli_helpers::multi_select_with_add, groups::{init::{InitializeGroup}, group::{Group}}, config::{MyConfig, save_conifg}, repo_config::RepoConfig, pull_request::initialize_pull_request};

struct PullRequest {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>
}
/// returns branch_id
pub fn create_branch_and_pr(
    local_branch_name: &String,
    commits_to_push: usize,
    repo_dir_path: &PathBuf,
    repo: &Repo,
    branch_refs: &mut BranchRefs,
    keys: &Keys,
    cfg: &mut MyConfig,
    client: &Client,
) -> String {
    let new_branch_name: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "push {} commits to a branch named",
            commits_to_push,
        ))
        .with_initial_text(&local_branch_name.to_string())
        .interact_text()
        .unwrap();
    let pr_details = match Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("open a pull request?")
        .default(true)
        .interact()
        .unwrap() {
            false => None,
            true => {
                let title = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("title")
                    .with_initial_text(&new_branch_name)
                    .interact_text()
                    .unwrap();
                let tags = multi_select_with_add(
                    vec![
                        "bugfix".to_string(),
                        "feature".to_string(),
                    ],
                    vec![
                        false,
                        false,
                    ],
                    "tags",
                    "new tag",
                );
                let description = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("description")
                    .interact_text()
                    .unwrap();
                // into main / master (lookup (yes/no) - if no select from existing branches with mapping
                Some(
                    PullRequest {
                        title,
                        tags,
                        description,
                    }
                )
            },
    };
    // create it now immediately before pushing the patches
    let mut events_to_broadcast = vec![];
    
    // create/store admin group
    let admin_group = match &cfg.default_admin_group_event_serialized {
        None => {
            let new_admin_group = Group::new(
                &InitializeGroup::new()
                    .members(
                        vec![
                            keys.public_key().to_string(),
                        ],
                        vec![],
                    )
                    .relays(&repo.relays),
                &keys,
            ).unwrap();
            cfg.default_admin_group_event_serialized = Some(new_admin_group.events[0].as_json());
            save_conifg(&cfg);
            new_admin_group
        },
        Some(admin) => Group::new_from_json_event(admin.clone())
            .expect("admin group event in MyConfig loads into Group"),
    };
    events_to_broadcast.push(admin_group.events[0].clone());
    branch_refs.update(admin_group.events[0].clone());


    // create group
    let branch_group_ref = match branch_refs.is_authorized(
        None,
        &keys.public_key(),
    )
        .expect("main repo maintainers group is cached in .ngit")
    {
        // use repo maintainers group
        true => branch_refs.maintainers_group(None)
            .expect("main repo maintainers group is cached in .ngit")
            .get_ref(),
        // create branch group
        false => {
            let new_group = Group::new(
                &InitializeGroup::new()
                    .name(
                        format!(
                            "branch-of:{},named:{}",
                            match &repo.name {
                                None => "untitled",
                                Some(s) => s.as_str(),
                            },
                            &new_branch_name,
                        ),
                    )
                    .admin(admin_group.get_ref())
                    .members(
                        vec![keys.public_key().to_string()],
                        vec![
                            branch_refs.maintainers_group(None)
                                .expect("repo maintainers group to exist in .ngit directory")
                                .get_ref()
                        ]
                    )
                    .relays(&repo.relays)
                    ,
                &keys,
            )
                .expect("new branch group to be created");

            events_to_broadcast.push(new_group.events[0].clone());
            branch_refs.update(new_group.events[0].clone());
            new_group.get_ref()
        }
     };

    // create branch
    let branch_init = InitializeRepo::new()
        .name(&new_branch_name)
        .relays(&repo.relays)
        .root_repo(repo.id.to_string())
        .maintainers_group(branch_group_ref)
        .initialize(&keys);
    events_to_broadcast.push(branch_init.clone());
    branch_refs.update(branch_init.clone());

     // TODO: create PR
     match pr_details {
        None => (),
        Some(pr_details) => {
            let pull_request_init = initialize_pull_request(
                &keys, 
                &repo.id.to_string(),
                &repo.id.to_string(),
                &branch_init.id.to_string(),
                &pr_details.title,
                &pr_details.description,
                pr_details.tags
            );
            events_to_broadcast.push(pull_request_init.clone());
            branch_refs.update(pull_request_init.clone());
            println!(
                "pull request '{}' created with id: {}",
                &pr_details.title,
                Nip19Event::new(
                    pull_request_init.id.clone(),
                    vec![&repo.relays[0]],
                )
                    .to_bech32()
                    .expect("Nip19Event to convert to to_bech32")
            );
        },
     }

    // add mapping to conifg.json
    RepoConfig::open(repo_dir_path).set_mapping(
        local_branch_name,
        &branch_init.id.to_string(),
    );

    println!(
        "branch '{}' created with id: {}",
        &new_branch_name,
        Nip19Event::new(
            branch_init.id.clone(),
            vec![&repo.relays[0]],
        )
            .to_bech32()
            .expect("Nip19Event to convert to to_bech32")         
    );

    // broadcast events
    for e in &events_to_broadcast { 
        match client.send_event(e.clone()) {
            Ok(_) => (),
            // TODO: this isn't working - if a relay is specified with a type it will wait 30ish secs and then return successful
            Err(e) => { println!("error broadcasting event: {}",e); },
        }
        // TODO: better error handling here / reporting. potentially warn if taking a while and report on troublesome relays
    }

    // return branch_id
    branch_init.id.to_string()
}                        
