use anyhow::{Result, bail};
use ngit::login::credential_store;

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// credential entry name printed at logout
    #[arg(value_name = "ENTRY")]
    pub entry: String,
}

pub fn launch(args: &SubCommandArgs) -> Result<()> {
    if credential_store::parse_pointer(&args.entry).is_none() {
        bail!(
            "'{}' is not a credential entry name; expected the npub-based entry name printed at logout",
            args.entry
        );
    }
    if credential_store::forget(&args.entry)? {
        eprintln!(
            "removed stored secret '{}'; any login still pointing at it will need `ngit account login` again",
            args.entry
        );
    } else {
        eprintln!("no stored secret found for '{}'", args.entry);
    }
    Ok(())
}
