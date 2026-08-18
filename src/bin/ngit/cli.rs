use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use console::style;
use ngit::login::SignerInfo;

use crate::sub_commands;

#[derive(Clone, Copy)]
pub struct SignerParams<'a> {
    pub info: &'a Option<SignerInfo>,
    pub password: &'a Option<String>,
}

#[derive(Parser)]
#[command(
    author,
    version,
    help_template = "{name} {version}\nnostr plugin for git\n includes a remote helper so native git commands (clone, fetch, push) work with nostr:// URLs\n - clone a nostr repository, or add as a remote, by using the url format nostr://npub123/identifier\n - remote branches beginning with `pr/` are open PRs from contributors; `ngit pr list` can be used to view all PRs\n - to open a PR, push a branch with the prefix `pr/` or use `ngit send` for advanced options\n   set title and description via push options:\n     git push -o 'title=My PR' -o 'description=line1\\n\\nline2' -u origin pr/branch\n   target another branch or select an explicit proposal base with:\n     git push -o target-branch=release/2.x -o base=<commit|branch|nevent> -u origin pr/branch\n - publish a repository to nostr with `ngit init`\n\n{usage}\n{all-args}"
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
    #[arg(
        short,
        long,
        global = true,
        conflicts_with_all = ["nsec_file", "signer"]
    )]
    pub nsec: Option<String>,
    /// read an nsec or hex private key from a path resolving to a regular file
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with_all = ["nsec", "signer"]
    )]
    pub nsec_file: Option<PathBuf>,
    /// use a configured signer by npub, alias, or cached profile name for
    /// this command
    #[arg(
        long,
        global = true,
        value_name = "NPUB|ALIAS|NAME",
        conflicts_with_all = ["nsec", "nsec_file", "bunker_uri", "bunker_app_key"]
    )]
    pub signer: Option<String>,
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
    /// Target repository for repo-scoped operations. Accepts a configured
    /// nostr:// remote name, an naddr, or a nostr:// URL. Overrides
    /// `nostr.repo`, tracked-upstream, and remote-based auto-detection.
    ///
    /// Available at any command position:
    ///   `ngit --repo upstream send`
    ///   `ngit issue --repo upstream create`
    ///   `ngit issue create --repo upstream`
    #[arg(long, global = true, value_name = "REMOTE|NADDR|NOSTR-URL")]
    pub repo: Option<String>,
    /// Output one machine-readable JSON document on stdout
    #[arg(long, global = true, conflicts_with = "interactive")]
    pub json: bool,
}

#[allow(clippy::too_many_lines)]
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

Most ngit settings are managed through git config. Where an environment
variable is listed alongside a git config key, it overrides git config; local
git config overrides global git config; built-in defaults are used last.

{cache_storage}

  {cache_dir}
    Overrides the platform-specific directory used for ngit's global event
    cache. Repository caches remain in the Git common directory.

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

  {auto_pr_branches}
    Set to false to stop advertising every open or draft PR as a `pr/*` branch.
    Defaults to true; local config overrides global config. `ngit pr checkout`
    still creates and tracks the selected PR branch so `git fetch` and
    `git pull` keep it current. Use `git fetch --prune` to remove branches
    fetched before disabling this setting.

  {trust_server_domains}
    Semicolon-separated git-server hostnames that `ngit sync` should trust when
    they are fast-forward ahead of nostr state, without `--trust-server`.
    Example: {trust_server_example}

  {skill_reminders}
    Set to false to disable repository skill setup and update reminders.
    For one repository, run: {skill_opt_out}

  {http_connect_timeout:<39} {http_connect_timeout_env:<32}
    HTTP connect timeout for libgit2 fetch/push operations in milliseconds.
    Default: {http_connect_timeout_default}. Example: {http_connect_timeout_example}

  {http_io_timeout:<39} {http_io_timeout_env:<32}
    Per-socket send/recv timeout for libgit2 fetch/push operations in milliseconds.
    Raise this for large pushes to GRASP servers that may be silent while indexing.
    Default: {http_io_timeout_default}. Example: {http_io_timeout_example}

