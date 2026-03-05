use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ngit::login::SignerInfo;

use crate::sub_commands;

#[derive(Parser)]
#[command(
    author,
    version,
    help_template = "{name} {version}\nnostr plugin for git\n includes a remote helper so native git commands (clone, fetch, push) work with nostr:// URLs\n - clone a nostr repository, or add as a remote, by using the url format nostr://npub123/identifier\n - remote branches beginning with `pr/` are open PRs from contributors; `ngit pr list` can be used to view all PRs\n - to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options\n   set title and description via push options:\n     git push -o 'title=My PR' -o 'description=line1\\n\\nline2' -u origin pr/branch\n - publish a repository to nostr with `ngit init`\n\n{usage}\n{all-args}"
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
    /// Enable verbose output
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
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
 - 'nostr.repo-relay-only true' to only publish nostr events to repo relays, skipping user and
   default relays. Useful for repositories where you don't want to broadcast to your personal
   relay set. Set via `git config nostr.repo-relay-only true` or `ngit init --repo-relay-only`.
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
    /// publish a repository to nostr; signal you are its maintainer accepting
    /// PRs and issues
    Init(sub_commands::init::SubCommandArgs),
    /// manage repository metadata and maintainership
    #[command(
        long_about = "manage repository metadata and maintainership\n\nrun without a subcommand to show repository info"
    )]
    Repo(RepoSubCommandArgs),
    /// submit PR with advanced options
    #[command(
        long_about = "submit PR with advanced options\n\nfor a simpler flow, push a branch with the `pr/` prefix using native git:\n  git push -o 'title=My PR' -o 'description=details here' -u origin pr/my-branch"
    )]
    Send(sub_commands::send::SubCommandArgs),
    /// work with pull requests
    #[command(
        long_about = "work with pull requests\n\nPRs are created by pushing a branch with the `pr/` prefix:\n  git push -u origin pr/my-branch\nor with advanced options via `ngit send`"
    )]
    Pr(PrSubCommandArgs),
    /// work with issues
    Issue(IssueSubCommandArgs),
    /// update repo git servers to reflect nostr state (add, update or delete
    /// remote refs)
    Sync(sub_commands::sync::SubCommandArgs),
    /// create account, login, logout or export keys
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
    /// show currently logged-in account(s)
    Whoami(sub_commands::whoami::SubCommandArgs),
}

#[derive(clap::Parser)]
pub struct AccountSubCommandArgs {
    #[command(subcommand)]
    pub account_command: AccountCommands,
}

