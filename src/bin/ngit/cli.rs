use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use console::style;
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
    /// Only publish nostr events to repository relays, not user or default
    /// relays
    #[arg(long, global = true)]
    pub repo_relay_only: bool,
}

pub fn customise_template() -> String {
    let title = style("Customize ngit").bold().cyan();
    let section = |text: &str| style(text.to_string()).bold().yellow();
    let key = |text: &str| style(text.to_string()).green();
    let env = |text: &str| style(text.to_string()).magenta();
    let cmd = |text: &str| style(text.to_string()).dim();

    format!(
        r"
{title}
==============

ngit settings are managed through git config. Where an environment variable is
listed, it overrides git config; local git config overrides global git config;
built-in defaults are used last.

{relay_defaults}

  {grasp:<39} {grasp_env:<32} only used during `ngit init`
  {relay:<39} {relay_env:<32} profile discovery and account bootstrapping
  {ann_indexer:<39} {ann_indexer_env:<32} repo announcement discovery
  {blaster:<39} {blaster_env:<32} repo announcement events only
  {signer:<39} {signer_env:<32} remote signer fallback relays

Values are semicolon-separated URLs without spaces.

  Global: {global_example}
  Local:  {local_example}
  Env:    {env_example}

{other_settings}

  {nostate}
    Avoid publishing a state event when pushing to a nostr remote.

  {repo_relay_only}
    Only publish nostr events to repo relays, skipping user and default relays.
    Useful when you do not want to broadcast to your personal relay set.
    Also available as: {repo_relay_only_flag}

  {trust_server_domains}
    Semicolon-separated git-server hostnames that `ngit sync` should trust when
    they are fast-forward ahead of nostr state, without `--trust-server`.
    Example: {trust_server_example}

  {http_connect_timeout:<39} {http_connect_timeout_env:<32}
    HTTP connect timeout for libgit2 fetch/push operations in milliseconds.
    Default: {http_connect_timeout_default}. Example: {http_connect_timeout_example}

  {http_io_timeout:<39} {http_io_timeout_env:<32}
    Per-socket send/recv timeout for libgit2 fetch/push operations in milliseconds.
    Raise this for large pushes to GRASP servers that may be silent while indexing.
    Default: {http_io_timeout_default}. Example: {http_io_timeout_example}

{login_settings}

  These are configured by {login_cmd}:

  {nsec:<27} nsec or ncryptsec
  {npub:<27} used for ncryptsec and remote signer
  {bunker_uri:<27} used for remote signer
  {bunker_app_key:<27} used for remote signer

Other repository-local config keys, such as {nip05} and {protocol_push}, are
implementation details used for efficiency.
",
        relay_defaults = section("Relay defaults"),
        grasp = key("nostr.grasp-default-set"),
        grasp_env = env("NGIT_GRASP_DEFAULT_SET"),
        relay = key("nostr.relay-default-set"),
        relay_env = env("NGIT_RELAY_DEFAULT_SET"),
        ann_indexer = key("nostr.relay-announcement-indexer-set"),
        ann_indexer_env = env("NGIT_RELAY_ANNOUNCEMENT_INDEXER_SET"),
        blaster = key("nostr.relay-blaster-set"),
        blaster_env = env("NGIT_RELAY_BLASTER_SET"),
        signer = key("nostr.relay-signer-fallback-set"),
        signer_env = env("NGIT_RELAY_SIGNER_FALLBACK_SET"),
        global_example = cmd("git config --global nostr.relay-default-set \
             'wss://relay1.example.com;wss://relay2.example.com'"),
        local_example = cmd("git config nostr.relay-default-set \
             'wss://relay1.example.com;wss://relay2.example.com'"),
        env_example = cmd(
            "NGIT_RELAY_DEFAULT_SET='wss://relay1.example.com;wss://relay2.example.com' ngit repo"
        ),
        other_settings = section("Other useful settings"),
        nostate = key("nostr.nostate true"),
        repo_relay_only = key("nostr.repo-relay-only true"),
        repo_relay_only_flag = cmd("ngit --repo-relay-only send"),
        trust_server_domains = key("nostr.trust-server-domains"),
        trust_server_example =
            cmd("git config --global nostr.trust-server-domains 'github.com;codeberg.org'"),
        http_connect_timeout = key("nostr.http-connect-timeout-ms"),
        http_connect_timeout_env = env("NGIT_HTTP_CONNECT_TIMEOUT_MS"),
        http_connect_timeout_default = key("3000"),
        http_connect_timeout_example = cmd("git config nostr.http-connect-timeout-ms 10000"),
        http_io_timeout = key("nostr.http-io-timeout-ms"),
        http_io_timeout_env = env("NGIT_HTTP_IO_TIMEOUT_MS"),
        http_io_timeout_default = key("15000"),
        http_io_timeout_example = cmd("git config nostr.http-io-timeout-ms 600000"),
        login_settings = section("Login settings"),
        login_cmd = cmd("ngit account login"),
        nsec = key("nostr.nsec"),
        npub = key("nostr.npub"),
        bunker_uri = key("nostr.bunker-uri"),
        bunker_app_key = key("nostr.bunker-app-key"),
        nip05 = key("nostr.nip05"),
        protocol_push = key("nostr.protocol-push"),
    )
}

