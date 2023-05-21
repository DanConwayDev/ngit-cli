use std::{path::PathBuf, fs, str::FromStr};

use nostr::{Event, Filter, Timestamp, secp256k1::XOnlyPublicKey, EventId};
use nostr_sdk::blocking::Client;

use crate::{utils::{load_event, save_event}, kind::Kind, repos::repo::Repo, groups::group::Group, repo_config::RepoConfig};


pub struct BranchRefs {
    pub branches: Vec<Event>,
    pub pull_requests: Vec<Event>,
    pub merges: Vec<Event>,
    pub groups: Vec<Event>,
    repo_dir_path: PathBuf,
    pub most_recent_timestamp: Timestamp,
}

impl BranchRefs {
    pub fn new (branch_events: Vec<Event>, repo_dir_path: PathBuf) -> Self {
        let mut refs = Self {
            branches: vec![],
            pull_requests: vec![],
            merges: vec![],
            groups: vec![],
            repo_dir_path,
            most_recent_timestamp: Timestamp::from(0),
        };

        // add repo first branch in branches vector
        refs.update(
            load_event(refs.repo_dir_path.join(".ngit/repo.json"))
                .expect("repo.json to be present and load as event")
        );

        //load locally
        for dir_name in [
            "groups",
            "branches",
            "merges",
            "prs",
        ] {
            let dir_path = refs.repo_dir_path.join(".ngit").join(&dir_name);
            if dir_path.exists() {
                let dir = fs::read_dir(&dir_path)
                    .expect("read_dir to produce ReadDir from a path that exists");
                for entry in dir {
                    let path = entry
                        .expect("DirEntry to return from ReadDir")
                        .path();
                    // load each BranchRef event in .ngit and call update
                    refs.update(
                        load_event(path)
                            .expect("every file in .ngit paths is a valid json event")
                    );
                }
            }
            else {
                panic!("expected dir to exist in branch_refs");
            }
        }
        refs.updates(branch_events);
        refs
    }

    pub fn updates (&mut self, branch_events: Vec<Event>) {
        for event in branch_events.clone().into_iter() {
            self.update(event);
        }
        let mut repo_config = RepoConfig::open(&self.repo_dir_path);
        repo_config.set_last_branch_ref_update_time(self.most_recent_timestamp.clone());
    }

    pub fn update (&mut self, event: Event) {
        let event_to_store = event.clone();
        // /// check event is for repo
        // fn event_is_for_repo(event: &Event,branch_refs: &BranchRefs) -> bool {
        //     match event.tags.iter().find(|tag| tag_is_repo(tag)) {
        //         None => false,
        //         Some(tag) => {
        //             match branch_refs.branches.get(0) {
        //                 None => true, // current repo unknown
        //                 Some(b) => tag_extract_value(tag) == b.id.to_string(),
        //             }
        //         },
        //     }
        // }

        // update most_recent_timestamp
        if event.created_at > self.most_recent_timestamp {
            self.most_recent_timestamp = event.created_at.clone();
        }
        
        // add events to vectors
        let dir_name = match Kind::from(event.clone().kind.as_u64()) {
            Kind::InitializeRepo => {
                // if !self.branches.iter().any(|e| e.id == event.id)
                // && event_is_for_repo(&event, &self) {
                if !self.branches.iter().any(|e| e.id == event.id) {
                    self.branches.push(event);
                    Some("branches")
                }
                else { None }
            },
            Kind::InitializeBranch => {
                // if !self.branches.iter().any(|e| e.id == event.id)
                // && event_is_for_repo(&event, &self) {
                if !self.branches.iter().any(|e| e.id == event.id) {
                    self.branches.push(event);
                    Some("branches")
                }
                else { None }
            },
            Kind::PullRequest => {
                // if !self.pull_requests.iter().any(|e| e.id == event.id)
                // && event_is_for_repo(&event, &self) {
                if !self.pull_requests.iter().any(|e| e.id == event.id) {
                    self.pull_requests.push(event);
                    Some("prs")
                }
                else { None }
            }
            Kind::Merge => {
                // if !self.merges.iter().any(|e| e.id == event.id)
                // && event_is_for_repo(&event, &self) {
                if !self.merges.iter().any(|e| e.id == event.id) {
                    self.merges.push(event);
                    Some("merges")
                }
                else { None }
            },
            Kind::InitializeGroup => {
                if !self.groups.iter().any(|e| e.id == event.id) {
                    self.groups.push(event);
                    Some("groups")
                }
                else { None }
            },
            _ => None,
        };

        // store events in .ngit directory
        match dir_name {
            Some(dir_name) => {
                let path = self.repo_dir_path.join(".ngit").join(format!("{}/{}.json",dir_name, event_to_store.id));
                if !path.exists() {
                    save_event(&path, &event_to_store)
                    .expect(format!("save_event will store BranchRefs event in {}",&path.to_string_lossy()).as_str());
                }
            },
            None => (),
        }
    }

