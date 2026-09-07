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
        conflicts_with_all = ["nsec_file", "nbunksec", "nbunksec_file", "signer"]
    )]
    pub nsec: Option<String>,
    /// read an nsec or hex private key from a path resolving to a regular file
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with_all = ["nsec", "nbunksec", "nbunksec_file", "signer"]
    )]
    pub nsec_file: Option<PathBuf>,
    /// established remote signer connection encoded as nbunksec
    #[arg(
        long,
        global = true,
        value_name = "NBUNKSEC",
        conflicts_with_all = [
            "nsec",
            "nsec_file",
            "nbunksec_file",
            "signer",
            "bunker_uri",
            "bunker_app_key"
        ]
    )]
    pub nbunksec: Option<String>,
    /// read an nbunksec connection from a path resolving to a regular file
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with_all = [
            "nsec",
            "nsec_file",
            "nbunksec",
            "signer",
            "bunker_uri",
            "bunker_app_key"
        ]
    )]
    pub nbunksec_file: Option<PathBuf>,
    /// use a configured signer by npub, alias, or cached profile name for
    /// this command
    #[arg(
        long,
        global = true,
        value_name = "NPUB|ALIAS|NAME",
        conflicts_with_all = [
            "nsec",
            "nsec_file",
            "nbunksec",
            "nbunksec_file",
            "bunker_uri",
            "bunker_app_key"
        ]
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
    /// Suppress progress and other non-essential output
    #[arg(short = 'q', long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Enable verbose output
    #[arg(short = 'v', long, global = true, conflicts_with = "quiet")]
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
    Set to true to advertise every open or draft PR as a `pr/*` branch.
    Defaults to false; local config overrides global config. `ngit pr checkout`
    creates and tracks a selected PR branch so `git fetch`, `git pull`, and
    `git push` keep working without enabling every PR. Use `git fetch --prune`
    to remove branches fetched before upgrading to this default.

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
        auto_pr_branches = key("nostr.auto-pr-branches true"),
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
    let nbunksec = if let Some(nbunksec) = &args.nbunksec {
        Some(nbunksec.clone())
    } else if let Some(path) = &args.nbunksec_file {
        Some(read_nbunksec_file(path)?)
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
    } else if let Some(value) = nbunksec {
        let connection = ngit::login::nbunksec::decode(&value).context("invalid nbunksec")?;
        Ok(Some(SignerInfo::Bunker {
            bunker_uri: connection.bunker_uri,
            bunker_app_key: connection.client_key,
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
    } else if let Some(selector) = &args.signer {
        Ok(Some(SignerInfo::Selection {
            selector: selector.clone(),
        }))
    } else {
        Ok(None)
    }
}

fn read_nsec_file(path: &Path) -> Result<String> {
    read_secret_file(path, "nsec")
}

fn read_nbunksec_file(path: &Path) -> Result<String> {
    read_secret_file(path, "nbunksec")
}

fn read_secret_file(path: &Path, label: &str) -> Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open {label} file"))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to inspect open {label} file"))?;
    if !meta.is_file() {
        bail!("{label} file path must resolve to a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !matches!(meta.permissions().mode() & 0o777, 0o400 | 0o600) {
            bail!("{label} file permissions must be 0400 or 0600");
        }
    }
    if !(1..=4096).contains(&meta.len()) {
        bail!("{label} file must contain 1 to 4096 bytes");
    }
    let capacity = usize::try_from(meta.len())
        .with_context(|| format!("{label} file size does not fit memory"))?;
    let mut raw = Vec::with_capacity(capacity);
    file.take(4097)
        .read_to_end(&mut raw)
        .with_context(|| format!("failed to read {label} file"))?;
    if raw.len() > 4096 {
        bail!("{label} file must contain 1 to 4096 bytes");
    }
    let value = std::str::from_utf8(&raw).with_context(|| format!("{label} file must be UTF-8"))?;
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value);
    if value.is_empty() || value.contains(['\r', '\n']) {
        bail!("{label} file must contain exactly one non-empty line");
    }
    Ok(value.to_string())
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// create and publish a new repository on nostr
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
    /// inspect CI results and the trust context behind them
    Ci(CiSubCommandArgs),
    /// work with software applications, releases, and release assets
    #[command(alias = "releases")]
    Release(ReleaseSubCommandArgs),
    /// publish static websites through Nostr and Blossom
    Nsite(NsiteSubCommandArgs),
    /// publish OCI container images through Nostr and Blossom
    #[command(visible_alias = "oci")]
    Container(ContainerSubCommandArgs),
    /// update repo git servers to reflect nostr state (add, update or delete
    /// remote refs)
    Sync(sub_commands::sync::SubCommandArgs),
    /// install and update ngit's repository skill for coding agents
    Skill(SkillArgs),
    /// inspect and update the ngit installation
    Update(UpdateArgs),
    /// list accounts, create an account, login, logout or export keys
    Account(AccountSubCommandArgs),
}

#[derive(clap::Args)]
pub struct UpdateArgs {
    /// Install this exact signed release instead of selecting the newest
    /// eligible version
    #[arg(value_name = "VERSION")]
    pub target: Option<String>,
    /// Check release readiness without modifying the installation
    #[arg(long)]
    pub check: bool,
    /// Extend release discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
}

// ---------------------------------------------------------------------------
// NIP-5A static-site subcommand group
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct NsiteSubCommandArgs {
    #[command(subcommand)]
    pub nsite_command: NsiteCommands,
}

#[derive(Subcommand)]
pub enum NsiteCommands {
    /// publish a directory as a root or named NIP-5A static site
    Publish(NsitePublishArgs),
}

#[derive(clap::Args)]
pub struct NsitePublishArgs {
    /// Directory containing already-built static assets
    #[arg(value_name = "DIRECTORY")]
    pub directory: PathBuf,
    /// Read nsyte-compatible defaults from this JSON file
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    pub config: Option<PathBuf>,
    /// Ignore .nsite/config.json
    #[arg(long, conflicts_with = "config")]
    pub no_config: bool,
    /// Publish a named site with this identifier; omit for the root site
    #[arg(long = "id", visible_alias = "name", value_name = "ID")]
    pub identifier: Option<String>,
    /// Human-readable site title
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,
    /// Short site description
    #[arg(long, value_name = "TEXT", conflicts_with = "description_file")]
    pub description: Option<String>,
    /// Read the site description from a file
    #[arg(long, value_name = "PATH", conflicts_with = "description")]
    pub description_file: Option<PathBuf>,
    /// Source repository or archive URL (https:// or nostr://)
    #[arg(long, value_name = "URL")]
    pub source: Option<String>,
    /// Map this build-output path to /404.html
    #[arg(long, value_name = "SITE_PATH")]
    pub fallback: Option<String>,
    /// Override kind-10063 discovery with a Blossom server (repeatable)
    #[arg(long = "blossom-server", value_name = "URL")]
    pub blossom_servers: Vec<String>,
    /// Extend manifest discovery and publication with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Maximum simultaneous Blossom presence checks and uploads
    #[arg(
        long,
        value_name = "N",
        default_value_t = ngit::blossom::DEFAULT_UPLOAD_CONCURRENCY,
        value_parser = parse_nsite_concurrency
    )]
    pub concurrency: usize,
}

