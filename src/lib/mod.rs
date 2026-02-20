pub mod accept_maintainership;
pub mod cli_interactor;
pub mod client;
pub mod fetch;
pub mod git;
pub mod git_events;
pub mod list;
pub mod login;
pub mod mbox_parser;
pub mod push;
pub mod repo_ref;
pub mod repo_state;
pub mod utils;

use anyhow::{Result, anyhow};
use directories::ProjectDirs;
use nostr_sdk::Url;

pub fn get_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "ngit").ok_or(anyhow!(
        "should find operating system home directories with rust-directories crate"
    ))
}

pub trait UrlWithoutSlash {
    fn as_str_without_trailing_slash(&self) -> &str;
    fn to_string_without_trailing_slash(&self) -> String;
}

impl UrlWithoutSlash for Url {
    fn as_str_without_trailing_slash(&self) -> &str {
        let url_str = self.as_str();
        if let Some(without) = url_str.strip_suffix('/') {
            without
        } else {
            url_str
        }
    }

    fn to_string_without_trailing_slash(&self) -> String {
        self.as_str_without_trailing_slash().to_string()
    }
}
