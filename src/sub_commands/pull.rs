

use clap::Args;
use nostr::{Keys};

use crate::{fetch_pull_push::fetch_pull_push};

#[derive(Args)]
pub struct PullSubCommand {
    /// branch nevent or hex to pull into a new local branch
    #[arg(short, long)]
    pub branch: Option<String>,
}

pub fn pull_from_relays(keys: Option<&Keys>, sub_command_args: &PullSubCommand) {

    fetch_pull_push(
        keys,
        true,
        false,
        sub_command_args.branch.clone(),
        false,
        None,
        None,
    );
}