fn parse_nsite_concurrency(value: &str) -> std::result::Result<usize, String> {
    let concurrency = value
        .parse::<usize>()
        .map_err(|_| "concurrency must be an integer from 1 to 64".to_owned())?;
    if (1..=64).contains(&concurrency) {
        Ok(concurrency)
    } else {
        Err("concurrency must be an integer from 1 to 64".to_owned())
    }
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
    #[command(
        long_about = "export nostr keys to login to other nostr clients\n\nBy default, opens a menu for printing or displaying the npub and secret as a QR code. Use --secret to print only the nsec or nbunksec.\n\nExamples:\n  ngit account export-keys --secret\n  ngit --signer work account export-keys --secret"
    )]
    ExportKeys(sub_commands::export_keys::SubCommandArgs),
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
// Software release subcommand group
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct ReleaseSubCommandArgs {
    #[command(subcommand)]
    pub release_command: ReleaseCommands,
}

// ---------------------------------------------------------------------------
// OCI container subcommand group
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct ContainerSubCommandArgs {
    #[command(subcommand)]
    pub container_command: ContainerCommands,
}

#[derive(Subcommand)]
pub enum ContainerCommands {
    /// upload an OCI image layout to Blossom and publish its tag map
    Publish(ContainerPublishArgs),
}

#[derive(clap::Args)]
pub struct ContainerPublishArgs {
    /// Container repository name; one lowercase OCI name component
    #[arg(value_name = "NAME")]
    pub repository: String,
    /// OCI image-layout directory containing index.json and blobs/sha256
    #[arg(long, value_name = "PATH")]
    pub layout: Option<PathBuf>,
    /// Container settings; otherwise discover .ngit/containers.yaml
    #[arg(long, value_name = "PATH", conflicts_with = "no_manifest")]
    pub manifest: Option<PathBuf>,
    /// Ignore .ngit/containers.yaml
    #[arg(long, conflicts_with = "manifest")]
    pub no_manifest: bool,
    /// Override kind-10063 discovery with a Blossom server (repeatable)
    #[arg(long = "blossom-server", value_name = "URL")]
    pub blossom_servers: Vec<String>,
    /// Extend the current repository relay set (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Human-readable repository title (defaults to NAME)
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,
    /// Human-readable repository description
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,
    /// Absolute HTTP(S) URL for the image source repository
    #[arg(long, value_name = "URL")]
    pub source: Option<String>,
    /// Replace the complete tag map and metadata instead of merging this layout
    #[arg(long)]
    pub replace: bool,
}

#[derive(Subcommand)]
pub enum ReleaseCommands {
    /// list releases for applications linked to this repository
    List(ReleaseListArgs),
    /// view a release and all of its referenced assets
    View(ReleaseViewArgs),
    /// publish a new release or explicitly edit an existing release
    Publish(ReleasePublishArgs),
    /// work with software applications
    #[command(alias = "application")]
    App(ReleaseAppSubCommandArgs),
    /// work with release assets
    Asset(ReleaseAssetSubCommandArgs),
}

#[derive(clap::Args)]
pub struct ReleaseListArgs {
    /// Filter by application identifier, naddr, or application coordinate
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
    /// Filter by release channel
    #[arg(long, value_name = "CHANNEL")]
    pub channel: Option<String>,
    /// Filter by target platform (repeatable, OR logic)
    #[arg(long = "platform", value_name = "PLATFORM")]
    pub platforms: Vec<String>,
    /// Filter by an explicit trusted application author
    #[arg(long, value_name = "PUBKEY")]
    pub author: Option<String>,
    /// Limit the number of releases returned
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Extend discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct ReleaseViewArgs {
    /// Release app@version, naddr, event-id, nevent, or unambiguous version
    #[arg(value_name = "RELEASE")]
    pub release: String,
    /// Application context for a bare release version
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
    /// Download release assets and verify their hashes and sizes
    #[arg(long)]
    pub verify: bool,
    /// Extend discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReleasePublishArgs {
    /// Exact release version (defaults to the exact Git tag, without one
    /// leading v)
    #[arg(value_name = "VERSION")]
    pub release_version: Option<String>,
    /// Application identifier, naddr, or application coordinate
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
    /// Release channel (defaults to main when creating)
    #[arg(long, value_name = "CHANNEL")]
    pub channel: Option<String>,
    /// Release notes
    #[arg(long, value_name = "TEXT", conflicts_with = "notes_file")]
    pub notes: Option<String>,
    /// Read release notes from a file
    #[arg(long, value_name = "PATH", conflicts_with = "notes")]
    pub notes_file: Option<PathBuf>,
    /// Release date as Unix seconds (defaults to now when creating)
    #[arg(long, value_name = "UNIX_SECONDS")]
    pub released_at: Option<u64>,
    /// Exact Git tag used for version discovery and {tag} manifest expansion
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,
    /// Git commit represented by this release (defaults to HEAD when creating)
    #[arg(long, value_name = "COMMIT")]
    pub commit: Option<String>,
    /// Release assets and publication defaults; creation also discovers
    /// .ngit/release.yaml
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,
    /// Add a URL-backed asset as PLATFORM=URL (repeatable)
    #[arg(long = "asset", value_name = "PLATFORM=URL")]
    pub assets: Vec<String>,
    /// Upload PATH, or use PLATFORM=PATH shorthand (repeatable)
    #[arg(long = "file", value_name = "[PLATFORM=]PATH")]
    pub files: Vec<String>,
    /// Target platform for one bare --file PATH (repeatable)
    #[arg(long = "platform", value_name = "PLATFORM")]
    pub file_platforms: Vec<String>,
    /// Reuse an existing kind 3063 asset event (repeatable)
    #[arg(long = "asset-event", value_name = "ASSET")]
    pub asset_events: Vec<String>,
    /// Add a URL-backed asset with no target platform (repeatable)
    #[arg(long = "platform-agnostic-asset", value_name = "URL")]
    pub platform_agnostic_assets: Vec<String>,
    /// Upload a local platform-agnostic asset to Blossom (repeatable)
    #[arg(long = "platform-agnostic-file", value_name = "PATH")]
    pub platform_agnostic_files: Vec<PathBuf>,
    /// Acknowledge reused asset events which have no platform tags
    #[arg(long)]
    pub accept_platform_agnostic_assets: bool,
    /// Add release-only platforms to the replaceable application event
    #[arg(long)]
    pub add_application_platforms: bool,
    /// Permit a non-main release to omit application platforms
    #[arg(long)]
    pub allow_partial_platforms: bool,
    /// Override kind-10063 discovery with an ordered Blossom server
    /// (repeatable)
    #[arg(long = "blossom-server", value_name = "URL")]
    pub blossom_servers: Vec<String>,
    /// Explicitly replace an existing release; never creates a missing release
    #[arg(long)]
    pub edit: bool,
    /// Treat metadata warnings as errors
    #[arg(long)]
    pub strict_metadata: bool,
    /// Extend discovery and publication with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Also publish to the Zapstore catalog relay; does not change Blossom
    #[arg(long)]
    pub zapstore_relay: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Parser)]
