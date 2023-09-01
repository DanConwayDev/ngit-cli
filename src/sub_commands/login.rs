use anyhow::Result;
use clap;

use crate::{login, Cli};

#[derive(clap::Args)]
pub struct SubCommandArgs;

pub fn launch(args: &Cli, _command_args: &SubCommandArgs) -> Result<()> {
    login::launch(&args.nsec)
}
