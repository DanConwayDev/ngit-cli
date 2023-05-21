

use std::fs;

use clap::Args;

use nostr::{Keys};

use crate::{cli_helpers::select_relays, config::load_config, utils::{create_client, load_event}};

#[derive(Args)]
pub struct RebroadcastSubCommand {
}

pub fn rebroadcast(
    _sub_command_args: &RebroadcastSubCommand,
) {

    // get relay input
    let relays = select_relays(
        &mut load_config(),
        &vec![],
    )
        .expect("relays to be selected");

    let client = create_client(&Keys::generate(), relays)
        .expect("create_client to return client for create_and_broadcast_patches");

    let repo_dir_path = std::env::current_dir().unwrap();

    // cycle through directories and send events
    for dir_name in [
        "groups",
        "branches",
        "patches",
        "merges",
        "prs",
        "issues",
        "comments",
    ] { 
        if !repo_dir_path.join(".ngit").exists() {
            println!("this isn't a repository here to rebroadcast")
        }
        let dir_path = repo_dir_path.join(".ngit").join(&dir_name);
        if dir_path.exists() {
            let dir = fs::read_dir(&dir_path)
                .expect("read_dir to produce ReadDir from a path that exists");
            // get json in directories
            for entry in dir {
                let path = entry
                    .expect("DirEntry to return from ReadDir")
                    .path();
                // send event
                match client.send_event(
                    load_event(&path)
                        .expect("every file in .ngit paths is a valid json event")
                ) {
                    Ok(_) => {
                        println!("sent: {}", &path.to_string_lossy());
                    },
                    // TODO: this isn't working - if a relay is specified with a type it will wait 30ish secs and then return successful
                    Err(e) => { println!("error broadcasting event: {}",e); },
                }
                // TODO: better error handling here / reporting. potentially warn if taking a while and report on troublesome relays
            }
        }
    }
}