{login_settings}

  These are configured by {login_cmd}:

  {signer_selection:<27} selected npub or alias
  {signer_alias:<27} alias-to-npub mapping, for example `.fred`
  {nsec:<27} credential-store entry name, or a plaintext nsec / ncryptsec
  {npub:<27} used for ncryptsec and remote signer
  {bunker_uri:<27} used for remote signer
  {bunker_app_key:<27} credential-store entry name used for remote signer
  {secret_storage:<27} auto | file | git-config

  Use {signer_flag} to select one configured signer without changing the
  current profile. Secrets normally live in the OS credential store, or in ngit's file store
  when no OS store is available. The value of nostr.nsec or
  nostr.bunker-app-key is then the entry name (the key's npub) under
  keyring service `nostr`;
  plaintext nsec1… values are also accepted. Override where login stores
  secrets with {secret_storage_env} or `ngit account login --secret-storage`.

Other repository-local config keys, such as {nip05} and {protocol_push}, are
implementation details used for efficiency.
",
        cache_storage = section("Cache storage"),
        cache_dir = env("NGIT_CACHE_DIR"),
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
        auto_pr_branches = key("nostr.auto-pr-branches false"),
        trust_server_domains = key("nostr.trust-server-domains"),
        trust_server_example =
            cmd("git config --global nostr.trust-server-domains 'github.com;codeberg.org'"),
        skill_reminders = key("nostr.skill-reminders true"),
        skill_opt_out = cmd("ngit skill opt-out --local"),
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
        signer_selection = key("nostr.signer"),
        signer_alias = key("nostr.signer-alias.<alias>"),
        signer_flag = cmd("ngit --signer <alias|npub|nostr-display-name> <command>"),
        nsec = key("nostr.nsec"),
        npub = key("nostr.npub"),
        bunker_uri = key("nostr.bunker-uri"),
        bunker_app_key = key("nostr.bunker-app-key"),
        secret_storage = key("nostr.secret-storage"),
        secret_storage_env = env("NGIT_SECRET_STORAGE"),
        nip05 = key("nostr.nip05"),
        protocol_push = key("nostr.protocol-push"),
    )
}

pub fn extract_signer_cli_arguments(args: &Cli) -> Result<Option<SignerInfo>> {
    let nsec = if let Some(nsec) = &args.nsec {
        Some(nsec.clone())
    } else if let Some(path) = &args.nsec_file {
        Some(read_nsec_file(path)?)
    } else {
        None
    };
    if let Some(nsec) = nsec {
        Ok(Some(SignerInfo::Nsec {
            nsec,
            password: None,
            npub: None,
            verify_npub: false,
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
    } else if let Some(selector) = &args.signer {
        Ok(Some(SignerInfo::Selection {
            selector: selector.clone(),
        }))
    } else {
        Ok(None)
    }
}

fn read_nsec_file(path: &Path) -> Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).context("failed to open nsec file")?;
    let meta = file
        .metadata()
        .context("failed to inspect open nsec file")?;
    if !meta.is_file() {
        bail!("nsec file path must resolve to a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !matches!(meta.permissions().mode() & 0o777, 0o400 | 0o600) {
            bail!("nsec file permissions must be 0400 or 0600");
        }
    }
    if !(1..=4096).contains(&meta.len()) {
        bail!("nsec file must contain 1 to 4096 bytes");
    }
    let capacity = usize::try_from(meta.len()).context("nsec file size does not fit memory")?;
    let mut raw = Vec::with_capacity(capacity);
    file.take(4097)
        .read_to_end(&mut raw)
        .context("failed to read nsec file")?;
    if raw.len() > 4096 {
        bail!("nsec file must contain 1 to 4096 bytes");
    }
    let value = std::str::from_utf8(&raw).context("nsec file must be UTF-8")?;
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value);
    if value.is_empty() || value.contains(['\r', '\n']) {
        bail!("nsec file must contain exactly one non-empty line");
    }
    Ok(value.to_string())
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
    /// merge a PR into its declared target, or the default branch, as a no-ff
    /// merge commit (does not push)
    #[command(
        long_about = "merge a PR into its declared target branch, or the repository default, as a no-ff merge commit (does not push)\n\nrun without an ID while on a `pr/` branch to merge that PR, or pass a PR event-id (hex) or nevent"
    )]
    Merge(MergeSubCommandArgs),
    /// work with issues
    Issue(IssueSubCommandArgs),
    /// update repo git servers to reflect nostr state (add, update or delete
    /// remote refs)
    Sync(sub_commands::sync::SubCommandArgs),
    /// install and update ngit's repository skill for coding agents
    Skill(SkillArgs),
    /// list accounts, create an account, login, logout or export keys
    Account(AccountSubCommandArgs),
}

#[derive(clap::Parser)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub skill_command: SkillCommands,
}

#[derive(Subcommand)]
pub enum SkillCommands {
    /// Install the bundled repository skill
    Install,
    /// Upgrade the repository skill to the version bundled with ngit
    Upgrade,
    /// Show installed and bundled skill versions without changing files
    Status,
    /// Disable repository skill reminders
    OptOut(SkillOptOutArgs),
}