pub struct ReleaseAppSubCommandArgs {
    #[command(subcommand)]
    pub app_command: ReleaseAppCommands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum ReleaseAppCommands {
    /// list applications linked to this repository or owned by a user
    List(ReleaseAppListArgs),
    /// view an application and its publication authority
    View(ReleaseAppViewArgs),
    /// create a linked application or explicitly edit one
    Init(ReleaseAppInitArgs),
    /// link an existing application to this repository
    Link(ReleaseAppLinkArgs),
}

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReleaseAppListArgs {
    /// List applications authored by the active user
    #[arg(long, group = "application_owner")]
    pub mine: bool,
    /// Filter user or author applications to those not linked to this
    /// repository
    #[arg(long, conflicts_with = "linked", requires = "application_owner")]
    pub unlinked: bool,
    /// Filter user or author applications to those linked to this repository
    #[arg(long, conflicts_with = "unlinked", requires = "application_owner")]
    pub linked: bool,
    /// List applications by an explicit author
    #[arg(long, value_name = "PUBKEY", group = "application_owner")]
    pub author: Option<String>,
    /// Extend discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct ReleaseAppViewArgs {
    /// Application identifier, naddr, or application coordinate
    #[arg(value_name = "APP")]
    pub app: String,
    /// Extend discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReleaseAppInitArgs {
    /// Application identifier (defaults to the repository identifier)
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,
    /// Application display name (required when no repository default exists)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Application description
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with_all = ["description_file", "clear_description"]
    )]
    pub description: Option<String>,
    /// Read the application description from a file
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["description", "clear_description"]
    )]
    pub description_file: Option<PathBuf>,
    /// Remove the application description when editing
    #[arg(long, conflicts_with_all = ["description", "description_file"])]
    pub clear_description: bool,
    /// Short application summary
    #[arg(long, value_name = "TEXT", conflicts_with = "clear_summary")]
    pub summary: Option<String>,
    /// Remove the application summary when editing
    #[arg(long, conflicts_with = "summary")]
    pub clear_summary: bool,
    /// Application icon URL
    #[arg(long, value_name = "URL", conflicts_with = "clear_icon")]
    pub icon: Option<String>,
    /// Remove the application icon when editing
    #[arg(long, conflicts_with = "icon")]
    pub clear_icon: bool,
    /// Application image URL (repeatable)
    #[arg(long = "image", value_name = "URL", conflicts_with = "clear_images")]
    pub images: Vec<String>,
    /// Remove all application images when editing
    #[arg(long, conflicts_with = "images")]
    pub clear_images: bool,
    /// Application topic (repeatable)
    #[arg(long = "topic", value_name = "TOPIC", conflicts_with = "clear_topics")]
    pub topics: Vec<String>,
    /// Remove all application topics when editing
    #[arg(long, conflicts_with = "topics")]
    pub clear_topics: bool,
    /// Application website URL
    #[arg(long, value_name = "URL", conflicts_with = "clear_website")]
    pub website: Option<String>,
    /// Remove the application website when editing
    #[arg(long, conflicts_with = "website")]
    pub clear_website: bool,
    /// Canonical repository clone URL
    #[arg(long, value_name = "URL", conflicts_with = "clear_repository")]
    pub repository: Option<String>,
    /// Remove the canonical repository clone URL when editing
    #[arg(long, conflicts_with = "repository")]
    pub clear_repository: bool,
    /// Supported platform (repeatable)
    #[arg(
        long = "platform",
        value_name = "PLATFORM",
        conflicts_with = "clear_platforms"
    )]
    pub platforms: Vec<String>,
    /// Remove all application platform hints when editing
    #[arg(long, conflicts_with = "platforms")]
    pub clear_platforms: bool,
    /// SPDX license expression
    #[arg(long, value_name = "SPDX", conflicts_with = "clear_license")]
    pub license: Option<String>,
    /// Remove the application license when editing
    #[arg(long, conflicts_with = "license")]
    pub clear_license: bool,
    /// Explicitly replace an existing application; never creates a missing app
    #[arg(long)]
    pub edit: bool,
    /// Treat metadata warnings as errors
    #[arg(long)]
    pub strict_metadata: bool,
    /// Extend discovery and publication with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Also publish to the Zapstore catalog relay; does not change Blossom
    #[arg(long)]
    pub zapstore_relay: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct ReleaseAppLinkArgs {
    /// Application identifier, naddr, or application coordinate
    #[arg(value_name = "APP")]
    pub app: String,
    /// Confirm replacement of the existing application event
    #[arg(long, required = true)]
    pub edit: bool,
    /// Extend discovery and publication with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Also publish to the Zapstore catalog relay; does not change Blossom
    #[arg(long)]
    pub zapstore_relay: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Parser)]
