use anyhow::Result;
use clap;

use crate::{login, Cli};

#[derive(clap::Args)]
pub struct SubCommandArgs;

pub fn launch(args: &Cli, _command_args: &SubCommandArgs) -> Result<()> {
    let _ = login::launch(&args.nsec, &args.password)?;
    Ok(())
}
