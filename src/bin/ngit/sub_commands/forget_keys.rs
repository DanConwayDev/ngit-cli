use anyhow::{Result, bail};
use ngit::login::{credential_store, user};

#[derive(clap::Args)]
pub struct SubCommandArgs {
    /// npub to forget all saved credentials for an identity, or an exact
    /// entry name (signer:npub or signer-alias:name)
    #[arg(value_name = "ENTRY")]
    pub entry: String,
}

pub fn launch(args: &SubCommandArgs) -> Result<()> {
    if !credential_store::valid_entry_name(&args.entry) {
        bail!(
            "'{}' is not a credential entry name; use `account logout --profile NAME` for a named profile, or supply an npub, signer:npub, or signer-alias:name",
            args.entry
        );
    }
    let removed = if credential_store::parse_pointer(&args.entry) == Some(args.entry.as_str()) {
        credential_store::forget_account(&args.entry)?
    } else {
        credential_store::forget(&args.entry)?
    };
    if removed {
        eprintln!(
            "removed stored credentials for '{}'; any login still pointing at it will need `ngit account login` again",
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