pub struct ReleaseAssetSubCommandArgs {
    #[command(subcommand)]
    pub asset_command: ReleaseAssetCommands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum ReleaseAssetCommands {
    /// list the assets referenced by a release
    List(ReleaseAssetListArgs),
    /// view complete metadata for a release asset
    View(ReleaseAssetViewArgs),
    /// attach an existing asset and explicitly replace a release
    Add(ReleaseAssetAddArgs),
}

#[derive(clap::Args)]
pub struct ReleaseAssetListArgs {
    /// Release app@version, naddr, event-id, nevent, or unambiguous version
    #[arg(value_name = "RELEASE")]
    pub release: String,
    /// Application context for a bare release version
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
    /// Extend discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct ReleaseAssetViewArgs {
    /// Asset event-id, nevent, or a filename unique within --release
    #[arg(value_name = "ASSET")]
    pub asset: String,
    /// Release context used to validate membership and authority
    #[arg(long, value_name = "RELEASE")]
    pub release: Option<String>,
    /// Application context for a bare release version
    #[arg(long, value_name = "APP", requires = "release")]
    pub app: Option<String>,
    /// Download the asset and verify its hash and size
    #[arg(long)]
    pub verify: bool,
    /// Extend discovery with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Use local cache only, skip network fetch
    #[arg(long)]
    pub offline: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReleaseAssetAddArgs {
    /// Release app@version, naddr, event-id, nevent, or unambiguous version
    #[arg(value_name = "RELEASE")]
    pub release: String,
    /// Application context for a bare release version
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
    /// URL of a new asset to download, hash, and publish
    #[arg(
        long,
        value_name = "URL",
        required_unless_present_any = ["event", "file"],
        conflicts_with_all = ["event", "file"]
    )]
    pub url: Option<String>,
    /// Local asset to upload to Blossom
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present_any = ["url", "event"],
        conflicts_with_all = ["url", "event"]
    )]
    pub file: Option<PathBuf>,
    /// Existing kind 3063 asset event to attach
    #[arg(
        long,
        value_name = "ASSET",
        required_unless_present_any = ["url", "file"],
        conflicts_with_all = ["url", "file"]
    )]
    pub event: Option<String>,
    /// Target platform (repeatable)
    #[arg(
        long = "platform",
        value_name = "PLATFORM",
        conflicts_with_all = ["event", "platform_agnostic"]
    )]
    pub platforms: Vec<String>,
    /// Explicitly acknowledge that the asset has no target platform
    #[arg(long, conflicts_with = "platforms")]
    pub platform_agnostic: bool,
    /// Asset identifier (defaults to the application identifier)
    #[arg(long, value_name = "ID", conflicts_with = "event")]
    pub asset_id: Option<String>,
    /// Asset version (defaults to the release version)
    #[arg(long, value_name = "VERSION", conflicts_with = "event")]
    pub asset_version: Option<String>,
    /// Published filename override
    #[arg(long, value_name = "NAME", conflicts_with = "event")]
    pub filename: Option<String>,
    /// MIME type override
    #[arg(long, value_name = "MIME", conflicts_with = "event")]
    pub mime: Option<String>,
    /// Minimum supported platform version
    #[arg(long, value_name = "VERSION", conflicts_with = "event")]
    pub min_platform_version: Option<String>,
    /// Target platform version
    #[arg(long, value_name = "VERSION", conflicts_with = "event")]
    pub target_platform_version: Option<String>,
    /// Supported NIP number (repeatable)
    #[arg(long = "supported-nip", value_name = "NIP", conflicts_with = "event")]
    pub supported_nips: Vec<String>,
    /// Build variant
    #[arg(long, value_name = "VARIANT", conflicts_with = "event")]
    pub variant: Option<String>,
    /// Source commit identifier
    #[arg(long, value_name = "COMMIT", conflicts_with = "event")]
    pub commit: Option<String>,
    /// Minimum allowed asset version
    #[arg(long, value_name = "VERSION", conflicts_with = "event")]
    pub min_allowed_version: Option<String>,
    /// Android version code
    #[arg(long, value_name = "CODE", conflicts_with = "event")]
    pub android_version_code: Option<u64>,
    /// Minimum allowed Android version code
    #[arg(long, value_name = "CODE", conflicts_with = "event")]
    pub android_min_allowed_version_code: Option<u64>,
    /// Android signing certificate SHA-256 (repeatable)
    #[arg(
        long = "android-certificate-sha256",
        value_name = "SHA256",
        conflicts_with = "event"
    )]
    pub android_certificate_sha256: Vec<String>,
    /// Original web source when it differs from the asset URL
    #[arg(long, value_name = "URL", conflicts_with = "event")]
    pub original_url: Option<String>,
    /// Add release-only platforms to the replaceable application event
    #[arg(long)]
    pub add_application_platforms: bool,
    /// Permit a non-main release to omit application platforms
    #[arg(long)]
    pub allow_partial_platforms: bool,
    /// Override kind-10063 discovery with an ordered Blossom server
    /// (repeatable)
    #[arg(
        long = "blossom-server",
        value_name = "URL",
        conflicts_with_all = ["url", "event"]
    )]
    pub blossom_servers: Vec<String>,
    /// Confirm replacement of the existing release event
    #[arg(long, required = true)]
    pub edit: bool,
    /// Treat metadata warnings as errors
    #[arg(long)]
    pub strict_metadata: bool,
    /// Extend discovery and publication with a relay (repeatable)
    #[arg(long = "relay", value_name = "URL")]
    pub relays: Vec<String>,
    /// Also publish to the Zapstore catalog relay; does not change Blossom
    #[arg(long)]
    pub zapstore_relay: bool,
    /// Output as JSON
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
    /// Refuse to merge unless the current CI result is a success whose
    /// weakest run meets this trust floor
    #[arg(long, value_name = "LEVEL", value_enum)]
    pub require_ci_trust: Option<CiTrustFloor>,
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
        /// Which signed per-job log tails to include in human and JSON output
        #[arg(long, value_name = "MODE", value_enum, default_value = "auto")]
        log_tail: LogTailMode,
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
        long_about = "merge a PR into the current branch (maintainer only)\n\nperforms a git merge of the PR branch; push afterwards to update the nostr state\n\nthe PR's CI results and the trust context of every signer behind them are printed before the merge. Without --require-ci-trust a result that is failing, unfinished, or signed only by signers with no known context is a warning, not a refusal."
    )]
    Merge {
        /// Proposal event-id (hex) or nevent (bech32)
        #[arg(value_name = "ID|nevent")]
        id: String,
        /// Use squash merge
        #[arg(long)]
        squash: bool,
        /// Refuse to merge unless the current CI result is a success whose
        /// weakest run meets this trust floor
        #[arg(long, value_name = "LEVEL", value_enum)]
        require_ci_trust: Option<CiTrustFloor>,
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
// CI subcommand group
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
pub struct CiSubCommandArgs {
    #[command(subcommand)]
    pub ci_command: CiCommands,
}

/// The trust floor `--require-ci-trust` enforces. The values are the
/// classification names shared with gitworkshop.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CiTrustFloor {
    #[value(name = "maintainer-directed")]
    MaintainerDirected,
    #[value(name = "operationally-associated")]
    OperationallyAssociated,
}

