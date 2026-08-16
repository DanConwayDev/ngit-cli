use anyhow::{Result, bail};
use ngit::login::{credential_store, user};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// credential entry name printed at logout
    #[arg(value_name = "ENTRY")]
    pub entry: String,
}

pub fn launch(args: &SubCommandArgs) -> Result<()> {
    if !credential_store::valid_entry_name(&args.entry) {
        bail!(
            "'{}' is not a credential entry name; expected the entry name printed at logout",
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
    // Cached decrypted relay lists must not outlive the forgotten secret, but
    // a failed cache wipe must not block the removal itself.
    if let Err(error) = user::wipe_private_git_relay_list_cache() {
        eprintln!("warning: failed to remove cached decrypted private relay lists: {error:#}");
    }
    Ok(())
}
