
use clap::{Args};

use crate::fetch_pull_push::fetch_pull_push;

#[derive(Args)]
struct PushRepo {
    /// Relays
    #[arg(short, long)]
    relays: Option<String>,
}

#[derive(Args)]
pub struct PushSubCommand {
}

pub fn push(
    _sub_command_args: &PushSubCommand,
) {
    
    fetch_pull_push(
        None,
        false,
        true,
        None,
        false,
        None,
        None,
    );

}
