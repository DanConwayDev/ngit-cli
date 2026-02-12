use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ngit::login::SignerInfo;

use crate::sub_commands;

#[derive(Parser)]
#[command(
    author,
    version,
    help_template = "{name} {version}\nnostr plugin for git\n - clone a nostr repository, or add as a remote, by using the url format nostr://pub123/identifier\n - remote branches beginning with `pr/` are open PRs from contributors; `ngit list` can be used to view all PRs\n - to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options\n- publish a repository to nostr with `ngit init`\n\n{usage}\n{all-args}"
)]
#[command(propagate_version = true)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// remote signer address
    #[arg(long, global = true, hide = true)]
    pub bunker_uri: Option<String>,
    /// remote signer app secret key
    #[arg(long, global = true, hide = true)]
    pub bunker_app_key: Option<String>,
    /// nsec or hex private key
    #[arg(short, long, global = true)]
    pub nsec: Option<String>,
    /// password to decrypt nsec
    #[arg(short, long, global = true, hide = true)]
    pub password: Option<String>,
    /// disable spinner animations
    #[arg(long, action, hide = true)]
    pub disable_cli_spinners: bool,
    /// show customization options via git config
    #[arg(short, long, global = true)]
    pub customize: bool,
    /// Use default values without prompting (non-interactive mode)
    #[arg(short = 'd', long, global = true, conflicts_with = "interactive")]
    pub defaults: bool,
    /// Enable interactive prompts (default behavior)
    #[arg(short = 'i', long, global = true)]
    pub interactive: bool,
    /// Force operations, bypass safety guards
    #[arg(short = 'f', long, global = true)]
    pub force: bool,
}

pub const CUSTOMISE_TEMPLATE: &str = r"
==========================
      Customize ngit      
==========================
ngit settings are managed through the git config.

Currently the only settings not reachable through standard commands relate to default hardcoded relays:

 - nostr.grasp-default-set - only used during `ngit init`
 - nostr.relay-default-set      - used for profile discovery and account bootstrapping
 - nostr.relay-blaster-set      - only used for repo announcement events 
 - nostr.relay-signer-fallback-set

These take a string of semi-colon separated websocket URLs without spaces. For example:
`git config --global nostr.relay-default-set 'wss://relay1.example.com;wss://relay2.example.com'`
Or just for this repository:
`git config nostr.relay-default-set 'wss://relay1.example.com;wss://relay2.example.com'`

Other useful settings:
 - 'nostr.nostate true' to avoid publishing a state event when pushing to a nostr remote.
 - Login settings configured during `ngit account login`:
   - nostr.nsec - nsec or ncryptsec
   - nostr.npub - used for ncryptsec and remote signer
   - nostr.bunker-uri - used for remote signer
   - nostr.bunker-app-key - used for remote signer

Other config settings are applied to the local repository but just for effiency reasons eg nostr.nip05 and nostr.protocol-push
==========================
";

pub fn extract_signer_cli_arguments(args: &Cli) -> Result<Option<SignerInfo>> {
    if let Some(nsec) = &args.nsec {
        Ok(Some(SignerInfo::Nsec {
            nsec: nsec.to_string(),
            password: None,
            npub: None,
        }))
    } else if let Some(bunker_uri) = args.bunker_uri.clone() {
        if let Some(bunker_app_key) = args.bunker_app_key.clone() {
            Ok(Some(SignerInfo::Bunker {
                bunker_uri,
                bunker_app_key,
                npub: None,
            }))
        } else {
            bail!("cli argument bunker-app-key must be supplied when bunker-uri is")
        }
    } else if args.bunker_app_key.is_some() {
        bail!("cli argument bunker-uri must be supplied when bunker-app-key is")
    } else {
        Ok(None)
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// signal you are this repo's maintainer accepting PRs and issues via nostr
    Init(sub_commands::init::SubCommandArgs),
    /// submit PR with advanced options
    Send(sub_commands::send::SubCommandArgs),
    /// list PRs; checkout, apply or download selected
    List,
    /// checkout a proposal branch by event-id or nevent
    Checkout {
        /// Proposal event-id (hex) or nevent (bech32)
        id: String,
    },
    /// update repo git servers to reflect nostr state (add, update or delete
    /// remote refs)
    Sync(sub_commands::sync::SubCommandArgs),
    /// login, logout or export keys
    Account(AccountSubCommandArgs),
}

#[derive(Subcommand)]
pub enum AccountCommands {
    /// login with nsec or nostr connect
    Login(sub_commands::login::SubCommandArgs),
    /// remove nostr account details stored in git config
    Logout,
    /// export nostr keys to login to other nostr clients
    ExportKeys,
    /// create a new nostr account
    Create(sub_commands::create::SubCommandArgs),
}

#[derive(clap::Parser)]
pub struct AccountSubCommandArgs {
    #[command(subcommand)]
    pub account_command: AccountCommands,
}
