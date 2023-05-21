
use std::{path::PathBuf};

use nostr::{EventId, Event, prelude::{Nip19Event, ToBech32}, Tag};
use nostr_sdk::{Keys};

use crate::{groups::{group::{MembershipCollection, StartFinish}}, utils::load_file};

use super::init::{InitializeRepo, self};

/// [`Repo`] error
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error processing initialisation Repo content json - incorrect format?
    #[error("Repo cannot be initialised from content: {0}")]
    InitializeJson(#[from] init::Error),
}

/// Repo, acts a branch if root_repo is set
pub struct Repo {
    pub id: EventId,
    pub name:Option<String>,
    about:Option<String>,
    picture:Option<String>,
    pub relays:Vec<String>,
    pub maintainers_group:MembershipCollection,
    pub events:Vec<Event>,
    pub root_repo: Option<EventId>,
    hash: String, // hash of event IDs that make up this state
}

impl Repo {

    pub fn new(init:&InitializeRepo, keys:&Keys) -> Result<Self,Error> {
        let event = init.initialize(&keys);
        Repo::new_from_event(event)
    }

    pub fn open(repo_dir_path: &PathBuf) -> Self {
        Repo::new_from_json_event(
            load_file(
                repo_dir_path.join(".ngit/repo.json"),
            )
                .expect("repo.json load from file")
        )
            .expect("repo.json to produce Repo")
    }

    pub fn new_from_json_event(json_string:String) -> Result<Self,Error> {
        let event = Event::from_json(json_string)
            .expect("json_string to be formated as event");
        Repo::new_from_event(event)
    }

    pub fn new_from_event(event:Event) -> Result<Self,Error> {
        match InitializeRepo::from_json(&event.content) {
            Err(e) => return Err(Error::InitializeJson(e)),
            Ok(g) => {
                let start_finish = StartFinish { start: event.created_at, finish: None };
                // add maintainers_group
                let mut maintainers_group = MembershipCollection::new();
                match g.maintainers_group {
                    None => (),
                    Some(t) => {
                        maintainers_group.add_group_dates(
                            Tag::parse(t.into())
                                .expect("maintainers_group to parse into Tag"),
                            start_finish.clone(),
                        )
                    }
                }
                Ok(Self {
                    id: event.id,
                    name: g.name,
                    about: g.about,
                    picture: g.picture,
                    relays: g.relays,
                    maintainers_group,
                    events:vec![event],
                    root_repo:None,
                    hash: "hash".to_string(), // hash of event IDs that make up this state
                })
            }
        }        
    }

    pub fn nevent(&self) -> String {
        let e = Nip19Event {
            event_id: self.id.clone(),
            relays: if self.relays.len() > 1 {
                vec![self.relays[0].clone(),self.relays[1].clone()]
            }
            else if self.relays.len() == 1 {
                vec![self.relays[0].clone()]
            }
            else { vec![] }
        };
        e.to_bech32()
            .expect("Nip19Event to produce nevent String")
    }
}
