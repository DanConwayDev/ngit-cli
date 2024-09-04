pub mod cli_interactor;
pub mod client;
pub mod git;
pub mod git_events;
pub mod login;
pub mod repo_ref;
pub mod repo_state;

use anyhow::{anyhow, Result};
use directories::ProjectDirs;

pub fn get_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "ngit").ok_or(anyhow!(
        "should find operating system home directories with rust-directories crate"
    ))
}
