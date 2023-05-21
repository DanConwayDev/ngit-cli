use dialoguer::Select;
use nostr::{Event, EventId, Filter};
use nostr_sdk::blocking::Client;

use crate::{kind::Kind, repos::repo::Repo};

pub fn find_select_recent_repos(
    client: &Client,
) -> EventId {

    let mut repo_events: Vec<Event> = client.get_events_of(
        vec![
            Filter::new()
                .hashtag("ngit-format-0.0.1")
                .kind(
                    Kind::InitializeRepo.into_sdk_custom_kind(),
                )
                .limit(10),
        ],
        None,
    )
        .expect("get_events_of to not return an error");

    repo_events.sort();
    repo_events.dedup();

    if repo_events.is_empty() {
        panic!("could not find any repositories. Create one with ngit init?")
    }

    let repos: Vec<Repo> = repo_events.iter().map(|r|
        Repo::new_from_event(r.clone())
            .expect("repo to be well formed event")
    ).collect();
    let repo_names: Vec<String> = repos.iter().map(|r|
        match r.name.clone() {
            None => "(untitled)".to_string(),
            Some(name) => name,
        }
    ).collect();

    // select pr to review
    let i = Select::new()
        .with_prompt("clone for a repository on selected relays")
        .items(&repo_names)
        .report(false)
        .interact()
        .unwrap();
    // display nevent
    println!("selected repo: {} {}",repo_names[i], repos[i].nevent());
    repos[i].id
}