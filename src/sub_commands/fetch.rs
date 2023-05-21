use clap::Args;
use nostr::Keys;

use crate::fetch_pull_push::fetch_pull_push;

#[derive(Args)]
pub struct FetchSubCommand {
}

pub fn fetch_from_relays(keys: Option<&Keys>) {

    fetch_pull_push(
        keys,
        false,
        false,
        None,
        false,
        None,
        None,
    );

}