#[derive(clap::Args)]
#[group(required = true, multiple = false)]
pub struct SkillOptOutArgs {
    /// Disable reminders in this repository
    #[arg(long)]
    pub local: bool,
    /// Disable reminders for all repositories by default
    #[arg(long)]
    pub global: bool,
}

#[derive(Subcommand)]
pub enum AccountCommands {
    /// show logged-in and other accounts available for direct use
    #[command(visible_alias = "list")]
    Whoami(sub_commands::whoami::SubCommandArgs),
    /// login with nsec or nostr connect
    Login(sub_commands::login::SubCommandArgs),
    /// connect interactively (alias for `login -i`)
    Connect(sub_commands::login::SubCommandArgs),
    /// remove nostr account details from git config; keeps the stored
    /// secret unless --forget is passed
    Logout(sub_commands::logout::SubCommandArgs),
    /// export nostr keys to login to other nostr clients
    ExportKeys,
    /// remove a stored account secret from the OS credential store / ngit
    /// file store
    ForgetKeys(sub_commands::forget_keys::SubCommandArgs),
    /// create a new nostr account
    Create(sub_commands::create::SubCommandArgs),
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
    use std::{fs, path::Path};

    use clap::{Command, CommandFactory, Parser};
    use tempfile::tempdir;

    use super::{AccountCommands, Cli, Commands, extract_signer_cli_arguments, read_nsec_file};

    fn assert_json_on_every_leaf(command: &Command, path: &str) {
        if command.has_subcommands() {
            for subcommand in command.get_subcommands() {
                if subcommand.get_name() == "help" {
                    continue;
                }
                assert_json_on_every_leaf(subcommand, &format!("{path} {}", subcommand.get_name()));
            }
            return;
        }

        assert!(
            command
                .get_arguments()
                .any(|argument| argument.get_id() == "json"),
            "{path} does not inherit --json"
        );
    }

    #[test]
    fn json_is_global_and_available_on_every_command() {
        let mut command = Cli::command();
        command.build();
        assert_json_on_every_leaf(&command, "ngit");

        for args in [
            ["ngit", "--json", "issue", "create"].as_slice(),
            ["ngit", "issue", "--json", "create"].as_slice(),
            ["ngit", "issue", "create", "--json"].as_slice(),
            ["ngit", "skill", "status", "--json"].as_slice(),
            ["ngit", "account", "logout", "--forget", "--json"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(args).unwrap().json,
                "failed for {args:?}"
            );
        }
    }

