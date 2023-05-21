use std::fs;

use nostr::EventId;

use crate::{utils::load_file};

use super::group::Group;

pub struct Groups {
    groups:Vec<Group>
}
impl Groups {
    pub fn new() -> Self {

        let cur_dir = std::env::current_dir().unwrap();
        
        // check for potential problems
        let ngit_path = cur_dir.clone().join(".ngit");
        if !ngit_path.is_dir() {
            panic!("ngit not initialised. Run 'ngit init' first...");
        }

        let mut groups = vec![];

        for dir_entry in fs::read_dir(ngit_path.join("groups"))
            .expect("groups folder to exist and read_dir to read it")
        {
            groups.push(
                Group::new_from_json_event(
                    load_file(
                        dir_entry
                            .expect("DirEntry in read_dir should exist").path()
                    )
                        .expect("group json to load from file")
                ).expect("group json to produce Group")
            );
        }

        Self {
            groups,
        }
    }

    pub fn by_event_id(&self, id:&EventId) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == *id)
    }
}