pub fn extract_signer_cli_arguments(args: &Cli) -> Result<Option<SignerInfo>> {
    if let Some(nsec) = &args.nsec {
        Ok(Some(SignerInfo::Nsec {
            nsec: nsec.clone(),
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
    /// merge a PR into the default branch as a no-ff merge commit (does not
    /// push)
    #[command(
        long_about = "merge a PR into the default branch as a no-ff merge commit (does not push)\n\nrun without an ID while on a `pr/` branch to merge that PR, or pass a PR event-id (hex) or nevent"
    )]
    Merge(MergeSubCommandArgs),
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
    /// connect interactively (alias for `login -i`)
    Connect(sub_commands::login::SubCommandArgs),
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
    /// Output repository info as JSON; `is_nostr_repo` is false when not in a
    /// nostr repository
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

// ---------------------------------------------------------------------------
// Merge command
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct MergeSubCommandArgs {
    /// PR event-id (hex) or nevent; omit when on a `pr/` branch to merge that
    /// PR
    #[arg(value_name = "ID|nevent")]
    pub id: Option<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Omit the cover note / PR description from the merge commit body, leaving
    /// only the summary line and the PR nevent reference
    #[arg(long)]
    pub exclude_description: bool,
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
        /// Overwrite local branch even if it has diverged from the published
        /// proposal
        #[arg(long)]
        force: bool,
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
        /// Optional reason stored in event content
        #[arg(long)]
        reason: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// reopen a closed PR (author or maintainer only)
    Reopen {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Optional reason stored in event content
        #[arg(long)]
        reason: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// mark a draft PR as ready for review (author or maintainer only)
    Ready {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Optional reason stored in event content
        #[arg(long)]
        reason: Option<String>,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// convert a PR back to draft (author or maintainer only)
    Draft {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Optional reason stored in event content
        #[arg(long)]
        reason: Option<String>,
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
    /// set the subject/title of a PR (author or maintainer only)
    #[command(name = "set-subject")]
    SetSubject {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// New subject/title for the PR
        #[arg(long, alias = "title")]
        subject: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// set or update the cover note for a PR (author or maintainer only)
    ///
    /// A cover note is a markdown body that replaces the displayed description.
    /// nostr: mentions in --body are converted to q/p tags automatically.
    #[command(name = "set-cover-note")]
    SetCoverNote {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Markdown body for the cover note
        #[arg(long)]
        body: String,
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
        /// Include full comment thread when viewing a specific issue (requires
        /// ID)
        #[arg(long)]
        comments: bool,
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
        /// Issue subject/title
        #[arg(long, alias = "title")]
        subject: Option<String>,
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
        /// Optional reason stored in event content
        #[arg(long)]
        reason: Option<String>,
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
    /// set the subject/title of an issue (author or maintainer only)
    #[command(name = "set-subject")]
    SetSubject {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// New subject/title for the issue
        #[arg(long, alias = "title")]
        subject: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// set or update the cover note for an issue (author or maintainer only)
    ///
    /// A cover note is a markdown body that replaces the displayed description.
    /// nostr: mentions in --body are converted to q/p tags automatically.
    #[command(name = "set-cover-note")]
    SetCoverNote {
        /// Issue event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Markdown body for the cover note
        #[arg(long)]
        body: String,
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn repo_relay_only_is_accepted_by_event_publishing_commands() {
        for args in [
            ["ngit", "send", "--repo-relay-only", "--defaults"].as_slice(),
            [
                "ngit",
                "pr",
                "comment",
                "deadbeef",
                "--body",
                "comment",
                "--repo-relay-only",
            ]
            .as_slice(),
            [
                "ngit",
                "issue",
                "create",
                "--repo-relay-only",
                "--subject",
                "issue",
                "--body",
                "body",
            ]
            .as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("command should parse");
            assert!(cli.repo_relay_only);
        }
    }
}