impl CiTrustFloor {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaintainerDirected => "maintainer-directed",
            Self::OperationallyAssociated => "operationally-associated",
        }
    }
}

/// Which signed Job Result log tails a detail surface renders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogTailMode {
    /// Include tails for every conclusion except success.
    #[default]
    Auto,
    /// Include tails for every job.
    All,
    /// Do not include log tails.
    None,
}

#[derive(Subcommand)]
pub enum CiCommands {
    /// show CI results, with the trust context of every signer behind them
    #[command(
        long_about = "show CI results, with the trust context of every signer behind them\n\n\
        <TARGET> is resolved in this order:\n  \
        1. `#<hex-prefix>` is always a PR/event-id prefix\n  \
        2. an nevent, note, or full 64-character event id is always an event id, and must name a cached PR or one of its revisions\n  \
        3. otherwise a commit-ish, resolved with git (an annotated tag is queried by both its tag object id and the commit it peels to)\n  \
        4. a bare short hex that is not a commit-ish falls back to a PR event-id prefix\n  \
        5. with no target, the HEAD commit\n\n\
        A PR reports only the runs for its latest revision; results for earlier revisions are never presented as current. Each job line includes its provider-published log URL when present. Signed per-job log tails are included for non-successful jobs by default; `--log-tail` selects auto, all, or none for both human and JSON output.\n\n\
        Trust context describes why a result may deserve attention. `No known context` is an absence of evidence, never a finding against the signer. The integrity marker is separate from trust: it is ngit's own check that it holds the commit and that the workflow file at that commit hashes to what the coordinator signed."
    )]
    Status {
        /// PR (`#<prefix>`, nevent, or event-id), commit-ish, or nothing for
        /// HEAD
        #[arg(value_name = "TARGET")]
        target: Option<String>,
        /// Exit non-zero unless the current result is a success whose weakest
        /// run meets this trust floor
        #[arg(long, value_name = "LEVEL", value_enum)]
        require_ci_trust: Option<CiTrustFloor>,
        /// Which signed per-job log tails to include in human and JSON output
        #[arg(long, value_name = "MODE", value_enum, default_value = "auto")]
        log_tail: LogTailMode,
        /// Skip the relay fetch and NIP-05 trust verification, reading CI
        /// from the local cache
        #[arg(long)]
        offline: bool,
    },
    /// ask a coordinator to run CI for this repository
    #[command(
        long_about = "ask a coordinator to run CI for this repository (kind-9843 Service Request)\n\n\
        The request is a standing one: it covers runs the coordinator starts after it, and stays in force until `ngit ci stop`. It never covers runs that started before it.\n\n\
        <COORDINATOR> is the coordinator's public key, as an npub or hex.\n\n\
        The request is signed for one repository perspective: your own announcement when you have published one, otherwise the selected maintainer's. A coordinator's default policy accepts only a confirmed maintainer of that perspective, so ngit warns — but does not refuse — when you are not one; an operator may have accepted your key explicitly."
    )]
    Request {
        /// Coordinator public key (npub or hex)
        #[arg(value_name = "COORDINATOR")]
        coordinator: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// ask a coordinator to stop running CI for this repository
    #[command(
        long_about = "ask a coordinator to stop running CI for this repository (kind-9844 Service Stop)\n\n\
        Published for the same repository perspective as `ngit ci request`. A confirmed maintainer's Stop closes every earlier Request for that perspective; anybody else's closes only their own."
    )]
    Stop {
        /// Coordinator public key (npub or hex)
        #[arg(value_name = "COORDINATOR")]
        coordinator: String,
        /// Use local cache only, skip network fetch
        #[arg(long)]
        offline: bool,
    },
    /// ask a coordinator to run one workflow once
    #[command(
        long_about = "ask a coordinator to run one workflow once (kind-9840 Manual Trigger)\n\n\
        A Manual Trigger is a one-shot authorization for exactly the workflow file identified by its content hash at the resolved commit, so it can replay a push or pull-request workflow that does not declare `manual`. It needs no standing Service Request.\n\n\
        <COMMIT-ISH> defaults to HEAD. An annotated tag is published as both the commit it peels to (first) and the tag object id, so a single `#c` query finds the run either way; ngit refuses to publish `c` values that do not all peel to the same commit.\n\n\
        --workflow is a path in the repository, and its SHA-256 is taken from the blob at the resolved commit — never from the working tree, whose line endings and clean/smudge filters can differ from the object the coordinator hashes."
    )]
    Trigger {
        /// Coordinator public key (npub or hex)
        #[arg(value_name = "COORDINATOR")]
        coordinator: String,
        /// Commit-ish to run; defaults to HEAD
        #[arg(value_name = "COMMIT-ISH")]
        commit_ish: Option<String>,
        /// Path of the workflow file, as it exists at the resolved commit
        #[arg(long, value_name = "PATH")]
        workflow: String,
        /// Git ref published as the run's context, e.g. refs/heads/main
        #[arg(long = "ref", value_name = "GIT-REF")]
        git_ref: Option<String>,
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
        /// Include the original title/body and every authorised edit (requires
        /// ID)
        #[arg(long, requires = "id")]
        history: bool,
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
        /// Include the original title/body and every authorised edit
        #[arg(long)]
        history: bool,
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
    /// create and publish a new repository on nostr (alias for `ngit init`)
    Init(sub_commands::init::SubCommandArgs),
    /// update repository metadata on nostr
    #[command(
        long_about = "update an existing repository announcement on nostr\n\nrepository announcements are created with `ngit init`; use this command for every later metadata or roster change. Omitted settings are preserved; collection settings use targeted --add-* and --remove-* actions",
        after_long_help = "Examples:\n  ngit repo edit --description \"New description\"\n  ngit repo edit --add-grasp-server grasp.example.com\n  ngit repo edit --remove-additional-relay wss://old.example.com --add-additional-relay wss://new.example.com\n  ngit repo edit --repair-self-defer m=continue\n  ngit repo edit --repair-self-defer m=1788467593"
    )]
    Edit(sub_commands::repo::edit::SubCommandArgs),
    /// accept an invitation to co-maintain a repository
    #[command(long_about = "accept an invitation to co-maintain a repository\n\n\
            publishes your repository announcement to nostr, confirming your co-maintainership.\n\n\
            This is required because your signed announcement is what ties your git state events\n\
            to a specific repository coordinate chain, preventing scammers from attributing your\n\
            commits to a fake repository. See `ngit repo info` for details on the maintainer model.")]
    Accept(sub_commands::repo::accept::SubCommandArgs),
    /// end your own role in a repository you co-maintain or moderate
    #[command(
        long_about = "end your own role in a repository you co-maintain or moderate\n\n\
            republishes your repository announcement with your self-role ended (per NIP-34 a\n\
            member may leave by ending their self-role). Your own record takes precedence over\n\
            maintainer assignments in other members' announcements, so this removes you from\n\
            the repository's authorized member set even while others still list you."
    )]
    Leave(sub_commands::repo::leave::SubCommandArgs),
    /// follow the repository's resolved lead maintainer
    #[command(
        name = "follow-lead",
        long_about = "follow the repository's resolved lead maintainer\n\nupdates retained role history when applicable, then switches the selected repository coordinate and matching nostr remotes"
    )]
    FollowLead(sub_commands::repo::follow_lead::SubCommandArgs),
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use clap::{Command, CommandFactory, Parser};
    use tempfile::tempdir;

    use super::{
        AccountCommands, CiCommands, CiTrustFloor, Cli, Commands, ContainerCommands, LogTailMode,
        PrCommands, ReleaseAppCommands, ReleaseAssetCommands, ReleaseCommands,
        extract_signer_cli_arguments, read_nsec_file,
    };

    #[test]
    fn ci_detail_log_tail_modes_default_to_auto() {
        let cli = Cli::try_parse_from(["ngit", "ci", "status"]).unwrap();
        let Some(Commands::Ci(args)) = cli.command else {
            panic!("expected ci command");
        };
        let CiCommands::Status { log_tail, .. } = args.ci_command else {
            panic!("expected ci status command");
        };
        assert_eq!(log_tail, LogTailMode::Auto);

        let cli =
            Cli::try_parse_from(["ngit", "pr", "view", "deadbeef", "--log-tail=none"]).unwrap();
        let Some(Commands::Pr(args)) = cli.command else {
            panic!("expected pr command");
        };
        let PrCommands::View { log_tail, .. } = args.pr_command else {
            panic!("expected pr view command");
        };
        assert_eq!(log_tail, LogTailMode::None);

        assert!(Cli::try_parse_from(["ngit", "ci", "status", "--log-tail=everything"]).is_err());
    }

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

    #[test]
    fn export_keys_secret_is_explicit_and_not_json_wrapped() {
        let cli = Cli::try_parse_from(["ngit", "account", "export-keys", "--secret"])
            .expect("export-keys should accept --secret");
        let Some(Commands::Account(account)) = cli.command else {
            panic!("expected account command");
        };
        let AccountCommands::ExportKeys(args) = account.account_command else {
            panic!("expected export-keys command");
        };
        assert!(args.secret);
        assert!(
            Cli::try_parse_from(["ngit", "account", "export-keys", "--secret", "--json",]).is_err()
        );
    }

    #[test]
    fn top_level_merge_accepts_ci_trust_gate() {
        let cli = Cli::try_parse_from([
            "ngit",
            "merge",
            "deadbeef",
            "--require-ci-trust",
            "maintainer-directed",
        ])
        .expect("top-level merge should accept a CI trust floor");
        let Some(Commands::Merge(args)) = cli.command else {
            panic!("expected merge command");
        };
        assert!(matches!(
            args.require_ci_trust,
            Some(CiTrustFloor::MaintainerDirected)
        ));
    }

    #[test]
    fn quiet_is_global_and_conflicts_with_verbose() {
        for args in [
            ["ngit", "--quiet", "issue", "list"].as_slice(),
            ["ngit", "issue", "-q", "list"].as_slice(),
            ["ngit", "issue", "list", "--quiet"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(args).unwrap().quiet,
                "failed for {args:?}"
            );
        }

        for args in [
            ["ngit", "--quiet", "--verbose", "issue", "list"].as_slice(),
            ["ngit", "issue", "list", "-q", "-v"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(args).is_err(),
                "quiet and verbose both parsed for {args:?}"
            );
        }
    }

    #[test]
    fn issue_list_history_requires_an_issue_id() {
        for args in [
            ["ngit", "issue", "list", "--history"].as_slice(),
            ["ngit", "issue", "list", "--history", "--json"].as_slice(),
        ] {
            let Err(error) = Cli::try_parse_from(args) else {
                panic!("issue list accepted --history without an issue ID: {args:?}");
            };
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
        }

        Cli::try_parse_from(["ngit", "issue", "list", "--history", "deadbeef"])
            .expect("issue list should accept --history with an issue ID");
    }

    fn key_file(path: &Path, value: &[u8]) {
        fs::write(path, value).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn nbunksec_fixture() -> String {
        use nostr::prelude::{Keys, NostrConnectUri, RelayUrl};

        let remote = Keys::parse(&"1".repeat(64)).unwrap();
        let client = Keys::parse(&"2".repeat(64)).unwrap();
        let uri = NostrConnectUri::Bunker {
            remote_signer_public_key: remote.public_key(),
            relays: vec![RelayUrl::parse("wss://relay.example.com").unwrap()],
            secret: None,
        };
        ngit::login::nbunksec::encode(&uri.to_string(), &client.secret_key().to_secret_hex())
            .unwrap()
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
    fn nbunksec_and_file_parse_as_one_shot_bunker_signers() {
        let value = nbunksec_fixture();
        for args in [
            vec!["ngit", "--nbunksec", &value, "issue", "create"],
            vec!["ngit", "issue", "create", "--nbunksec", &value],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(
                extract_signer_cli_arguments(&cli).unwrap(),
                Some(ngit::login::SignerInfo::Bunker { npub: None, .. })
            ));
        }

        let dir = tempdir().unwrap();
        let path = dir.path().join("connection");
        key_file(&path, format!("{value}\n").as_bytes());
        let cli = Cli::try_parse_from([
            "ngit",
            "--nbunksec-file",
            path.to_str().unwrap(),
            "issue",
            "create",
        ])
        .unwrap();
        assert!(matches!(
            extract_signer_cli_arguments(&cli).unwrap(),
            Some(ngit::login::SignerInfo::Bunker { npub: None, .. })
        ));
    }

    #[test]
    fn nbunksec_sources_conflict_with_other_signers() {
        let value = nbunksec_fixture();
        for args in [
            vec!["ngit", "--nbunksec", &value, "--nsec", "key"],
            vec!["ngit", "--nbunksec", &value, "--signer", "fred"],
            vec![
                "ngit",
                "--nbunksec",
                &value,
                "--nbunksec-file",
                "connection",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
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
                "--nbunksec",
                &nbunksec_fixture()
            ])
            .is_err()
        );
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
            ["--nbunksec", "key"],
            ["--nbunksec-file", "key"],
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

        for conflicting in ["--signer", "--nsec", "--nbunksec", "--bunker-url"] {
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

    #[test]
    fn release_application_commands_and_aliases_parse() {
        for args in [
            ["ngit", "release", "app", "list", "--json", "--offline"].as_slice(),
            [
                "ngit",
                "releases",
                "application",
                "view",
                "ngit",
                "--json",
                "--offline",
            ]
            .as_slice(),
            ["ngit", "release", "app", "init", "--name", "ngit", "--json"].as_slice(),
            ["ngit", "release", "app", "link", "ngit", "--edit", "--json"].as_slice(),
        ] {
            Cli::try_parse_from(args)
                .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
        }
    }

    #[test]
    fn container_publish_and_oci_alias_parse() {
        for args in [
            [
                "ngit",
                "container",
                "publish",
                "my-app",
                "--layout",
                "/tmp/layout",
                "--blossom-server",
                "https://blossom.example",
            ]
            .as_slice(),
            [
                "ngit",
                "oci",
                "publish",
                "my-app",
                "--layout",
                "/tmp/layout",
                "--blossom-server",
                "https://blossom.example",
                "--json",
            ]
            .as_slice(),
            [
                "ngit",
                "container",
                "publish",
                "my-app",
                "--manifest",
                "ci/containers.yaml",
            ]
            .as_slice(),
        ] {
            let cli = Cli::try_parse_from(args)
                .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
            let Some(Commands::Container(container)) = cli.command else {
                panic!("expected container command");
            };
            assert!(matches!(
                container.container_command,
                ContainerCommands::Publish(_)
            ));
        }

        assert!(
            Cli::try_parse_from([
                "ngit",
                "container",
                "publish",
                "my-app",
                "--manifest",
                "ci/containers.yaml",
                "--no-manifest",
            ])
            .is_err()
        );
    }

    #[test]
    fn release_read_commands_parse() {
        for args in [
            ["ngit", "release", "list", "--json", "--offline"].as_slice(),
            [
                "ngit",
                "releases",
                "view",
                "ngit@1.8.0",
                "--json",
                "--offline",
            ]
            .as_slice(),
        ] {
            Cli::try_parse_from(args)
                .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
        }
    }

    #[test]
    fn update_command_parses_version_check_and_relays() {
        let cli = Cli::try_parse_from([
            "ngit",
            "update",
            "3.0.2",
            "--check",
            "--relay",
            "wss://releases.example",
        ])
        .expect("update should parse");
        let Some(Commands::Update(args)) = cli.command else {
            panic!("expected update command");
        };
        assert_eq!(args.target.as_deref(), Some("3.0.2"));
        assert!(args.check);
        assert_eq!(args.relays, ["wss://releases.example"]);

        let cli = Cli::try_parse_from(["ngit", "update", "--check"])
            .expect("update should allow automatic version selection");
        let Some(Commands::Update(args)) = cli.command else {
            panic!("expected update command");
        };
        assert!(args.target.is_none());
        assert!(args.check);
    }

    #[test]
    fn release_publish_commands_parse() {
        for args in [
            [
                "ngit",
                "release",
                "publish",
                "1.8.0",
                "--asset-event",
                "deadbeef",
                "--commit",
                "HEAD~1",
                "--json",
            ]
            .as_slice(),
            [
                "ngit",
                "release",
                "publish",
                "1.8.0",
                "--asset",
                "linux-x86_64=https://example.com/ngit.tar.gz",
                "--json",
            ]
            .as_slice(),
        ] {
            Cli::try_parse_from(args)
                .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
        }
    }

    #[test]
    fn zapstore_relay_is_available_to_release_mutations() {
        for args in [
            "ngit release app init --name ngit --zapstore-relay",
            "ngit release app link ngit --edit --zapstore-relay",
            "ngit release publish 1.8.0 --asset-event deadbeef --zapstore-relay",
            "ngit release asset add ngit@1.8.0 --event deadbeef --edit --zapstore-relay",
        ] {
            Cli::try_parse_from(args.split_ascii_whitespace())
                .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
        }
    }

    #[test]
    fn release_asset_commands_parse() {
        for args in [
            [
                "ngit",
                "release",
                "asset",
                "list",
                "ngit@1.8.0",
                "--json",
                "--offline",
            ]
            .as_slice(),
            [
                "ngit",
                "release",
                "asset",
                "view",
                "deadbeef",
                "--json",
                "--offline",
            ]
            .as_slice(),
            [
                "ngit",
                "release",
                "asset",
                "add",
                "ngit@1.8.0",
                "--event",
                "deadbeef",
                "--edit",
                "--json",
            ]
            .as_slice(),
        ] {
            Cli::try_parse_from(args)
                .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"));
        }
    }

    #[test]
    fn release_asset_command_type_is_exposed_to_dispatch() {
        let cli = Cli::try_parse_from(["ngit", "release", "asset", "view", "deadbeef"])
            .expect("asset view should parse");
        let Some(Commands::Release(release)) = cli.command else {
            panic!("expected release command");
        };
        let ReleaseCommands::Asset(asset) = release.release_command else {
            panic!("expected asset command group");
        };
        assert!(matches!(asset.asset_command, ReleaseAssetCommands::View(_)));
    }

    #[test]
    fn release_replacement_commands_require_edit() {
        for args in [
            ["ngit", "release", "app", "link", "ngit"].as_slice(),
            [
                "ngit",
                "release",
                "asset",
                "add",
                "ngit@1.8.0",
                "--event",
                "deadbeef",
            ]
            .as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(args).is_err(),
                "command unexpectedly accepted without --edit: {args:?}"
            );
        }
    }

    #[test]
    fn release_app_link_filters_require_an_author_scope() {
        assert!(Cli::try_parse_from(["ngit", "release", "app", "list", "--unlinked"]).is_err());
        Cli::try_parse_from(["ngit", "release", "app", "list", "--mine", "--unlinked"])
            .expect("--mine --unlinked should parse");
        Cli::try_parse_from([
            "ngit", "release", "app", "list", "--author", "deadbeef", "--linked",
        ])
        .expect("--author --linked should parse");
    }

    #[test]
    fn release_asset_add_requires_exactly_one_source() {
        assert!(
            Cli::try_parse_from(["ngit", "release", "asset", "add", "ngit@1.8.0", "--edit",])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "ngit",
                "release",
                "asset",
                "add",
                "ngit@1.8.0",
                "--url",
                "https://example.com/ngit.tar.gz",
                "--event",
                "deadbeef",
                "--edit",
            ])
            .is_err()
        );
    }

    #[test]
    fn release_asset_add_preserves_detailed_url_metadata() {
        let cli = Cli::try_parse_from([
            "ngit",
            "release",
            "asset",
            "add",
            "ngit@1.8.0",
            "--url",
            "https://example.com/ngit.apk",
            "--platform",
            "android-arm64-v8a",
            "--filename",
            "ngit.apk",
            "--mime",
            "application/vnd.android.package-archive",
            "--android-version-code",
            "42",
            "--supported-nip",
            "82",
            "--edit",
        ])
        .expect("detailed URL asset should parse");
        let Some(Commands::Release(release)) = cli.command else {
            panic!("expected release command");
        };
        let ReleaseCommands::Asset(asset) = release.release_command else {
            panic!("expected release asset command");
        };
        let ReleaseAssetCommands::Add(args) = asset.asset_command else {
            panic!("expected release asset add command");
        };

        assert_eq!(args.filename.as_deref(), Some("ngit.apk"));
        assert_eq!(args.platforms, ["android-arm64-v8a"]);
        assert_eq!(args.android_version_code, Some(42));
        assert_eq!(args.supported_nips, ["82"]);
    }

    #[test]
    fn release_edit_fields_preserve_omission() {
        let cli = Cli::try_parse_from(["ngit", "release", "publish", "1.8.0", "--edit"])
            .expect("release edit should parse");
        let Some(Commands::Release(release)) = cli.command else {
            panic!("expected release command");
        };
        let ReleaseCommands::Publish(args) = release.release_command else {
            panic!("expected release publish command");
        };

        assert!(args.edit);
        assert!(args.channel.is_none());
        assert!(args.notes.is_none());
        assert!(args.released_at.is_none());
        assert!(args.commit.is_none());
    }

    #[test]
    fn release_nested_command_types_are_exposed_to_dispatch() {
        let cli = Cli::try_parse_from(["ngit", "release", "app", "list"])
            .expect("application list should parse");
        let Some(Commands::Release(release)) = cli.command else {
            panic!("expected release command");
        };
        let ReleaseCommands::App(app) = release.release_command else {
            panic!("expected application command group");
        };
        assert!(matches!(app.app_command, ReleaseAppCommands::List(_)));
    }

    #[test]
    fn init_rejects_membership_edit_options() {
        for option in ["--other-maintainers", "--lead-maintainer"] {
            assert!(
                Cli::try_parse_from(["ngit", "init", option, "npub1invalid"]).is_err(),
                "ngit init unexpectedly accepted {option}",
            );
        }
    }

    #[test]
    fn repository_settings_use_explicit_initial_and_edit_flags() {
        for args in [
            [
                "ngit",
                "init",
                "--additional-relay",
                "wss://relay.example.com",
                "--additional-clone",
                "https://git.example.com/repo.git",
            ]
            .as_slice(),
            [
                "ngit",
                "repo",
                "edit",
                "--add-grasp-server",
                "grasp.example.com",
                "--remove-grasp-server",
                "old-grasp.example.com",
                "--add-additional-relay",
                "wss://relay.example.com",
                "--remove-additional-relay",
                "wss://old-relay.example.com",
                "--add-additional-clone",
                "https://git.example.com/repo.git",
                "--remove-additional-clone",
                "https://old-git.example.com/repo.git",
                "--add-hashtag",
                "rust",
                "--remove-hashtag",
                "nostr",
            ]
            .as_slice(),
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|error| panic!("failed to parse: {error}"));
        }

        for removed_flag in [
            "--identifier",
            "--grasp-server",
            "--relay",
            "--clone",
            "--hashtag",
        ] {
            assert!(
                Cli::try_parse_from(["ngit", "repo", "edit", removed_flag, "value"]).is_err(),
                "repo edit unexpectedly accepted removed flag {removed_flag}",
            );
        }
        for removed_flag in ["--relay", "--clone"] {
            assert!(
                Cli::try_parse_from(["ngit", "init", removed_flag, "value"]).is_err(),
                "ngit init unexpectedly accepted removed flag {removed_flag}",
            );
        }
    }

    #[test]
    fn repo_edit_exposes_named_relationship_actions() {
        for args in [
            ["ngit", "repo", "edit", "--add-maintainer", "npub1invalid"].as_slice(),
            [
                "ngit",
                "repo",
                "edit",
                "--remove-maintainer",
                "npub1invalid",
                "--no-lead-maintainer",
            ]
            .as_slice(),
            ["ngit", "repo", "edit", "--lead-maintainer", "npub1invalid"].as_slice(),
            [
                "ngit",
                "repo",
                "edit",
                "--acknowledge-maintainer-change",
                "npub1invalid",
            ]
            .as_slice(),
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|error| panic!("failed to parse: {error}"));
        }

        assert!(
            Cli::try_parse_from([
                "ngit",
                "repo",
                "edit",
                "--add-maintainer",
                "npub1invalid",
                "--remove-maintainer",
                "npub1alsoinvalid",
            ])
            .is_err(),
            "one invocation must not accept two named relationship actions",
        );
        assert!(
            Cli::try_parse_from([
                "ngit",
                "repo",
                "edit",
                "--acknowledge-maintainer-change",
                "npub1invalid",
                "--name",
                "changed too",
            ])
            .is_err(),
            "the history acknowledgement must be standalone",
        );
    }

    #[test]
    fn repo_follow_lead_is_non_interactive() {
        assert!(Cli::try_parse_from(["ngit", "repo", "follow-lead"]).is_ok());
    }

    #[test]
    fn repository_membership_commands_reserve_force() {
        assert!(Cli::try_parse_from(["ngit", "repo", "accept", "--force"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "ngit",
                "repo",
                "edit",
                "--add-maintainer",
                "npub1invalid",
                "--force",
            ])
            .is_ok()
        );
    }
}