    fn branch_event(&self, branch_id: Option<&String>) -> Event {
        match branch_id {
            None => self.branches[0].clone(),
            Some(branch_id) => self.branches.iter().find(|b| b.id.to_string() == *branch_id)
                .expect("BranchRefs.branch_event() will always be called with a branch_id from a branch in its cache")
                .clone(),
        }
    }

    pub fn branch_as_repo(&self, branch_id: Option<&String>) -> Repo {
        Repo::new_from_event(self.branch_event(branch_id))
            .expect("event in BranchRefs.branches to produce Repo")
    }

    /// assumes the branch_id is in cachse
    pub fn maintainers_group_id(&self, branch_id: Option<&String>) -> EventId {
        self.branch_as_repo(branch_id)
            .maintainers_group.get_first_active_group()
            .expect("a repo will always have an active maintainers group")
            .clone()
    }

    /// assumes the branch_id is in cache. returns None if maintainers group event cannot be found.
    pub fn maintainers_group(&self, branch_or_group_id: Option<&String>) -> Option<Group> {
        match self.groups.iter().find(|g|
            // for branch id
            g.id == self.maintainers_group_id(branch_or_group_id)
            // for group id
            || match branch_or_group_id {
                None => false,
                Some(id) => g.id == EventId::from_str(id).expect("id to be valid event id"),
            },
        ) {
            None => None,
            Some(event) => Some(
                Group::new_from_event(event.clone())
                .expect("group stored in BranchRefs.groups will always produce Group")
            ),
        }
    }

    /// returns None if maintainers group event cannot be found
    pub fn is_authorized(&self, branch_id: Option<&String>, pubkey: &XOnlyPublicKey) -> Option<bool> {
        match self.maintainers_group(branch_id) {
            None => None,
            Some(group) => Some(
                group.is_member(pubkey)
                // TODO - add support for nested groups so 'is_member' checks for indirect membership
                // for it will just be members of the branch group or maintainers group
                ||
                match self.maintainers_group(None) {
                    None => false,
                    Some(group) => group.is_member(pubkey),
                }
            ),
        }
    }

    pub fn group_ids_for_branches_without_cached_groups(&self) -> Vec<EventId> {
        self.branches.iter()
            .map(|b|
                self.maintainers_group_id(Some(&b.id.to_string()))
                    .clone()
            )
            .filter(|id|!self.groups.iter().any(|e|e.id == *id))
            .collect()

    }
}

pub fn get_branch_refs (repo: &Repo, client: &Client, repo_dir_path: &PathBuf) -> BranchRefs {

    let mut refs = BranchRefs::new(vec![],repo_dir_path.clone());

    let repo_config = RepoConfig::open(repo_dir_path);

    // filter for branches, PRs and Merges
    let mut tag_filter = Filter::new()
        .event(repo.id)
        .kinds(vec![
            Kind::InitializeBranch.into_sdk_custom_kind(),
            Kind::PullRequest.into_sdk_custom_kind(),
            Kind::Merge.into_sdk_custom_kind(),
        ]);
    match repo_config.last_branch_ref_update_time() {
        None => (),
        Some(timestamp) => {
            tag_filter = tag_filter.since(timestamp.clone());
        }
    };

    let branch_events: Vec<Event> = client.get_events_of(
        vec![
            // branch maintainer groups
            Filter::new().ids(refs.group_ids_for_branches_without_cached_groups()),
            tag_filter,
        ],
        None,
    )
        .expect("get_events_of to not return an error");

    refs.updates(branch_events);
    // refs.merged_branches_ids.push(repo.id.to_string());

    // for event in refs.merges.iter() {
    //     match &event.tags.iter().find(|t|tag_is_branch(t)) {
    //         None => {},
    //         Some(t) => {
    //             match &refs.maintainers_group {
    //                 None => (),
    //                 Some(g) => {
    //                     if g.is_member(&event.pubkey) {
    //                         refs.merged_branches_ids.push(tag_extract_value(t));
    //                     }
    //                 }
    //             }
    //         }
    //     }
    // }
    refs
}