    fn key_file(path: &Path, value: &[u8]) {
        fs::write(path, value).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn nsec_file_parser_conflict_and_valid_line() {
        assert!(Cli::try_parse_from(["ngit", "--nsec", "x", "--nsec-file", "key"]).is_err());
        let dir = tempdir().unwrap();
        let path = dir.path().join("key");
        key_file(&path, b"fixture\n");
        let cli = Cli::try_parse_from(["ngit", "--nsec-file", path.to_str().unwrap()]).unwrap();
        assert!(
            matches!(extract_signer_cli_arguments(&cli).unwrap(), Some(ngit::login::SignerInfo::Nsec { nsec, .. }) if nsec == "fixture")
        );
    }

    #[test]
    fn signer_is_global_and_conflicts_with_direct_secrets() {
        for args in [
            ["ngit", "--signer", "fred", "issue", "create"].as_slice(),
            ["ngit", "issue", "create", "--signer", "fred"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(
                extract_signer_cli_arguments(&cli).unwrap(),
                Some(ngit::login::SignerInfo::Selection {
                    selector: "fred".to_string()
                })
            );
        }
        assert!(Cli::try_parse_from(["ngit", "--signer", "fred", "--nsec", "key"]).is_err());
        assert!(
            Cli::try_parse_from([
                "ngit",
                "--signer",
                "fred",
                "--bunker-uri",
                "bunker://example"
            ])
            .is_err()
        );
    }

    #[test]
    fn bunker_login_url_conflicts_with_other_signer_sources() {
        for conflicting in [
            ["--nsec", "key"],
            ["--nsec-file", "key"],
            ["--signer", "fred"],
            ["--bunker-uri", "bunker://example"],
            ["--bunker-app-key", "key"],
        ] {
            assert!(
                Cli::try_parse_from([
                    "ngit",
                    "account",
                    "login",
                    "--bunker-url",
                    "bunker://example",
                    conflicting[0],
                    conflicting[1],
                ])
                .is_err(),
                "--bunker-url accepted conflicting source {}",
                conflicting[0]
            );
        }
    }

    #[test]
    fn account_login_accepts_a_positional_selector() {
        let cli = Cli::try_parse_from([
            "ngit",
            "account",
            "login",
            "DanConwayDev",
            "--alias",
            "dcdev",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Account(args))
                if matches!(args.account_command, AccountCommands::Login(_))
        ));

        for conflicting in ["--signer", "--nsec", "--bunker-url"] {
            assert!(
                Cli::try_parse_from([
                    "ngit",
                    "account",
                    "login",
                    "DanConwayDev",
                    conflicting,
                    "value",
                ])
                .is_err(),
                "positional account accepted conflicting source {conflicting}"
            );
        }
    }

    #[test]
    fn account_list_is_an_alias_for_whoami() {
        for command in ["whoami", "list"] {
            let cli = Cli::try_parse_from(["ngit", "account", command, "--offline"]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Commands::Account(args))
                    if matches!(args.account_command, AccountCommands::Whoami(_))
            ));
        }
    }

    #[test]
    fn nsec_file_is_global_for_signing_commands() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("key");
        key_file(&path, b"fixture");
        let path = path.to_string_lossy().into_owned();

        for args in [
            vec![
                "ngit",
                "issue",
                "comment",
                "deadbeef",
                "--body",
                "body",
                "--nsec-file",
                &path,
            ],
            vec!["ngit", "--nsec-file", &path, "pr", "close", "deadbeef"],
            vec!["ngit", "sync", "--nsec-file", &path],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(
                matches!(extract_signer_cli_arguments(&cli).unwrap(), Some(ngit::login::SignerInfo::Nsec { nsec, .. }) if nsec == "fixture")
            );
        }
    }

    #[test]
    fn nsec_file_rejects_bad_content_without_echo() {
        let dir = tempdir().unwrap();
        for (name, value) in [
            ("empty", b"".as_slice()),
            ("multi", b"never-echo\nline\n"),
            ("large", vec![b'x'; 4097].leak()),
        ] {
            let path = dir.path().join(name);
            key_file(&path, value);
            let cli = Cli::try_parse_from(["ngit", "--nsec-file", path.to_str().unwrap()]).unwrap();
            let error = match extract_signer_cli_arguments(&cli) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("bad nsec file accepted"),
            };
            assert!(!error.contains("never-echo"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn nsec_file_accepts_symlink_and_validates_its_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        key_file(&target, b"fixture");
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();

        let cli = Cli::try_parse_from(["ngit", "--nsec-file", link.to_str().unwrap()]).unwrap();
        assert!(
            matches!(extract_signer_cli_arguments(&cli).unwrap(), Some(ngit::login::SignerInfo::Nsec { nsec, .. }) if nsec == "fixture")
        );

        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(extract_signer_cli_arguments(&cli).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn nsec_file_accepts_read_only_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("key");
        key_file(&path, b"fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();

        let cli = Cli::try_parse_from(["ngit", "--nsec-file", path.to_str().unwrap()]).unwrap();
        assert!(
            matches!(extract_signer_cli_arguments(&cli).unwrap(), Some(ngit::login::SignerInfo::Nsec { nsec, .. }) if nsec == "fixture")
        );
    }

    #[cfg(unix)]
    #[test]
    fn nsec_file_rejects_fifo_without_waiting_for_a_writer() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt, sync::mpsc, thread, time::Duration};

        let dir = tempdir().unwrap();
        let path = dir.path().join("key-pipe");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid, NUL-terminated path and the mode is a
        // conventional user-only permission mask.
        let status = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            status,
            0,
            "failed to create FIFO: {}",
            std::io::Error::last_os_error()
        );

        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || sender.send(read_nsec_file(&path)).unwrap());
        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("opening a FIFO blocked instead of rejecting it");
        handle.join().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn repo_arg_is_accepted_at_every_command_position() {
        // Documented policy: `--repo` is a global argument accepted at any
        // level. Regression-test all three placements shown in the spec.
        for args in [
            ["ngit", "--repo", "upstream", "issue", "create"].as_slice(),
            ["ngit", "issue", "--repo", "upstream", "create"].as_slice(),
            ["ngit", "issue", "create", "--repo", "upstream"].as_slice(),
            ["ngit", "--repo", "upstream", "send", "--defaults"].as_slice(),
            ["ngit", "send", "--repo", "upstream", "--defaults"].as_slice(),
            [
                "ngit", "pr", "comment", "deadbeef", "--body", "hi", "--repo", "upstream",
            ]
            .as_slice(),
        ] {
            let cli = Cli::try_parse_from(args)
                .unwrap_or_else(|e| panic!("failed to parse {args:?}: {e}"));
            assert_eq!(
                cli.repo.as_deref(),
                Some("upstream"),
                "--repo not captured for {args:?}"
            );
        }
    }

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