#[derive(clap::Parser)]
pub struct RepoSubCommandArgs {
    #[command(subcommand)]
    pub repo_command: Option<RepoCommands>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output repository info as JSON; `is_nostr_repo` is false when not in a nostr repository
    #[arg(long)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// PR subcommand group
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct PrSubCommandArgs {
    #[command(subcommand)]
    pub pr_command: PrCommands,
}

#[derive(Subcommand)]
pub enum PrCommands {
    /// list PRs and view details
    List {
        /// Filter by status (comma-separated: open,draft,closed,applied)
        #[arg(long, default_value = "open,draft")]
        status: String,
        /// Filter by label (repeatable, OR logic: --label bug --label
        /// help-wanted)
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show details for specific proposal (event-id or nevent)
        #[arg(value_name = "ID|nevent")]
        id: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// view a PR; use --comments to include comment thread
    View {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Include full comment thread (default: show count only)
        #[arg(long)]
        comments: bool,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// checkout a proposal branch by event-id or nevent
    #[command(
        long_about = "checkout a proposal branch by event-id or nevent\n\nuse `ngit pr list` to find proposal IDs"
    )]
    Checkout {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// apply proposal patches to current branch
    #[command(
        long_about = "apply proposal patches to current branch\n\nuse `ngit pr list` to find proposal IDs"
    )]
    Apply {
        /// Proposal event-id or nevent
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Output patches to stdout instead of applying
        #[arg(long)]
        stdout: bool,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// submit PR with advanced options (alias for `ngit send`)
    #[command(
        long_about = "submit PR with advanced options\n\nfor a simpler flow, push a branch with the `pr/` prefix using native git:\n  git push -o 'title=My PR' -o 'description=details here' -u origin pr/my-branch"
    )]
    Send(sub_commands::send::SubCommandArgs),
    /// close a PR (author or maintainer only)
    Close {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// reopen a closed PR (author or maintainer only)
    Reopen {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// mark a draft PR as ready for review (author or maintainer only)
    Ready {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// convert a PR back to draft (author or maintainer only)
    Draft {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// add a comment to a PR
    Comment {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Comment body
        #[arg(long)]
        body: String,
        /// Reply to a specific comment event-id (hex) or nevent (bech32);
        /// defaults to top-level
        #[arg(long, value_name = "ID|nevent")]
        reply_to: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// merge a PR into the current branch (maintainer only)
    #[command(
        long_about = "merge a PR into the current branch (maintainer only)\n\nperforms a git merge of the PR branch; push afterwards to update the nostr state"
    )]
    Merge {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use squash merge
        #[arg(long)]
        squash: bool,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// add one or more labels to a PR (author or maintainer only)
    Label {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Label to apply (repeatable: --label bug --label help-wanted)
        #[arg(long = "label", value_name = "LABEL", required = true)]
        labels: Vec<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
}

// ---------------------------------------------------------------------------
// Issue subcommand group
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct IssueSubCommandArgs {
    #[command(subcommand)]
    pub issue_command: IssueCommands,
}

#[derive(Subcommand)]
pub enum IssueCommands {
    /// list issues and their statuses
    List {
        /// Filter by status (comma-separated: open,draft,closed,applied)
        #[arg(long, default_value = "open")]
        status: String,
        /// Filter by label (repeatable, OR logic: --label bug --label
        /// help-wanted)
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show details for a specific issue (event-id or nevent)
        #[arg(value_name = "ID|nevent")]
        id: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// view an issue; use --comments to include comment thread
    View {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Include full comment thread (default: show count only)
        #[arg(long)]
        comments: bool,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// create a new issue
    Create {
        /// Issue title
        #[arg(long)]
        title: Option<String>,
        /// Issue body / description
        #[arg(long)]
        body: Option<String>,
        /// Labels to apply (repeatable: --label bug --label help-wanted)
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
    },
    /// close an issue without resolving it (author or maintainer only)
    Close {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Optional reason (e.g. wontfix, duplicate, invalid)
        #[arg(long)]
        reason: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// mark an issue as resolved (author or maintainer only)
    #[command(
        long_about = "mark an issue as resolved (author or maintainer only)\n\nuse this when the issue has been fixed or addressed, as distinct from closing without resolution"
    )]
    Resolved {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Optional reason or resolution summary
        #[arg(long)]
        reason: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// reopen a closed issue (author or maintainer only)
    Reopen {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// add a comment to an issue
    Comment {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Comment body
        #[arg(long)]
        body: String,
        /// Reply to a specific comment event-id (hex) or nevent (bech32);
        /// defaults to top-level
        #[arg(long, value_name = "ID|nevent")]
        reply_to: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// add one or more labels to an issue (author or maintainer only)
    Label {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Label to apply (repeatable: --label bug --label help-wanted)
        #[arg(long = "label", value_name = "LABEL", required = true)]
        labels: Vec<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
}

#[derive(Subcommand)]
pub enum RepoCommands {
    /// publish a repository to nostr (alias for `ngit init`)
    Init(sub_commands::init::SubCommandArgs),
    /// update repository metadata on nostr
    #[command(
        long_about = "update repository metadata on nostr\n\nlike `ngit init` but makes clear you are editing an existing repository"
    )]
    Edit(sub_commands::init::SubCommandArgs),
    /// accept an invitation to co-maintain a repository
    #[command(long_about = "accept an invitation to co-maintain a repository\n\n\
            publishes your repository announcement to nostr, confirming your co-maintainership.\n\n\
            This is required because your signed announcement is what ties your git state events\n\
            to a specific repository coordinate chain, preventing scammers from attributing your\n\
            commits to a fake repository. See `ngit repo info` for details on the maintainer model.")]
    Accept(sub_commands::repo::accept::SubCommandArgs),
}
