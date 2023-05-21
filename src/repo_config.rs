use std::{path::PathBuf, fs::File, io::Write};

use nostr::Timestamp;
use serde::{Deserialize, Serialize};

use crate::utils::load_file;

#[derive(Serialize, Deserialize)]
pub struct RepoConfig {
    version: u8,
    branch_mappings: Vec<(String, String, Option<Timestamp>)>,
    last_branch_ref_update_time: Option<Timestamp>,
    repo_dir_path: PathBuf,
}


impl RepoConfig {

    pub fn open(repo_dir_path: &PathBuf) -> Self {
        let path = repo_dir_path.join(".ngit/config.json");
        if path.exists() {
            let repo_config: Self = serde_json::from_str(
                load_file(path)
                    .expect("config json to load from file")
                    .as_str()
            )
                .expect("config.json to deserialize into RepoConfig");
            repo_config
        }
        else {
            Self {
                version: 0,
                branch_mappings: vec![],
                last_branch_ref_update_time: None,
                repo_dir_path:repo_dir_path.clone(),

            }
        }
    }
    
    fn save(&self) {
        let path = self.repo_dir_path.join(".ngit/config.json");
        let mut f = File::create(path)
            .expect("config.json to open using File::Create");
        f.write_all(
            serde_json::json!(self).to_string().as_bytes()
        )
            .expect("write_all to write serialized RepoConfig to config.json");
    }

    pub fn set_mapping(&mut self, branch_name:&String, branch_id:&String) {
        for i in 0..self.branch_mappings.len() {
            if branch_name.clone() == self.branch_mappings[i].0 {
                self.branch_mappings[i].1 = branch_id.clone();
                self.save();
                return;
            }
        }
        self.branch_mappings.push(
            (branch_name.clone(), branch_id.clone(), None)
        );
        self.save();
    }

    pub fn set_last_branch_ref_update_time(&mut self, timestamp:Timestamp) {
        self.last_branch_ref_update_time = Some(timestamp);
        self.save();
    }

    pub fn last_branch_ref_update_time(&self) -> &Option<Timestamp> {
        &self.last_branch_ref_update_time
    }

    pub fn set_last_patch_update_time(&mut self, branch_id: String, timestamp:Timestamp) {
        for i in 0..self.branch_mappings.len() {
            if branch_id.clone() == self.branch_mappings[i].1 {
                self.branch_mappings[i].2 = Some(timestamp);
                self.save();
                return;
            }
        }
    }

    pub fn last_patch_update_time(&self, branch_id: String) -> &Option<Timestamp> {
        for mapping in self.branch_mappings.iter() {
            if branch_id.clone() == mapping.1 {
                return &mapping.2;
            }
        }
        return &None;
    }

    pub fn branch_id_from_name (&self,branch_name:&String) -> Option<&String> {
        for mapping in self.branch_mappings.iter() {
            if branch_name.clone() == mapping.0
                && self.check_local_branch_exists(&mapping.0)
            {
                return Some(&mapping.1);
            }
        }
        return None;
    }

    pub fn branch_name_from_id (&self,branch_id:&String) -> Option<&String> {
        for mapping in self.branch_mappings.iter() {
            if branch_id.clone() == mapping.1
                && self.check_local_branch_exists(&mapping.0)
            {
                return Some(&mapping.0);
            }
        }
        return None;
    }

    fn check_local_branch_exists(&self, branch_name: &String) -> bool {
        match git2::Repository::open(&self.repo_dir_path)
            .expect("git repo not initialized. run ngit init first")
            .find_branch(branch_name, git2::BranchType::Local)
        {
            Ok(_) => true,
            Err(_) => false,
        }
    }

}
