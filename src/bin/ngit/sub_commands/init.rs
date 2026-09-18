use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use console::{Style, Term};
use ngit::{
    accept_maintainership::{grasp_servers_from_user_or_fallback, wait_for_grasp_servers},
    agent_guidance,
    cli_interactor::{
        PromptChoiceParms, PromptConfirmParms, cli_error, cli_error_with_category,
        multi_select_with_custom_value, show_multi_input_prompt_success,
    },
    client::{
        Params, get_events_from_local_cache, get_filter_state_events, get_state_from_cache,
        send_events,
    },
    event_ordering,
    fetch::fetch_refs_from_git_server,
    git::{
        is_git_remote_helper_url,
        nostr_url::{CloneUrl, NostrUrlDecoded},
        validate_git_server_clone_url,
    },
    git_http_auth::{clear_private_git_auth, prepare_private_git_auth},
    list::{list_from_remote, list_from_remotes},
    output_mode::is_quiet,
    repo_ref::{
        apply_grasp_infrastructure, detect_existing_grasp_servers, extract_npub, extract_pks,
        format_grasp_server_url_as_relay_url, is_grasp_server_clone_url, latest_event_repo_ref,
        normalize_grasp_server_url, role_tags_assert_lead, save_repo_config_to_yaml,
    },
    repo_state::RepoState,
    utils::join_with_and,
};
use nostr::prelude::{
    Event, EventId, FromBech32, Kind, PublicKey, RelayUrl, ToBech32, Url, nip01::Coordinate,
    nip19::Nip19Coordinate,
};

use crate::{
    cli::{Cli, SignerParams},
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::{
        Client, Connect, fetching_with_private_discovery, fetching_with_report,
        get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions, nostr_url::convert_clone_url_to_https},
    git_remote_helper::push::{
        create_rejected_refspecs_and_remotes_refspecs, generate_updated_state, pin_push_sources,
    },
    login,
    login::user::{PrivateGitRelayDiscovery, publish_private_git_relay_list},
    push_bookkeeping::{record_accepted_push_refspecs, set_branch_upstream},
    repo_ref::{
        RepoCoordinateSource, RepoRef, ResolvedRepoCoordinate, get_repo_config_from_yaml,
        print_selected_repo, try_resolve_repo_coordinate,
    },
    state_transaction::{LiveOps, ServerForcePolicy, StateTransaction},
    sub_commands::repository_fetch::prepare_account_for_repo_fetch,
};

// ---------------------------------------------------------------------------
// InitState: determines what scenario we're in
// ---------------------------------------------------------------------------

enum InitState {
    /// No coordinate found anywhere (State A)
    Fresh,
    /// Coordinate found but no announcement event on relays (State B)
    CoordinateOnly { coordinate: Nip19Coordinate },
    /// Announcement exists, I am the selected maintainer (State C)
    MyAnnouncement {
        coordinate: Nip19Coordinate,
        repo_ref: RepoRef,
    },
    /// Announcement exists, I'm in the maintainer set (State D)
    CoMaintainer {
        coordinate: Nip19Coordinate,
        repo_ref: RepoRef,
    },
    /// Announcement exists, I'm not in the maintainer set (State E)
    NotListed {
        coordinate: Nip19Coordinate,
        repo_ref: RepoRef,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchMode {
    Init,
    RepoEdit,
    /// `ngit repo accept` repairing an existing self-defer announcement; it
    /// republishes that announcement's hosting and only `--grasp-server` can
    /// change it.
    RepoAccept,
}

impl LaunchMode {
    /// How this command adds hosting, for suggestions that can be run as
    /// printed. `ngit init` declares a complete announcement; `ngit repo edit`
    /// uses targeted add actions; `ngit repo accept` has only its grasp flag.
    fn add_hosting_suggestions(self) -> Vec<&'static str> {
        match self {
            Self::Init => vec![
                "ngit init --grasp-server <URL>",
                "ngit init --additional-relay <URL> --additional-clone <URL>",
            ],
            Self::RepoEdit => vec![
                "ngit repo edit --add-grasp-server <URL>",
                "ngit repo edit --add-additional-relay <URL> --add-additional-clone <URL>",
            ],
            Self::RepoAccept => vec!["ngit repo accept --grasp-server <URL>"],
        }
    }

    /// Whether the command can add a relay or clone URL individually. `ngit
    /// repo accept` cannot, so its refusal must not detail flags it lacks.
    fn has_additional_hosting_flags(self) -> bool {
        !matches!(self, Self::RepoAccept)
    }
}

/// Refuse to publish an announcement that names no way to reach the
/// repository.
///
/// A kind-30617 with no `relays` carries no repository state and no
/// collaboration events; one with no `clone` entry carries no git data. Either
/// way the repository is unreachable, so both `ngit init` and `ngit repo edit`
/// stop before signing rather than replacing a usable announcement with an
/// unusable one. Grasp servers supply both halves at once, which is why they
/// lead the suggestions; a grasp-free repository is fine as long as its
/// additional relays and clone URLs keep both fields populated.
pub(crate) fn validate_announcement_hosting<C, R>(
    clone_urls: &[C],
    relays: &[R],
    mode: LaunchMode,
) -> Result<()> {
    match announcement_hosting_refusal(clone_urls.is_empty(), relays.is_empty(), mode) {
        None => Ok(()),
        Some(refusal) => Err(cli_error(
            refusal.message,
            &refusal.details,
            &refusal.suggestions,
        )),
    }
}

/// What [`validate_announcement_hosting`] reports. Kept separate from
/// `cli_error`, which renders straight to stderr and drops everything but the
/// message from the returned error, so the whole refusal stays assertable.
struct AnnouncementHostingRefusal {
    message: &'static str,
    /// Flags that can supply the missing half, with what each one is for.
    details: Vec<(&'static str, &'static str)>,
    /// Commands runnable as printed.
    suggestions: Vec<&'static str>,
}

fn announcement_hosting_refusal(
    missing_clone: bool,
    missing_relay: bool,
    mode: LaunchMode,
) -> Option<AnnouncementHostingRefusal> {
    let message = match (missing_relay, missing_clone) {
        (false, false) => return None,
        (true, true) => "a repository announcement needs at least one relay and one git server",
        (true, false) => "a repository announcement needs at least one relay",
        (false, true) => "a repository announcement needs at least one git server",
    };

    let mut details: Vec<(&'static str, &'static str)> = vec![(
        "--grasp-server <URL>",
        "hosts your nostr and git data together",
    )];
    if missing_relay && mode.has_additional_hosting_flags() {
        details.push((
            "--additional-relay <URL>",
            "where your nostr data is hosted",
        ));
    }
    if missing_clone && mode.has_additional_hosting_flags() {
        details.push(("--additional-clone <URL>", "where your git data is hosted"));
    }

    Some(AnnouncementHostingRefusal {
        message,
        details,
        suggestions: mode.add_hosting_suggestions(),
    })
}

/// Network state used to ensure a named removal still applies to the graph
/// that its caller previewed before the internal announcement publisher runs.
pub(crate) struct RepoEditPreflight {
    removed_maintainer: PublicKey,
    announcement_frontier: HashMap<PublicKey, EventId>,
}

impl RepoEditPreflight {
    pub(crate) fn removal(repo_ref: &RepoRef, removed_maintainer: PublicKey) -> Self {
        Self {
            removed_maintainer,
            announcement_frontier: announcement_frontier(repo_ref),
        }
    }

    fn require_unchanged(&self, repo_ref: &RepoRef) -> Result<()> {
        if self.announcement_frontier == announcement_frontier(repo_ref) {
            return Ok(());
        }
        Err(cli_error_with_category(
            "membership_preflight_changed",
            "the repository membership changed after the removal preview",
            &[],
            &["fetch and rerun the named removal against the current membership"],
        ))
    }
}

fn announcement_frontier(repo_ref: &RepoRef) -> HashMap<PublicKey, EventId> {
    repo_ref
        .events
        .values()
        .map(|event| (event.pubkey, event.id))
        .collect()
}

fn may_suggest_skill(state: &InitState) -> bool {
    matches!(
        state,
        InitState::Fresh
            | InitState::CoordinateOnly { .. }
            | InitState::MyAnnouncement { .. }
            | InitState::CoMaintainer { .. }
    )
}

impl InitState {
    fn coordinate(&self) -> Option<&Nip19Coordinate> {
        match self {
            Self::Fresh => None,
            Self::CoordinateOnly { coordinate }
            | Self::MyAnnouncement { coordinate, .. }
            | Self::CoMaintainer { coordinate, .. }
            | Self::NotListed { coordinate, .. } => Some(coordinate),
        }
    }

    fn repo_ref(&self) -> Option<&RepoRef> {
        match self {
            Self::Fresh | Self::CoordinateOnly { .. } => None,
            Self::MyAnnouncement { repo_ref, .. }
            | Self::CoMaintainer { repo_ref, .. }
            | Self::NotListed { repo_ref, .. } => Some(repo_ref),
        }
    }

    /// Extract my own announcement's `RepoRef` from the events map.
    /// Returns `None` if no coordinate, no announcement, or I have no event.
    fn my_repo_ref(&self, my_pubkey: &PublicKey) -> Option<RepoRef> {
        self.repo_ref()
            .and_then(|rr| my_event_repo_ref(rr, my_pubkey))
    }

    fn has_coordinate(&self) -> bool {
        !matches!(self, Self::Fresh)
    }
}

struct ResolvedFields {
    identifier: String,
    name: String,
    description: String,
    git_servers: Vec<String>,
    relays: Vec<RelayUrl>,
    web: Vec<String>,
    upstream: Vec<Vec<String>>,
    maintainers: Vec<PublicKey>,
    earliest_unique_commit: String,
    blossoms: Vec<Url>,
    hashtags: Vec<String>,
    private: bool,
    selected_grasp_servers: Vec<String>,
    /// Existing announcements for this coordinate, retained so a republish can
    /// order itself after the current NIP-01 winner.
    announcement_events: HashMap<Nip19Coordinate, nostr::prelude::Event>,
    /// Tags from the source announcement that aren't in ngit's known
    /// allowlist ([`is_known_tag_name`]), preserved verbatim on
    /// republish so that tags added by a future ngit version or a
    /// third-party tool aren't silently dropped. Cleared when
    /// `--clean` is passed. See [`SubCommandArgs::clean`].
    extra_tags: Vec<nostr::prelude::Tag>,
    /// NIP-34 indexed role tags from **my own** existing announcement,
    /// supplying the start/end history boundaries and moderator (`o`)
    /// entries that `RepoRef::generate_role_tags` builds the emitted
    /// role tags from. Sourced from my announcement only — like
    /// `maintainers`, each maintainer's role record is their own
    /// statement. Deliberately unaffected by `--clean`: role tags are
    /// ngit-known tags, and dropping them would silently discard
    /// moderators and restart every member's role history.
    role_tags: Vec<nostr::prelude::Tag>,
    /// The lead maintainer to assert with the NIP-34 `M` role:
    /// `--lead-maintainer`, or my own announcement's existing assertion
    /// while the lead remains in `maintainers`. `None` emits only `m` tags.
    lead: Option<PublicKey>,
    preserve_selected_coordinate: bool,
}

/// Apply the resolved lead (`M` role) to the maintainer listing.
///
/// Without `--lead-maintainer` the author's own announcement's lead
/// assertion is carried forward while that pubkey remains in the listing.
/// Specifying yourself keeps the full listing and emits you as `M`.
/// Specifying someone else follows NIP-34's SHOULD — the announcement then
/// keeps only the author and lead active. When that change would remove a
/// pubkey from the active graph because the lead does not cover them, refuse
/// the unnamed removal.
fn apply_lead_to_maintainers(
    lead_arg: Option<PublicKey>,
    my_pubkey: &PublicKey,
    maintainers: Vec<PublicKey>,
    my_ref: Option<&RepoRef>,
    consolidated: Option<&RepoRef>,
) -> Result<(Vec<PublicKey>, Option<PublicKey>)> {
    let Some(lead) = lead_arg else {
        let inherited = my_ref
            .and_then(|mr| mr.lead)
            .filter(|lead| maintainers.contains(lead));
        return Ok((maintainers, inherited));
    };
    if lead == *my_pubkey {
        return Ok((maintainers, Some(lead)));
    }
    let listing = vec![*my_pubkey, lead];
    let dropped: Vec<String> = members_losing_authorized_status_after_lead_change(
        &listing,
        &lead,
        my_pubkey,
        my_ref,
        consolidated,
    )
    .iter()
    .map(|pk| pk.to_bech32().unwrap_or_else(|_| pk.to_hex()))
    .collect();
    if !dropped.is_empty() {
        let lead_npub = lead.to_bech32().unwrap_or_else(|_| lead.to_hex());
        let mut suggestions = vec![format!(
            "ask {lead_npub} to add these maintainers first: {}",
            dropped.join(", ")
        )];
        suggestions.extend(
            dropped
                .iter()
                .map(|npub| format!("ngit repo edit --remove-maintainer {npub}")),
        );
        let suggestion_refs: Vec<&str> = suggestions.iter().map(String::as_str).collect();
        return Err(cli_error(
            &format!(
                "setting {lead_npub} as lead would remove specific maintainers from your active graph: {}",
                dropped.join(", ")
            ),
            &[],
            &suggestion_refs,
        ));
    }
    Ok((listing, Some(lead)))
}

/// Pubkeys that would lose authorized status when the author's active roles
/// change to `[me, lead]`, excluding those the lead's own announcement keeps
/// listed *with authority*. Per NIP-34, the lead's listing counts as cover
/// only when the lead is already confirmed or their announcement acknowledges
/// the author, making the relationship reciprocal when this update is
/// published. An unconfirmed, non-reciprocal lead's announcement covers
/// nobody.
///
/// Conservative over-approximation: a drop kept listed by another remaining
/// member's announcement still gates even though that listing may keep the
/// pubkey confirmed. Under a lead, non-lead members SHOULD list only
/// themselves and the lead, so such cover is transitional at best.
fn members_losing_authorized_status_after_lead_change(
    listing: &[PublicKey],
    lead: &PublicKey,
    my_pubkey: &PublicKey,
    my_ref: Option<&RepoRef>,
    consolidated: Option<&RepoRef>,
) -> Vec<PublicKey> {
    let lead_lists: Vec<PublicKey> = consolidated
        .and_then(|rr| {
            let event = rr.events.values().find(|e| e.pubkey == *lead)?;
            let lead_ref = RepoRef::try_from((event.clone(), None)).ok()?;
            (rr.is_authorized_maintainer(lead) || lead_ref.maintainers.contains(my_pubkey))
                .then_some(lead_ref.maintainers)
        })
        .unwrap_or_default();
    my_ref
        .map(|mr| mr.maintainers.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|pk| !listing.contains(pk) && !lead_lists.contains(pk))
        .collect()
}

/// Extract my own announcement's `RepoRef` from the events map.
fn my_event_repo_ref(repo_ref: &RepoRef, my_pubkey: &PublicKey) -> Option<RepoRef> {
    repo_ref
        .events
        .values()
        .find(|e| e.pubkey == *my_pubkey)
        .and_then(|e| RepoRef::try_from((e.clone(), None)).ok())
}

/// Check if a grasp-format clone URL belongs to the given public key.
pub(super) fn is_my_grasp_clone_url(url: &str, my_pubkey: &PublicKey) -> bool {
    if !is_grasp_server_clone_url(url) {
        return false;
    }
    if let Ok(npub) = extract_npub(url) {
        if let Ok(url_pk) = PublicKey::from_bech32(npub) {
            return url_pk == *my_pubkey;
        }
    }
    false
}

/// Check if a relay URL corresponds to one of the given grasp servers.
pub(super) fn is_grasp_derived_relay(relay: &str, grasp_servers: &[String]) -> bool {
    let Ok(relay_normalized) = normalize_grasp_server_url(relay) else {
        return false;
    };
    grasp_servers.iter().any(|gs| {
        normalize_grasp_server_url(gs).is_ok_and(|gs_normalized| gs_normalized == relay_normalized)
    })
}

pub(super) fn is_grasp_derived_clone(clone: &str, grasp_servers: &[String]) -> bool {
    let Ok(clone_server) = normalize_grasp_server_url(clone) else {
        return false;
    };
    grasp_servers.iter().any(|grasp_server| {
        normalize_grasp_server_url(grasp_server)
            .is_ok_and(|grasp_server| grasp_server == clone_server)
    })
}

fn dir_name_fallback() -> String {
    env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default()
}

fn identifier_from_name(name: &str) -> String {
    name.replace(' ', "-")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.eq(&'/') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn build_gitworkshop_url(
    public_key: &PublicKey,
    identifier: &str,
    first_relay: Option<&RelayUrl>,
) -> String {
    NostrUrlDecoded {
        original_string: String::new(),
        coordinate: Nip19Coordinate {
            coordinate: Coordinate {
                public_key: *public_key,
                kind: Kind::GitRepoAnnouncement,
                identifier: identifier.to_string(),
            },
            relays: first_relay.into_iter().cloned().collect(),
        },
        protocol: None,
        ssh_key_file: None,
        nip05: None,
    }
    .to_string()
    .replace("nostr://", "https://gitworkshop.dev/")
}

/// Resolve the `web` field from args, existing announcement, or gitworkshop
/// default.
fn resolve_web(
    args_web: &[String],
    state: &InitState,
    identifier: &str,
    gitworkshop_url: &str,
) -> Vec<String> {
    if !args_web.is_empty() {
        return args_web.to_vec();
    }
    if let Some(rr) = state.repo_ref() {
        let latest_web = latest_event_repo_ref(rr).map_or_else(|| rr.web.clone(), |lr| lr.web);
        let joined = latest_web.join(" ");
        // replace legacy gitworkshop.dev url format
        if joined.contains(&format!("https://gitworkshop.dev/repo/{identifier}")) {
            return vec![gitworkshop_url.to_string()];
        }
        return latest_web;
    }
    vec![gitworkshop_url.to_string()]
}

/// Normalize and validate a hashtag: lowercase, strip leading `#`, allow only
/// `a-z`, `0-9`, and `-` (no leading/trailing/consecutive hyphens).
pub(super) fn validate_hashtag(s: &str) -> Result<String> {
    let trimmed = s.trim().trim_start_matches('#').to_lowercase();
    if trimmed.is_empty() {
        bail!("hashtag cannot be empty");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("hashtag can only contain lowercase letters (a-z), digits (0-9), and hyphens (-)");
    }
    if trimmed.starts_with('-') || trimmed.ends_with('-') {
        bail!("hashtag cannot start or end with a hyphen");
    }
    if trimmed.contains("--") {
        bail!("hashtag cannot contain consecutive hyphens");
    }
    Ok(trimmed)
}

/// Resolve the `hashtags` field from args or existing announcement.
fn resolve_hashtags(
    args_hashtag: &[String],
    replace_hashtags: bool,
    state: &InitState,
) -> Result<Vec<String>> {
    if replace_hashtags || !args_hashtag.is_empty() {
        return args_hashtag.iter().map(|h| validate_hashtag(h)).collect();
    }
    if let Some(rr) = state.repo_ref() {
        return Ok(latest_event_repo_ref(rr).map_or_else(|| rr.hashtags.clone(), |lr| lr.hashtags));
    }
    Ok(vec![])
}

/// How `--grasp-server` was supplied.
///
/// clap collapses "flag absent" and "flag supplied with only empty values"
/// into the same `Vec<String>`, so the distinction is drawn here. Hosting a
/// repository on no grasp server at all has to be stated
/// (`--grasp-server ""`); it is never a side effect of which other flags
/// happened to be passed.
#[derive(Debug, Eq, PartialEq)]
enum GraspServerArgs {
    /// The flag was not supplied: preferences and defaults apply.
    Unspecified,
    /// The flag was supplied. An empty list is an explicit opt-out.
    Explicit(Vec<String>),
}

/// Interpret the repeated `--grasp-server` values. Blank values carry no URL,
/// so a flag supplied with only blanks is an explicit opt-out.
fn interpret_grasp_server_args(values: &[String]) -> GraspServerArgs {
    if values.is_empty() {
        return GraspServerArgs::Unspecified;
    }
    GraspServerArgs::Explicit(
        values
            .iter()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .collect(),
    )
}

/// The grasp servers that follow from what the user stated and from what the
/// repository already declares, without consulting preferences or system
/// defaults.
///
/// `None` means nothing has been stated yet, so the caller should fall back to
/// the user's preferred grasp servers or the system defaults. Note the
/// difference between the two empty results: `Some(vec![])` is a decision (an
/// explicit opt-out, or an existing announcement of mine that declares its own
/// non-grasp hosting), while `None` is the absence of one.
fn grasp_servers_before_fallback(
    selection: &GraspServerArgs,
    my_ref: Option<&RepoRef>,
    args_relays: &[String],
    args_clones: &[String],
    identifier: &str,
) -> Option<Vec<String>> {
    if let GraspServerArgs::Explicit(servers) = selection {
        return Some(servers.clone());
    }

    // My announcement's own grasp servers, then any implied by the URLs
    // supplied on this invocation. Detected separately so that supplying
    // `--additional-clone` doesn't hide the servers my announcement already
    // declares.
    let mut detected = detect_existing_grasp_servers(my_ref, &[], &[], identifier);
    for server in detect_existing_grasp_servers(None, args_relays, args_clones, identifier) {
        if !detected.contains(&server) {
            detected.push(server);
        }
    }
    if !detected.is_empty() {
        return Some(detected);
    }

    // An announcement of mine that carries no grasp servers states its own
    // hosting; republishing it must not graft defaults onto it. Defaulting is
    // for repositories that have yet to say anything.
    if my_ref.is_some() {
        return Some(vec![]);
    }

    None
}

/// Resolve which grasp servers to use. Handles flag overrides, detection from
/// existing URLs, user grasp list / system fallbacks, and interactive
/// prompting.
fn resolve_grasp_servers(
    args: &SubCommandArgs,
    cli: &Cli,
    state: &InitState,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    identifier: &str,
    interactive: bool,
) -> Result<Vec<String>> {
    if args.replace_grasp_servers {
        return Ok(args.grasp_server.clone());
    }

    // Use my own announcement (not the consolidated union) for grasp detection.
    // Infrastructure is personal — each maintainer has their own servers.
    let my_ref = state.my_repo_ref(&user_ref.public_key);

    if let Some(servers) = grasp_servers_before_fallback(
        &interpret_grasp_server_args(&args.grasp_server),
        my_ref.as_ref(),
        &args.additional_relay,
        &args.additional_clone,
        identifier,
    ) {
        return Ok(servers);
    }

    if !interactive || cli.defaults || state.has_coordinate() || cli.force {
        // Nothing stated and nothing to detect: use the user's grasp list (or
        // the selected maintainer's servers as fallback), then system defaults.
        // For co-maintainer state, pass the repo_ref so the selected
        // maintainer's grasp servers can be used as a fallback.
        let selected_maintainer_repo_ref = if matches!(state, InitState::CoMaintainer { .. }) {
            state.repo_ref()
        } else {
            None
        };
        return Ok(grasp_servers_from_user_or_fallback(
            user_ref,
            selected_maintainer_repo_ref,
            client,
        ));
    }

    // Interactive prompt. `grasp_servers_before_fallback` found nothing to
    // detect, so the options are the user's grasp list and the system defaults.
    let mut options: Vec<String> = vec![];
    let mut selections: Vec<bool> = vec![];
    for user_grasp_option in &user_ref.grasp_list.urls {
        if !options
            .iter()
            .any(|option| option.contains(user_grasp_option.as_str()))
        {
            options.push(user_grasp_option.to_string());
            selections.push(true);
        }
    }
    let empty = options.is_empty();
    let fallback_grasp_servers = client.get_grasp_default_set();
    for fallback in fallback_grasp_servers {
        if !options.iter().any(|option| option.contains(fallback)) {
            options.push(fallback.clone());
            selections.push(empty);
        }
    }
    let selected = multi_select_with_custom_value(
        "grasp servers (ideally use between 2-4)",
        "grasp server",
        options,
        selections,
        normalize_grasp_server_url,
    )?;
    show_multi_input_prompt_success("grasp servers", &selected);
    Ok(selected)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validation for State A (Fresh): no existing coordinate.
fn validate_fresh(cli: &Cli, args: &SubCommandArgs, user_has_grasp_list: bool) -> Result<()> {
    // -d or -f with no substantive flags: proceed with all defaults
    if !args.has_substantive_flags(cli.repo_relay_only) && (cli.defaults || cli.force) {
        return Ok(());
    }

    // Substantive flags provided: -d fills any gaps
    if cli.defaults {
        return Ok(());
    }

    // Validate essential fields
    let mut missing: Vec<(&str, &str)> = Vec::new();

    let missing_name = args.identifier.is_none() && args.name.is_none();
    if missing_name {
        missing.push(("--name <NAME>", "repository name or identifier"));
    }

    // Only an absent `--grasp-server` leaves hosting unstated. A supplied one
    // — named servers or the empty opt-out — is an answer; whether that answer
    // leaves the announcement reachable is settled by
    // `validate_announcement_hosting` once the fields are resolved.
    let grasp_unstated = matches!(
        interpret_grasp_server_args(&args.grasp_server),
        GraspServerArgs::Unspecified
    );
    let has_both_relays_and_clone_url =
        !args.additional_relay.is_empty() && !args.additional_clone.is_empty();
    if grasp_unstated && !user_has_grasp_list && !has_both_relays_and_clone_url {
        missing.push((
            "--grasp-server <URL>...",
            "where your git+nostr data is hosted",
        ));
    }

    if missing.is_empty() {
        return Ok(());
    }

    let message = if missing.len() == 1 {
        let (flag, desc) = missing[0];
        format!("missing {flag} ({desc})")
    } else {
        "missing required fields".to_string()
    };

    let mut details: Vec<(&str, &str)> = if missing.len() > 1 {
        missing.clone()
    } else {
        vec![]
    };

    details.push(("-d, --defaults", "or just use sensible defaults"));
    let name_part = if missing_name {
        " --name \"My Project\""
    } else {
        ""
    };
    let suggestion =
        format!("ngit init{name_part} --description \"my project description\" --defaults");

    Err(cli_error(&message, &details, &[&suggestion]))
}

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct SubCommandArgs {
    #[clap(long, alias = "title")]
    /// name of repository (preferred over --identifier); --title is an alias
    pub(crate) name: Option<String>,
    #[clap(long)]
    /// shortname with no spaces or special characters
    pub(crate) identifier: Option<String>,
    #[clap(long)]
    /// optional description
    pub(crate) description: Option<String>,
    #[clap(short, long, value_parser, num_args = 1..)]
    /// where your git+nostr data is hosted; defaults apply when omitted, pass
    /// an empty value (--grasp-server "") to host without a grasp server
    pub(crate) grasp_server: Vec<String>,
    #[clap(long = "additional-relay", value_parser, num_args = 1..)]
    /// additional relays beyond grasp servers
    pub(crate) additional_relay: Vec<String>,
    #[clap(long = "additional-clone")]
    /// additional git server URLs beyond grasp servers
    pub(crate) additional_clone: Vec<String>,
    #[clap(long, value_parser, num_args = 1..)]
    /// homepage
    pub(crate) web: Vec<String>,
    #[clap(short = 'u', long = "u", alias = "upstream", value_parser, num_args = 1..)]
    /// informational NIP-34 subordinate-fork `u` tag fields
    pub(crate) upstream: Vec<String>,
    /// Internal named-action projection used by `ngit repo edit`. It is not
    /// part of the `ngit init` command surface.
    #[clap(skip)]
    pub(crate) other_maintainers: Vec<String>,
    /// Internal governance choice used by `ngit repo edit`.
    #[clap(skip)]
    pub(crate) lead_maintainer: Option<String>,
    /// Whether `other_maintainers` is an exact named-action result, including
    /// the empty result of removing the final co-maintainer.
    #[clap(skip)]
    pub(crate) replace_maintainers: bool,
    /// Whether an exact internal replacement already contains the author's
    /// pubkey when their repaired role remains active. Ordinary edits leave
    /// this false and receive the historical implicit author insertion.
    #[clap(skip)]
    pub(crate) replacement_lists_author: bool,
    /// Permit the internal repository-edit recovery path to replace an
    /// announcement for an author who currently has no resolvable role.
    #[clap(skip)]
    pub(crate) allow_self_defer_repair: bool,
    /// Explicitly clear this announcement's active lead declaration.
    #[clap(skip)]
    pub(crate) clear_lead: bool,
    /// Exact role history prepared by a named lifecycle action.
    #[clap(skip)]
    pub(crate) role_tags: Option<Vec<nostr::prelude::Tag>>,
    /// Keep the checkout rooted at its selected maintainer after publication.
    #[clap(skip)]
    pub(crate) preserve_selected_coordinate: bool,
    /// Treat `grasp_server` as an exact internal replacement, including an
    /// empty list. Used by targeted `ngit repo edit` actions.
    #[clap(skip)]
    pub(crate) replace_grasp_servers: bool,
    /// Treat `additional_relay` as an exact internal replacement.
    #[clap(skip)]
    pub(crate) replace_additional_relays: bool,
    /// Treat `additional_clone` as an exact internal replacement.
    #[clap(skip)]
    pub(crate) replace_additional_clones: bool,
    /// Treat `hashtag` as an exact internal replacement.
    #[clap(skip)]
    pub(crate) replace_hashtags: bool,
    #[clap(long, value_parser, num_args = 1..)]
    /// hashtags for repository discovery
    pub(crate) hashtag: Vec<String>,
    #[clap(long)]
    /// usually root commit but will be more recent commit for forks
    pub(crate) earliest_unique_commit: Option<String>,
    #[clap(long)]
    /// drop unknown tags from the existing announcement when republishing
    /// (default is to preserve them so tags added by future ngit versions
    /// or third-party tools aren't silently lost)
    pub(crate) clean: bool,
    #[clap(long, conflicts_with = "public")]
    /// mark the repository private and restrict discovery to its repository
    /// relays
    pub(crate) private: bool,
    #[clap(long, conflicts_with = "private")]
    /// remove the private marker from this maintainer's announcement
    pub(crate) public: bool,
}

impl SubCommandArgs {
    fn has_substantive_flags(&self, repo_relay_only: bool) -> bool {
        self.name.is_some()
            || self.identifier.is_some()
            || self.description.is_some()
            || !self.additional_clone.is_empty()
            || !self.additional_relay.is_empty()
            || !self.grasp_server.is_empty()
            || self.replace_grasp_servers
            || self.replace_additional_relays
            || self.replace_additional_clones
            || self.replace_hashtags
            || !self.web.is_empty()
            || !self.upstream.is_empty()
            || self.replace_maintainers
            || self.lead_maintainer.is_some()
            || !self.hashtag.is_empty()
            || self.earliest_unique_commit.is_some()
            || repo_relay_only
            || self.clean
            || self.private
            || self.public
    }
}

// ---------------------------------------------------------------------------
// Pre/post-fetch validation
// ---------------------------------------------------------------------------

fn validate_pre_fetch(
    cli: &Cli,
    args: &SubCommandArgs,
    mode: LaunchMode,
    repo_coordinate: Option<&Nip19Coordinate>,
    user_has_grasp_list: bool,
    cached_repo_ref: Option<&RepoRef>,
    my_pubkey: &PublicKey,
) -> Result<()> {
    // Interactive mode bypasses pre-fetch validation
    if cli.interactive {
        return Ok(());
    }

    // If no coordinate exists, we're in State A (Fresh) - validate now
    if repo_coordinate.is_none() {
        return validate_fresh(cli, args, user_has_grasp_list);
    }

    // Repository edits retain the existing fast-path validation. Public init
    // waits for the post-fetch state so it can give the correct create/edit/
    // accept boundary error even when the cache is stale.
    if mode == LaunchMode::Init {
        return Ok(());
    }

    // If we have cached data and it's MyAnnouncement state, validate early
    if let (Some(coord), Some(repo_ref)) = (repo_coordinate, cached_repo_ref) {
        if coord.coordinate.public_key == *my_pubkey {
            // MyAnnouncement state - validate before network fetch
            if let Some(new_id) = &args.identifier {
                if *new_id != repo_ref.identifier && !cli.force {
                    let suggestion = format!("ngit init --identifier {new_id} --force");
                    return Err(cli_error(
                        "changing identifier creates a new repository",
                        &[],
                        &[&suggestion],
                    ));
                }
            }
            if !args.has_substantive_flags(cli.repo_relay_only) && !cli.force {
                return Err(cli_error(
                    "no arguments specified, use --force to publish with new timestamp",
                    &[],
                    &["ngit init --force"],
                ));
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_post_fetch(
    cli: &Cli,
    args: &SubCommandArgs,
    mode: LaunchMode,
    state: &InitState,
    my_pubkey: PublicKey,
) -> Result<()> {
    if mode == LaunchMode::Init {
        return match state {
            InitState::Fresh => Ok(()),
            InitState::CoordinateOnly { coordinate } => {
                if cli.force {
                    Ok(())
                } else {
                    let id = &coordinate.identifier;
                    Err(cli_error(
                        &format!(
                            "no announcement found for coordinate '{id}'\n\n\
                             \x20 This could be a relay or network issue. Only proceed with --force\n\
                             \x20 if you are sure there isn't an existing announcement event."
                        ),
                        &[],
                        &["ngit init --force"],
                    ))
                }
            }
            InitState::MyAnnouncement { .. } => Err(cli_error(
                "this repository has already been initialized",
                &[],
                &["edit your existing announcement with `ngit repo edit`"],
            )),
            InitState::CoMaintainer { repo_ref, .. } => {
                if repo_ref.confirmed_maintainers().contains(&my_pubkey) {
                    Err(cli_error(
                        "this repository has already been initialized",
                        &[],
                        &["edit your existing announcement with `ngit repo edit`"],
                    ))
                } else {
                    Err(cli_error(
                        "ngit init cannot accept an existing repository invitation",
                        &[],
                        &["accept the invitation with `ngit repo accept`"],
                    ))
                }
            }
            InitState::NotListed { .. } => Err(cli_error(
                "ngit init cannot join or replace an existing repository",
                &[],
                &["clone or select the repository without publishing an announcement"],
            )),
        };
    }

    // Interactive repository editing retains its prompting behavior after the
    // public init boundary above has rejected every existing announcement.
    if cli.interactive {
        return Ok(());
    }

    match state {
        InitState::Fresh => {
            // Already validated in pre-fetch
            Ok(())
        }
        InitState::CoordinateOnly { coordinate } => {
            if cli.force {
                Ok(())
            } else {
                let id = &coordinate.identifier;
                Err(cli_error(
                    &format!(
                        "no announcement found for coordinate '{id}'\n\n\
                         \x20 This could be a relay or network issue. Only proceed with --force\n\
                         \x20 if you are sure there isn't an existing announcement event."
                    ),
                    &[],
                    &["ngit init --force"],
                ))
            }
        }
        InitState::MyAnnouncement { repo_ref, .. } => {
            if let Some(new_id) = &args.identifier {
                if *new_id != repo_ref.identifier && !cli.force {
                    let suggestion = format!("ngit init --identifier {new_id} --force");
                    return Err(cli_error(
                        "changing identifier creates a new repository",
                        &[],
                        &[&suggestion],
                    ));
                }
            }
            if !args.has_substantive_flags(cli.repo_relay_only) && !cli.force {
                return Err(cli_error(
                    "no arguments specified, use --force to publish with new timestamp",
                    &[],
                    &["ngit init --force"],
                ));
            }
            Ok(())
        }
        InitState::CoMaintainer { repo_ref, .. } => {
            if let Some(new_id) = &args.identifier {
                if *new_id != repo_ref.identifier && !cli.force {
                    let suggestion = format!("ngit init --identifier {new_id} --force");
                    return Err(cli_error(
                        "changing identifier creates a new repository",
                        &[],
                        &[&suggestion],
                    ));
                }
            }
            Ok(())
        }
        InitState::NotListed { .. } => {
            if args.allow_self_defer_repair || cli.force {
                Ok(())
            } else {
                Err(cli_error(
                    "you are not listed as a maintainer",
                    &[],
                    &["ngit init --force"],
                ))
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn resolve_fields(
    state: &InitState,
    user_ref: &ngit::login::user::UserRef,
    args: &SubCommandArgs,
    cli: &Cli,
    git_repo: &Repo,
    root_commit: &str,
    client: &Client,
    repo_config_result: &Result<ngit::repo_ref::RepoConfigYaml>,
    interactive: bool,
) -> Result<ResolvedFields> {
    let my_pubkey = &user_ref.public_key;

    // Shared lookups used by multiple fields below
    let latest = state.repo_ref().and_then(latest_event_repo_ref);
    let my_ref = state.my_repo_ref(my_pubkey);

    // --- Identifier default ---
    let identifier_default = if let Some(coord) = state.coordinate() {
        coord.identifier.clone()
    } else if let Ok(config) = repo_config_result {
        if let Some(id) = &config.identifier {
            id.clone()
        } else {
            dir_name_fallback()
        }
    } else {
        dir_name_fallback()
    };

    // --- Name ---
    let name_default = if let Some(ref lr) = latest {
        lr.name.clone()
    } else if let Some(coord) = state.coordinate() {
        coord.identifier.clone()
    } else {
        dir_name_fallback()
    };

    let name = if let Some(v) = &args.name {
        v.clone()
    } else if interactive {
        Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo name")
                .with_default(name_default.clone())
                .with_flag_name("--name"),
        )?
    } else {
        name_default.clone()
    };

    // --- Description ---
    let description_default = latest
        .as_ref()
        .map_or_else(String::new, |lr| lr.description.clone());

    let description = if let Some(v) = &args.description {
        v.clone()
    } else if interactive {
        Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo description (one sentence)")
                .optional()
                .with_default(description_default.clone())
                .with_flag_name("--description"),
        )?
    } else {
        description_default
    };

    // --- Simple mode (interactive only) ---
    let simple_mode = if !interactive
        || (!args.additional_clone.is_empty() && !args.additional_relay.is_empty())
    {
        false // not used in non-interactive, but avoids Option
    } else {
        Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt("config mode")
                .with_choices(vec![
                    "simple - all you need".to_string(),
                    "advanced - all the dials and switches".to_string(),
                ])
                .with_default(0),
        )? == 0
    };

    // --- Identifier ---
    let identifier = if let Some(id) = &args.identifier {
        id.clone()
    } else if state.has_coordinate() {
        identifier_default.clone()
    } else if !interactive || cli.defaults {
        if args.name.is_some() && !state.has_coordinate() {
            identifier_from_name(&name)
        } else {
            identifier_default.clone()
        }
    } else {
        let id_default = if args.name.is_some() || name != name_default {
            identifier_from_name(&name)
        } else {
            identifier_default.clone()
        };
        Interactor::default().input(
            PromptInputParms::default()
                .with_prompt("repo identifier")
                .with_default(id_default)
                .with_flag_name("--identifier"),
        )?
    };

    // --- Grasp servers ---
    let selected_grasp_servers =
        resolve_grasp_servers(args, cli, state, user_ref, client, &identifier, interactive)?;

    // --- Base infrastructure (flag > my event > fallback) ---
    // Grasp-derived infrastructure (my clone URLs, relays) is handled
    // by apply_grasp_infrastructure below. Defaults here are *additional*
    // infrastructure only. My own grasp-format clone URLs are filtered out so
    // they get re-derived from the resolved grasp servers. Grasp-format clone
    // URLs belonging to other maintainers are kept as additional git servers.
    let no_state = git_repo
        .get_git_config_item("nostr.nostate", None)
        .ok()
        .flatten()
        .is_some_and(|s| s == "true");

    // Detect my grasp servers from my existing announcement (for filtering)
    let my_existing_grasp_servers: Vec<String> = my_ref
        .as_ref()
        .map(|mr| detect_existing_grasp_servers(Some(mr), &[], &[], &identifier))
        .unwrap_or_default();

    let git_servers_default = if let Some(ref mr) = my_ref {
        // Keep non-grasp URLs and grasp URLs from other maintainers;
        // filter out my own grasp-derived clone URLs (re-derived from grasp servers)
        mr.git_server
            .iter()
            .filter(|url| {
                !is_my_grasp_clone_url(url, my_pubkey)
                    || !is_grasp_derived_clone(url, &my_existing_grasp_servers)
            })
            .cloned()
            .collect()
    } else if no_state {
        // Only fall back to origin URL when nostate is set (user pushes directly
        // to a traditional git server rather than through grasp servers)
        if let Ok(url) = git_repo.get_origin_url() {
            if let Ok(fetch_url) = convert_clone_url_to_https(&url) {
                vec![fetch_url]
            } else if url.starts_with("nostr://") {
                vec![]
            } else {
                vec![url]
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let relays_default = if let Some(ref mr) = my_ref {
        // Keep relays that don't correspond to my grasp servers
        // (grasp-derived relays are re-added by apply_grasp_infrastructure)
        mr.relays
            .iter()
            .map(std::string::ToString::to_string)
            .filter(|r| !is_grasp_derived_relay(r, &my_existing_grasp_servers))
            .collect()
    } else if let Ok(config) = repo_config_result {
        config.relays.clone()
    } else {
        vec![]
    };

    let mut git_servers = if !args.replace_additional_clones && args.additional_clone.is_empty() {
        git_servers_default
    } else {
        args.additional_clone.clone()
    };
    let mut relay_strings = if !args.replace_additional_relays && args.additional_relay.is_empty() {
        relays_default
    } else {
        args.additional_relay.clone()
    };

    apply_grasp_infrastructure(
        &selected_grasp_servers,
        &mut git_servers,
        &mut relay_strings,
        &user_ref.public_key,
        &identifier,
    )?;

    // --- Interactive: nostr.nostate prompt ---
    if interactive
        && no_state
        && Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("store state on nostr? required for nostr-permissioned git servers")
                .with_default(true),
        )?
    {
        if git_repo
            .get_git_config_item("nostr.nostate", Some(true))
            .unwrap_or(None)
            .is_some()
        {
            git_repo.remove_git_config_item("nostr.nostate", true)?;
        } else {
            git_repo.remove_git_config_item("nostr.nostate", false)?;
        }
    }

    // --- Git servers (interactive prompting) ---
    let git_servers =
        if args.replace_additional_clones || !args.additional_clone.is_empty() || !interactive {
            git_servers
        } else {
            prompt_git_servers(git_servers, &selected_grasp_servers, simple_mode)?
        };
    for git_server in &git_servers {
        validate_git_server_url(git_server)?;
    }

    // --- Relays ---
    let relays: Vec<RelayUrl> =
        if args.replace_additional_relays || !args.additional_relay.is_empty() || !interactive {
            relay_strings
                .iter()
                .filter_map(|r| parse_relay_url(r).ok())
                .collect()
        } else if simple_mode {
            let grasp_relay_urls: Vec<String> = selected_grasp_servers
                .iter()
                .filter_map(|r| format_grasp_server_url_as_relay_url(r).ok())
                .collect();
            let options: Vec<String> = relay_strings
                .iter()
                .filter(|s| !grasp_relay_urls.iter().any(|r| s.as_str() == r))
                .cloned()
                .collect();
            let selections: Vec<bool> = vec![true; options.len()];
            let selected = multi_select_with_custom_value(
                "extra nostr relays (grasp servers are sufficient; public relays optional)",
                "nostr relay",
                options,
                selections,
                |s| {
                    parse_relay_url(s)
                        .map(|_| s.to_string())
                        .context(format!("Invalid relay URL format: {s}"))
                },
            )?;
            show_multi_input_prompt_success("additional nostr relays", &selected);
            [
                grasp_relay_urls
                    .iter()
                    .filter_map(|r| parse_relay_url(r).ok())
                    .collect::<Vec<RelayUrl>>(),
                selected
                    .iter()
                    .filter_map(|r| parse_relay_url(r).ok())
                    .collect::<Vec<RelayUrl>>(),
            ]
            .concat()
        } else {
            // advanced interactive
            let selections: Vec<bool> = vec![true; relay_strings.len()];
            let selected = multi_select_with_custom_value(
                "nostr relays",
                "nostr relay",
                relay_strings,
                selections,
                |s| {
                    parse_relay_url(s)
                        .map(|_| s.to_string())
                        .context(format!("Invalid relay URL format: {s}"))
                },
            )?;
            show_multi_input_prompt_success("nostr relays", &selected);
            selected
                .iter()
                .filter_map(|r| parse_relay_url(r).ok())
                .collect()
        };

    // --- Maintainers ---
    let maintainers_default = if let Some(ref mr) = my_ref {
        let mut m = vec![*my_pubkey];
        for pk in &mr.maintainers {
            if !m.contains(pk) {
                m.push(*pk);
            }
        }
        m
    } else if let Some(coord) = state.coordinate() {
        let selected = coord.coordinate.public_key;
        if selected == *my_pubkey {
            vec![*my_pubkey]
        } else {
            vec![*my_pubkey, selected]
        }
    } else {
        vec![*my_pubkey]
    };

    let base_maintainers = if args.replace_maintainers {
        let mut m = if args.replacement_lists_author {
            Vec::new()
        } else {
            vec![user_ref.public_key]
        };
        for npub in &args.other_maintainers {
            if let Ok(pk) = PublicKey::from_bech32(npub) {
                if !m.contains(&pk) {
                    m.push(pk);
                }
            }
        }
        m
    } else {
        maintainers_default
    };

    // `ngit init` always creates a sole-maintainer repository. Existing
    // membership is changed only by the named actions in `ngit repo edit`,
    // which supply an exact internal projection here.
    let maintainers = base_maintainers;

    // --- Lead maintainer (NIP-34 `M` role) ---
    let lead_arg = args
        .lead_maintainer
        .as_deref()
        .map(|input| {
            PublicKey::parse(input).with_context(|| {
                format!("--lead-maintainer '{input}' is not a valid npub or hex public key")
            })
        })
        .transpose()?;
    let (maintainers, lead) = if args.clear_lead {
        (maintainers, None)
    } else if let (None, Some(role_tags)) = (lead_arg, &args.role_tags) {
        // An explicitly prepared role-tag history (acknowledgement or
        // self-defer repair) already records the lead this replacement
        // asserts. Re-deriving the lead from the cached announcement would
        // resurrect the pre-repair record: an `M=continue` repair would emit
        // the signer as `m` and close the repaired `M` with a departure
        // boundary they never signed.
        let implied = role_tags_assert_lead(role_tags).filter(|lead| maintainers.contains(lead));
        (maintainers, implied)
    } else {
        apply_lead_to_maintainers(
            lead_arg,
            my_pubkey,
            maintainers,
            my_ref.as_ref(),
            state.repo_ref(),
        )?
    };

    // --- Interactive: github/codeberg warning ---
    if interactive
        && selected_grasp_servers.is_empty()
        && git_servers
            .iter()
            .any(|s| s.contains("github.com") || s.contains("codeberg.org"))
        && Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("you have listed github / codeberg. Are you or other maintainers planning on pushing directly to github / codeberg rather than using your shiny new nostr clone url which will do this for you?")
                .with_default(false),
        )?
    {
        println!("This means people using the nostr URL won't get your latest branch updates.");
        if Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt("opt-out of storing git state on nostr and relay on github for now? you will still receive PRs and issues via nostr")
                .with_default(true),
        )? {
            git_repo.save_git_config_item("nostr.nostate", "true", false)?;
        }
    }

    // --- Web ---
    let gitworkshop_url = build_gitworkshop_url(&user_ref.public_key, &identifier, relays.first());
    let web_default = resolve_web(&args.web, state, &identifier, &gitworkshop_url);

    let web = if !args.web.is_empty() || !interactive || simple_mode {
        web_default
    } else {
        // advanced interactive
        let web_default_str = web_default.join(" ");
        Interactor::default()
            .input(
                PromptInputParms::default()
                    .with_prompt("repo website")
                    .optional()
                    .with_default(web_default_str)
                    .with_flag_name("--web"),
            )?
            .split(' ')
            .map(std::string::ToString::to_string)
            .collect()
    };

    // --- Informational upstream (`u`) tags ---
    // NIP-34 uses `u` to mark a repository as a subordinate fork. ngit never
    // invents one; it only preserves existing metadata or emits fields
    // explicitly supplied with `--u` / `--upstream`.
    let upstream = if args.upstream.is_empty() {
        latest
            .as_ref()
            .map_or_else(Vec::new, |lr| lr.upstream.clone())
    } else {
        vec![args.upstream.clone()]
    };

    // --- Earliest unique commit ---
    // Cascade: my event -> consolidated RepoRef (selected maintainer's) -> local
    // root commit
    let my_euc = my_ref
        .as_ref()
        .map(|mr| &mr.root_commit)
        .filter(|c| !c.is_empty());
    let repo_euc = state
        .repo_ref()
        .map(|rr| &rr.root_commit)
        .filter(|c| !c.is_empty());
    let euc_default = my_euc
        .or(repo_euc)
        .cloned()
        .unwrap_or_else(|| root_commit.to_string());

    let earliest_unique_commit = if let Some(commit) = &args.earliest_unique_commit {
        if let Ok(exists) = git_repo.does_commit_exist(commit) {
            if !exists {
                bail!("earliest unique commit does not exist on current repository");
            }
        } else {
            bail!("earliest unique commit id not formatted correctly");
        }
        if commit.len() != 40 {
            bail!("earliest unique commit id must be 40 characters long");
        }
        commit.clone()
    } else if interactive && !simple_mode {
        println!(
            "the earliest unique commit helps with discoverability. It defaults to the root commit. Only change this if your repo has completely forked off an has formed its own identity."
        );
        let mut result = euc_default.clone();
        loop {
            result = Interactor::default().input(
                PromptInputParms::default()
                    .with_prompt("earliest unique commit (to help with discoverability)")
                    .with_default(result.clone())
                    .with_flag_name("--earliest-unique-commit"),
            )?;
            if let Ok(exists) = git_repo.does_commit_exist(&result) {
                if exists && result.len() == 40 {
                    break;
                }
                if !exists {
                    println!("commit does not exist on current repository");
                }
            } else {
                println!("commit id not formatted correctly");
            }
            if result.len() != 40 {
                println!("commit id must be 40 characters long");
            }
        }
        result
    } else {
        euc_default
    };

    // --- Blossoms (preserve from latest event) ---
    let blossoms = latest
        .as_ref()
        .map_or_else(Vec::new, |lr| lr.blossoms.clone());

    // --- Hashtags (shared metadata — from latest event, like name/description/web)
    // ---
    let hashtags_default = resolve_hashtags(&args.hashtag, args.replace_hashtags, state)?;

    let hashtags =
        if args.replace_hashtags || !args.hashtag.is_empty() || !interactive || simple_mode {
            hashtags_default
        } else {
            // advanced interactive
            let selections: Vec<bool> = vec![true; hashtags_default.len()];
            let selected = multi_select_with_custom_value(
                "hashtags for repository discovery",
                "hashtag",
                hashtags_default,
                selections,
                validate_hashtag,
            )?;
            show_multi_input_prompt_success("hashtags", &selected);
            selected
        };

    // --- Extra (unknown) tags ---
    // Cascade: --clean wipes them, otherwise inherit from the latest
    // event across all maintainers (so a co-maintainer's newer publish
    // propagates new tags, matching how name/description/web cascade).
    // Falls back to my own event for symmetry; in MyAnnouncement state
    // my event *is* the latest so both branches return the same set.
    let extra_tags: Vec<nostr::prelude::Tag> = if args.clean {
        vec![]
    } else if let Some(ref lr) = latest {
        lr.extra_tags.clone()
    } else if let Some(ref mr) = my_ref {
        mr.extra_tags.clone()
    } else {
        vec![]
    };

    // --- Role tags (my own announcement only, like `maintainers`) ---
    // Prior role tags supply the history boundaries and moderator entries
    // for the generated role tags; `--clean` leaves them alone (see
    // [`ResolvedFields::role_tags`]). When my announcement predates role
    // tags, untimed entries are materialized from its maintainer listing so
    // a member this republish drops is closed with an end boundary rather
    // than silently unlisted. Materialize them under the lead this
    // replacement will assert so first establishing a lead does not invent a
    // prior co-maintainer interval for that pubkey.
    let role_tags = args.role_tags.clone().unwrap_or_else(|| {
        my_ref.as_ref().map_or_else(Vec::new, |repo_ref| {
            repo_ref.role_history_for_republish_with_lead(lead)
        })
    });

    let private = if args.private {
        true
    } else if args.public {
        false
    } else {
        state.repo_ref().is_some_and(|repo_ref| repo_ref.private)
    };

    Ok(ResolvedFields {
        identifier,
        name,
        description,
        git_servers,
        relays,
        web,
        upstream,
        maintainers,
        earliest_unique_commit,
        blossoms,
        hashtags,
        private,
        selected_grasp_servers,
        announcement_events: state
            .repo_ref()
            .map(|repo_ref| repo_ref.events.clone())
            .unwrap_or_default(),
        extra_tags,
        role_tags,
        lead,
        preserve_selected_coordinate: args.preserve_selected_coordinate,
    })
}

/// Interactive prompt for git server selection with simple/advanced modes.
fn prompt_git_servers(
    git_servers: Vec<String>,
    selected_grasp_servers: &[String],
    simple_mode: bool,
) -> Result<Vec<String>> {
    let grasp_server_git_servers: Vec<String> = git_servers
        .iter()
        .filter(|s| is_grasp_server_clone_url(s))
        .cloned()
        .collect();
    let mut additional_server_options: Vec<String> = git_servers
        .iter()
        .filter(|s| !is_grasp_server_clone_url(s))
        .cloned()
        .collect();

    if simple_mode && !selected_grasp_servers.is_empty() {
        if additional_server_options.is_empty() {
            return Ok(git_servers);
        }
        let selected = loop {
            let selections: Vec<bool> = vec![true; additional_server_options.len()];
            let selected = multi_select_with_custom_value(
                "additional git server(s) on top of grasp servers",
                "git server remote url",
                additional_server_options,
                selections,
                validate_git_server_url,
            )?;

            if selected.is_empty()
                || Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_prompt("if you or another maintainer start pushing directly to these, nostr will be out of date")
                        .dont_report()
                        .with_choices(vec![
                            "I'll always push to the nostr remote".to_string(),
                            "change setup".to_string(),
                        ])
                        .with_default(0),
                )? == 1
            {
                additional_server_options = selected;
                continue;
            }
            break selected;
        };
        show_multi_input_prompt_success("additional git servers", &selected);
        let mut combined = grasp_server_git_servers;
        combined.extend(selected);
        Ok(combined)
    } else {
        let selections: Vec<bool> = vec![true; git_servers.len()];
        let selected = multi_select_with_custom_value(
            "git server remote url(s)",
            "git server remote url",
            git_servers,
            selections,
            validate_git_server_url,
        )?;
        show_multi_input_prompt_success("git servers", &selected);
        Ok(selected)
    }
}

pub(super) fn validate_git_server_url(url: &str) -> Result<String> {
    validate_git_server_clone_url(url)?;
    if is_git_remote_helper_url(url) {
        Ok(url.to_string())
    } else {
        CloneUrl::from_str(url)
            .map(|_| url.to_string())
            .context(format!("Invalid git server URL format: {url}"))
    }
}

/// How `ngit init` establishes the repository state after publishing
/// the announcement.
enum StateAction {
    /// `nostr.nostate`: no state event is created or synced.
    None,
    /// A canonical state event is already cached: re-sign its ref
    /// values as a fresh candidate ordered after it and establish the
    /// candidate through a state transaction, so every announced relay
    /// — including ones this init just added — receives the state and
    /// the git servers are realigned to it (see
    /// [`republish_cached_state`]).
    RepublishCachedState,
    /// No canonical state, but the pre-existing `origin` remote was
    /// listable: build a candidate state event from its listing and
    /// establish it through a candidate state transaction. The listing
    /// travels with the origin's URL — the one server guaranteed to
    /// serve the objects it advertised.
    PublishOriginState {
        origin_url: String,
        origin_state: HashMap<String, String>,
    },
    /// Fresh repository: push the local main/master branch and its
    /// state through the state transaction.
    PushInitialBranch,
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn publish_and_finalize(
    fields: ResolvedFields,
    signer: Arc<ngit::signer::NgitSigner>,
    user_ref: &ngit::login::user::UserRef,
    client: &mut Client,
    cli: &Cli,
    mode: LaunchMode,
    git_repo: &Repo,
    repo_config_result: &Result<ngit::repo_ref::RepoConfigYaml>,
    selected_repo: Option<&ResolvedRepoCoordinate>,
    private_discovery: &PrivateGitRelayDiscovery,
    pre_edit_repo_ref: Option<&RepoRef>,
    repo_edit_preflight: Option<&RepoEditPreflight>,
) -> Result<()> {
    // Every route into an announcement — either command, any flag shape, any
    // repository config — resolves its hosting into these two fields, so this
    // is the one place the invariant has to hold. Checked before any signing,
    // private-auth setup or maintainer-state handoff so a refusal leaves the
    // published graph untouched.
    validate_announcement_hosting(&fields.git_servers, &fields.relays, mode)?;

    let git_repo_path = git_repo.get_path()?;
    let preserve_selected_coordinate = fields.preserve_selected_coordinate;
    let infrastructure_changed = pre_edit_repo_ref.is_some_and(|current| {
        current.git_server != fields.git_servers || current.relays != fields.relays
    });

    // Step 1: Build RepoRef
    //
    // `fields.extra_tags` carries any tags on the source announcement
    // that this ngit doesn't itself emit (see `is_known_tag_name`).
    // They are round-tripped verbatim so a tag added by a future ngit
    // version or third-party tool isn't silently lost; `--clean`
    // suppresses the carry-over upstream in `resolve_fields`.
    if !fields.extra_tags.is_empty() {
        let names: Vec<String> = fields
            .extra_tags
            .iter()
            .filter_map(|t| t.as_slice().first().cloned())
            .collect();
        let warn_style = Style::new().yellow();
        eprintln!(
            "{}",
            warn_style.apply_to(format!(
                "warning: preserving unknown tag(s) from existing announcement: {}",
                names.join(", "),
            )),
        );
        eprintln!(
            "{}",
            warn_style.apply_to("         pass --clean to drop them on republish"),
        );
    }
    let repo_ref = RepoRef {
        identifier: fields.identifier.clone(),
        name: fields.name,
        description: fields.description,
        root_commit: fields.earliest_unique_commit,
        git_server: fields.git_servers,
        web: fields.web,
        upstream: fields.upstream,
        relays: fields.relays.clone(),
        blossoms: fields.blossoms,
        hashtags: fields.hashtags,
        private: fields.private,
        selected_maintainer: user_ref.public_key,
        maintainers_without_annoucnement: None,
        maintainers: fields.maintainers.clone(),
        events: fields.announcement_events,
        nostr_git_url: None,
        extra_tags: fields.extra_tags,
        role_tags: fields.role_tags,
        moderators: vec![],
        lead: fields.lead,
    };
    clear_private_git_auth();
    if let Some(current) = pre_edit_repo_ref.filter(|current| current.private) {
        client.nip42_register_private_repo_relays(current.relays.clone());
        prepare_private_git_auth(&current.git_server, &signer).await?;
    }
    if repo_ref.private {
        client.nip42_register_private_repo_relays(repo_ref.relays.clone());
        prepare_private_git_auth(&repo_ref.git_server, &signer).await?;
    }

    // Whether the network fetch in `launch` already covered the
    // coordinate being announced. The fetch runs only when a repo
    // coordinate was resolved, and an `--identifier` change announces a
    // coordinate that fetch never saw. (A co-maintainer's resolved
    // coordinate names another maintainer, but the fetch discovers and
    // covers every maintainer coordinate for the identifier, so
    // matching on the identifier is sufficient.)
    let announced_coordinate_fetched =
        selected_repo.is_some_and(|resolved| resolved.coordinate.identifier == repo_ref.identifier);

    let selected_repo = selected_repo
        .cloned()
        .unwrap_or_else(|| ResolvedRepoCoordinate {
            coordinate: Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: user_ref.public_key,
                    identifier: repo_ref.identifier.clone(),
                },
                relays: repo_ref.relays.clone(),
            },
            source: RepoCoordinateSource::NewRepository,
            remote: None,
        });
    print_selected_repo(&selected_repo);

    client.set_signer(signer.clone()).await;

    // A removal cannot make the state event it currently resolves from
    // ineligible. Hand the exact state to the removing maintainer before
    // signing or publishing the membership replacement, then refresh and
    // verify both frontiers. An equivalent handoff may remain published if a
    // later check fails, but the membership event is still untouched.
    let state_handed_off =
        if let (Some(current), Some(preflight)) = (pre_edit_repo_ref, repo_edit_preflight) {
            handoff_removed_maintainer_state(
                git_repo,
                current,
                preflight,
                user_ref,
                client,
                &signer,
                &selected_repo,
                private_discovery,
            )
            .await?
        } else {
            false
        };

    // Step 2: Create event
    let repo_event = repo_ref.to_event(&signer).await?;

    // Step 3: Build nostr URL
    let nostr_url_decoded = repo_ref.to_nostr_git_url(&Some(git_repo));

    let events = vec![repo_event];

    // Step 4: Handle state events and push/sync logic
    let no_state = if let Ok(Some(s)) = git_repo.get_git_config_item("nostr.nostate", None) {
        s == "true"
    } else {
        false
    };

    // Stale-state protection: a candidate state event must only be
    // built after this invocation has fetched state events for the
    // coordinate being announced, otherwise a fresh init could re-sign
    // ref values older than a state event it never saw. `launch` only
    // fetches when a repo coordinate was already resolvable, so cover
    // the announced coordinate here when it didn't.
    if !no_state && !announced_coordinate_fetched {
        if let Err(error) = fetching_with_report(
            git_repo_path,
            client,
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: user_ref.public_key,
                    identifier: repo_ref.identifier.clone(),
                },
                relays: repo_ref.relays.clone(),
            },
        )
        .await
        {
            eprintln!(
                "WARNING: failed to fetch any existing repository state from relays: {error:#}"
            );
        }
    }

    let state_action = if no_state || (state_handed_off && !infrastructure_changed) {
        // The user opted out, or the pre-announcement handoff already
        // established the required state on the unchanged infrastructure.
        StateAction::None
    } else if get_state_from_cache(Some(git_repo.get_path()?), &repo_ref)
        .await
        .is_ok()
    {
        // A canonical state event is already cached: republish it as a
        // fresh candidate through the state transaction so relays this
        // init just announced receive the state immediately. Safe
        // against stale-state clobbering because this invocation
        // fetched state events for the coordinate above and the
        // candidate is ordered after the newest cached event.
        StateAction::RepublishCachedState
    } else if let Ok(remote) = git_repo.git_repo.find_remote("origin") {
        if let Ok(url) = remote.url() {
            // Build a state event from the pre-existing origin's
            // listing. It is a transaction candidate: pushed to the
            // repo's git servers, fanned out to the relays and cached
            // only after acceptance (see publish_origin_state).
            if let Ok(mut origin_state) = list_from_remote(
                &crate::output::term(),
                git_repo,
                url,
                &nostr_url_decoded,
                false,
            ) {
                origin_state.retain(|key, _| {
                    key.starts_with("refs/heads/")
                        || key.starts_with("refs/tags/")
                        || key.starts_with("HEAD")
                });
                StateAction::PublishOriginState {
                    origin_url: url.to_string(),
                    origin_state,
                }
            } else {
                // cant reach existing origin so just try push
                StateAction::PushInitialBranch
            }
        } else {
            // origin never connected so just try push
            StateAction::PushInitialBranch
        }
    } else {
        // no origin so we need to just push
        StateAction::PushInitialBranch
    };

    // Step 5: Publish events
    if repo_ref.private {
        publish_private_git_relay_list(client, &repo_ref.relays, user_ref, &signer).await?;
    }

    let _ = send_events(
        client,
        Some(git_repo_path),
        events,
        user_ref.relays.write(),
        fields.relays.clone(),
        !cli.disable_cli_spinners,
        false,
    )
    .await?;

    let nostr_url = nostr_url_decoded.to_string();
    if !preserve_selected_coordinate {
        // Step 6: Set git config
        git_repo.save_git_config_item(
            "nostr.repo",
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: user_ref.public_key,
                    identifier: fields.identifier.clone(),
                },
                relays: vec![],
            }
            .to_bech32()?,
            false,
        )?;

        // Step 7: Set origin remote
        if let Ok(remote) = git_repo.git_repo.find_remote("origin") {
            let previous_url = remote.url().ok().map(std::string::ToString::to_string);
            drop(remote);
            if let Some(previous_url) = previous_url {
                preserve_replaced_origin_remote(git_repo, &previous_url);
            }
            git_repo.git_repo.remote_set_url("origin", &nostr_url)?;
        } else {
            git_repo.git_repo.remote("origin", &nostr_url)?;
        }
        println!("set remote origin to nostr url");
    }

    // Step 8: Push/sync
    match state_action {
        StateAction::None => {}
        StateAction::PushInitialBranch => {
            let branch_name = main_or_master_branch_name(git_repo)?;
            if !fields.selected_grasp_servers.is_empty() {
                wait_for_grasp_servers(
                    git_repo,
                    &fields.selected_grasp_servers,
                    &user_ref.public_key,
                    &fields.identifier,
                    repo_ref.private.then(|| signer.clone()),
                )
                .await?;
            }

            println!("pushing your repository data to your git server(s)...");
            push_initial_branch(
                git_repo,
                &repo_ref,
                user_ref,
                client,
                &signer,
                &nostr_url_decoded,
                branch_name,
            )
            .await
            .with_context(|| {
                format!(
                    "your repository announcement was published to nostr but pushing your git data failed. fix the reported issue and run `git push -u origin {branch_name}` to push your git data and publish the repository state"
                )
            })?;
        }
        StateAction::RepublishCachedState => {
            if !fields.selected_grasp_servers.is_empty() {
                wait_for_grasp_servers(
                    git_repo,
                    &fields.selected_grasp_servers,
                    &user_ref.public_key,
                    &fields.identifier,
                    repo_ref.private.then(|| signer.clone()),
                )
                .await?;
            }

            println!(
                "republishing your repository state to nostr and syncing your git server(s) with it..."
            );
            republish_cached_state(
                git_repo,
                &repo_ref,
                user_ref,
                client,
                &signer,
                &nostr_url_decoded,
                false,
            )
            .await
            .context(
                "your repository announcement was published to nostr but republishing its repository state failed. fix the reported issue and run `ngit sync`",
            )?;
        }
        StateAction::PublishOriginState {
            origin_url,
            origin_state,
        } => {
            if !fields.selected_grasp_servers.is_empty() {
                wait_for_grasp_servers(
                    git_repo,
                    &fields.selected_grasp_servers,
                    &user_ref.public_key,
                    &fields.identifier,
                    repo_ref.private.then(|| signer.clone()),
                )
                .await?;
            }

            println!(
                "publishing your repository state and syncing your git server(s) with the existing origin's state..."
            );
            publish_origin_state(
                git_repo,
                &repo_ref,
                user_ref,
                client,
                &signer,
                &nostr_url_decoded,
                &origin_url,
                origin_state,
            )
            .await
            .context(
                "your repository announcement was published to nostr but publishing its repository state failed. fix the reported issue and run `ngit sync`",
            )?;
        }
    }

    // Step 9: Print share URLs / completion message
    let gitworkshop_url = nostr_url_decoded
        .to_string()
        .replace("nostr://", "https://gitworkshop.dev/");
    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "published",
            "entity": "repository",
            "nostr_url": nostr_url,
            "url": gitworkshop_url,
        }));
    }
    println!("share your repository: {gitworkshop_url}");
    println!("clone url: {nostr_url}");

    // Step 10: Update maintainers.yaml if needed
    let relays = fields
        .relays
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<String>>();
    if match repo_config_result {
        Ok(config) => {
            !<std::option::Option<std::string::String> as Clone>::clone(&config.identifier)
                .unwrap_or_default()
                .eq(&fields.identifier)
                || !extract_pks(config.maintainers.clone())?.eq(&fields.maintainers)
                || !config.relays.eq(&relays)
        }
        Err(_) => false,
    } {
        let title_style = Style::new().bold().fg(console::Color::Yellow);
        println!("{}", title_style.apply_to("maintainers.yaml"));
        save_repo_config_to_yaml(
            git_repo,
            fields.identifier.clone(),
            fields.maintainers.clone(),
            relays.clone(),
        )?;
        println!(
            "maintainers.yaml {}. commit and push.",
            if repo_config_result.is_err() {
                "created"
            } else {
                "updated"
            }
        );
        println!(
            "this optional file helps in identifying who the maintainers are over time through the commit history"
        );
    }
    Ok(())
}

pub async fn launch(cli_args: &Cli, args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    launch_with_mode(cli_args, args, signer, LaunchMode::Init, None).await
}

pub(crate) async fn launch_repo_edit(
    cli_args: &Cli,
    args: &SubCommandArgs,
    signer: SignerParams<'_>,
    preflight: Option<RepoEditPreflight>,
) -> Result<()> {
    launch_with_mode(cli_args, args, signer, LaunchMode::RepoEdit, preflight).await
}

#[allow(clippy::too_many_lines)]
async fn launch_with_mode(
    cli_args: &Cli,
    args: &SubCommandArgs,
    signer: SignerParams<'_>,
    mode: LaunchMode,
    repo_edit_preflight: Option<RepoEditPreflight>,
) -> Result<()> {
    // Phase 1: Local-only setup
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer.info,
        signer.password,
        Some(&client),
        false,
    )
    .await?;

    client.set_signer(signer.clone()).await;
    let resolved_repo_coordinate = try_resolve_repo_coordinate(&git_repo).await?;
    let private_discovery = if let Some(resolved) = &resolved_repo_coordinate {
        prepare_account_for_repo_fetch(
            &git_repo,
            &mut client,
            &resolved.coordinate,
            &signer,
            &user_ref,
        )
        .await
    } else {
        PrivateGitRelayDiscovery::Absent
    };
    let mut repo_coordinate = resolved_repo_coordinate
        .as_ref()
        .map(|resolved| resolved.coordinate.clone());

    // Phase 2: Try to get cached repo_ref for early validation
    let cached_repo_ref = if let Some(coord) = &repo_coordinate {
        (get_repo_ref_from_cache(Some(git_repo_path), coord).await).ok()
    } else {
        None
    };

    // Phase 3: Pre-fetch validation (fail fast)
    let user_has_grasp_list = !user_ref.grasp_list.urls.is_empty();
    validate_pre_fetch(
        cli_args,
        args,
        mode,
        repo_coordinate.as_ref(),
        user_has_grasp_list,
        cached_repo_ref.as_ref(),
        &user_ref.public_key,
    )?;

    // Phase 4: Network fetch (only if coordinate exists)
    let repo_ref = if let Some(repo_coordinate) = &mut repo_coordinate {
        fetching_with_private_discovery(
            git_repo_path,
            &client,
            repo_coordinate,
            &private_discovery,
        )
        .await?;
        (get_repo_ref_from_cache(Some(git_repo_path), repo_coordinate).await).ok()
    } else {
        None
    };

    if let Some(preflight) = &repo_edit_preflight {
        let current = repo_ref
            .as_ref()
            .context("the repository announcement disappeared after the removal preview")?;
        preflight.require_unchanged(current)?;
    }

    // Phase 4: Determine state + post-fetch validation
    let state = match (&repo_coordinate, &repo_ref) {
        (None, _) => InitState::Fresh,
        (Some(coord), None) => InitState::CoordinateOnly {
            coordinate: coord.clone(),
        },
        (Some(coord), Some(rr)) => {
            if coord.coordinate.public_key == user_ref.public_key {
                InitState::MyAnnouncement {
                    coordinate: coord.clone(),
                    repo_ref: rr.clone(),
                }
            } else if rr.maintainers.contains(&user_ref.public_key) {
                InitState::CoMaintainer {
                    coordinate: coord.clone(),
                    repo_ref: rr.clone(),
                }
            } else {
                InitState::NotListed {
                    coordinate: coord.clone(),
                    repo_ref: rr.clone(),
                }
            }
        }
    };

    validate_post_fetch(cli_args, args, mode, &state, user_ref.public_key)?;

    // This state is reachable only through the internal repository-edit
    // publication path. Public init rejects every existing announcement, and
    // acceptance is handled exclusively by `ngit repo accept`.
    if let InitState::CoMaintainer { repo_ref: rr, .. } = &state {
        println!(
            "updating your co-maintainer announcement for '{}' on nostr...",
            rr.name
        );
    }

    // Phase 5: Resolve all fields
    let repo_config_result = get_repo_config_from_yaml(&git_repo);
    let fields = resolve_fields(
        &state,
        &user_ref,
        args,
        cli_args,
        &git_repo,
        &root_commit.to_string(),
        &client,
        &repo_config_result,
        cli_args.interactive,
    )?;

    // Phase 6: Persist --repo-relay-only flag to local git config if supplied
    if cli_args.repo_relay_only {
        git_repo.save_git_config_item("nostr.repo-relay-only", "true", false)?;
    }

    // Phase 7: Build and publish
    let suggest_skill = may_suggest_skill(&state);
    let result = publish_and_finalize(
        fields,
        signer,
        &user_ref,
        &mut client,
        cli_args,
        mode,
        &git_repo,
        &repo_config_result,
        resolved_repo_coordinate.as_ref(),
        &private_discovery,
        state.repo_ref(),
        repo_edit_preflight.as_ref(),
    )
    .await;
    if result.is_ok()
        && suggest_skill
        && !is_quiet()
        && should_suggest_skill(&git_repo, git_repo_path)
    {
        print_skill_suggestion();
    }
    result
}

fn should_suggest_skill(git_repo: &Repo, git_repo_path: &Path) -> bool {
    agent_guidance::reminders_enabled(git_repo).unwrap_or(false)
        && agent_guidance::status(git_repo_path).is_ok_and(|status| !status.installed)
}

fn print_skill_suggestion() {
    eprintln!(
        "{}",
        Style::new()
            .fg(console::Color::Color256(214))
            .apply_to(
                "tip: help coding agents collaborate through ngit by running `ngit skill install` (or `ngit skill opt-out --local` to stop reminders here)",
            )
            .for_stderr()
    );
}

fn parse_relay_url(s: &str) -> Result<RelayUrl> {
    // Attempt to parse the original string
    match RelayUrl::parse(s) {
        Ok(url) => Ok(url),
        Err(original_err) => {
            // If parsing fails, prefix with "wss://" and try again
            let prefixed = format!("wss://{s}");
            RelayUrl::parse(&prefixed).map_err(|_| original_err)
        }
    }
    .context(format!("failed to parse relay url: {s}"))
}

fn main_or_master_branch_name(git_repo: &Repo) -> Result<&'static str> {
    let local_branches = git_repo
        .get_local_branch_names()
        .context("failed to find any local branches")?;
    if local_branches.contains(&"main".to_string()) {
        Ok("main")
    } else if local_branches.contains(&"master".to_string()) {
        Ok("master")
    } else {
        bail!(
            "set remote origin to nostr url and tried to push main or master branch but they dont exist yet"
        )
    }
}

/// Push the local `main`/`master` branch and its repository state to the
/// repo's git servers through the state transaction — the in-process
/// equivalent of the `git push -u origin <branch>` subprocess init used
/// to spawn. The candidate state event is built exactly as the remote
/// helper would have built it (the first reachable git server's listing
/// plus the pushed branch) and becomes locally authoritative only after
/// a git server and at least one relay accepted it.
async fn push_initial_branch(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    signer: &Arc<ngit::signer::NgitSigner>,
    nostr_url_decoded: &NostrUrlDecoded,
    branch_name: &str,
) -> Result<()> {
    let term = Term::stderr();
    let refspec = format!("refs/heads/{branch_name}:refs/heads/{branch_name}");
    let refspecs = pin_push_sources(&git_repo.git_repo, &[refspec])?;
    let refspec = &refspecs[0];
    let private_signer = repo_ref.private.then_some(signer);

    // Git-server reality must come from a same-invocation listing; local
    // remote-tracking refs are only written after a successful push.
    let list_outputs = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        nostr_url_decoded,
        None,
        private_signer,
    )
    .await;

    // Mirror the remote helper: with no state event on nostr yet, the
    // baseline state is the first reachable git server's listing.
    let existing_state = repo_ref
        .git_server
        .iter()
        .find_map(|url| list_outputs.get(url).map(|(state, _)| state.clone()))
        .with_context(|| {
            format!(
                "failed to connect to git servers: {}",
                repo_ref.git_server.join(" ")
            )
        })?;

    let (rejected_refspecs, remote_refspecs) = create_rejected_refspecs_and_remotes_refspecs(
        &term,
        git_repo,
        &refspecs,
        &existing_state,
        &list_outputs,
    )?;
    if rejected_refspecs.contains_key(refspec) {
        bail!("refs/heads/{branch_name} is out of sync with an existing git server");
    }

    let new_state = generate_updated_state(git_repo, &existing_state, &refspecs)?;

    // The newest cached state event across maintainer coordinates is
    // the NIP-01 ordering reference for the candidate. A fresh init
    // just fetched and normally finds none, but keeping the ordering
    // discipline means this invocation can never re-sign ref values as
    // an event that loses to a cached predecessor.
    let old_state_event = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![get_filter_state_events(&repo_ref.coordinates(), true)],
    )
    .await
    .ok()
    .and_then(|events| event_ordering::latest_event(&events).cloned());

    let state = RepoState::build(
        repo_ref.identifier.clone(),
        new_state,
        signer,
        old_state_event.as_ref(),
    )
    .await?;

    let repo_relay_only = repository_relay_only(git_repo, repo_ref);
    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        user_ref.relays.write()
    };

    let mut ops = LiveOps {
        client,
        git_repo,
        term: &term,
        git_server_push_options: &[],
        decoded_nostr_url: nostr_url_decoded,
    };
    let mut transaction = StateTransaction::new(repo_ref, Some(state));
    if let Err(failure) = transaction
        .execute(
            &mut ops,
            remote_refspecs,
            &refspecs,
            &my_write_relays,
            repo_relay_only,
        )
        .await?
    {
        bail!("{}", failure.user_message());
    }
    // The transaction committed: the pushed state is now the
    // authoritative cached state.
    record_accepted_push_refspecs(git_repo, "origin", &refspecs)
        .context("failed to update the origin remote-tracking ref after push")?;
    set_branch_upstream(git_repo, "origin", branch_name)?;
    println!("pushed {branch_name} branch and published repository state");
    Ok(())
}

/// Re-sign the cached repository state's ref values as a fresh
/// candidate event and establish it through a candidate state
/// transaction: stage it on the grasp relays, realign the repository's
/// git servers to it, fan it out to every remaining announced relay and
/// cache it only after a git server and at least one relay accepted it.
///
/// The republish exists because `ngit repo edit` can add relays and git
/// servers to an announcement: a repository whose refs are
/// unchanged would otherwise leave a newly announced relay without the
/// state event (and a newly announced git server without the git data)
/// until the next real `git push`, which a fully synced repository may
/// not make for a long time.
///
/// Safe against stale-state clobbering because the caller only reaches
/// this arm after fetching state events for the announced coordinate in
/// this invocation, and the candidate is ordered after the newest
/// cached kind-30618 across maintainer coordinates.
#[allow(clippy::too_many_arguments)]
async fn handoff_removed_maintainer_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    preflight: &RepoEditPreflight,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    signer: &Arc<ngit::signer::NgitSigner>,
    selected_repo: &ResolvedRepoCoordinate,
    private_discovery: &PrivateGitRelayDiscovery,
) -> Result<bool> {
    let authoritative_authors = repo_ref.confirmed_maintainers();
    let authoritative_events = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![get_filter_state_events(&repo_ref.coordinates(), true)],
    )
    .await
    .context(
        "failed to inspect repository state before removal; the maintainer relationship was not removed",
    )?
    .into_iter()
    .filter(|event| authoritative_authors.contains(&event.pubkey))
    .collect::<Vec<_>>();
    if authoritative_events.is_empty() {
        return Ok(false);
    }
    let current_state = RepoState::try_from(authoritative_events).context(
        "failed to resolve repository state before removal; the maintainer relationship was not removed",
    )?;
    if current_state.event.pubkey != preflight.removed_maintainer {
        return Ok(false);
    }
    let expected_state = current_state.state;
    let nostr_url = repo_ref.to_nostr_git_url(&Some(git_repo));

    println!("handing repository state to a remaining maintainer before removal...");
    let handoff = republish_cached_state(
        git_repo, repo_ref, user_ref, client, signer, &nostr_url, true,
    )
    .await
    .context("repository state handoff failed; the maintainer relationship was not removed")?;

    let mut coordinate = selected_repo.coordinate.clone();
    fetching_with_private_discovery(
        git_repo.get_path()?,
        client,
        &mut coordinate,
        private_discovery,
    )
    .await
    .context(
        "failed to refresh the repository after state handoff; the maintainer relationship was not removed",
    )?;
    let refreshed = get_repo_ref_from_cache(Some(git_repo.get_path()?), &coordinate)
        .await
        .context(
            "failed to resolve the repository after state handoff; the maintainer relationship was not removed",
        )?;
    preflight.require_unchanged(&refreshed)?;

    let verified = get_state_from_cache(Some(git_repo.get_path()?), &refreshed)
        .await
        .context(
            "failed to verify the repository state handoff; the maintainer relationship was not removed",
        )?;
    if verified.event.id != handoff.id
        || verified.event.pubkey != user_ref.public_key
        || verified.state != expected_state
    {
        return Err(cli_error_with_category(
            "membership_state_handoff_changed",
            "the repository state changed while handing it to a remaining maintainer",
            &[],
            &["fetch and rerun the named removal against the current state"],
        ));
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
async fn republish_cached_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    signer: &Arc<ngit::signer::NgitSigner>,
    nostr_url_decoded: &NostrUrlDecoded,
    require_complete_state: bool,
) -> Result<Event> {
    let term = Term::stderr();
    let private_signer = repo_ref.private.then_some(signer);

    let cached_state = get_state_from_cache(Some(git_repo.get_path()?), repo_ref)
        .await
        .context("failed to load the cached repository state event")?;

    // The NIP-01 ordering reference for the fresh candidate: the newest
    // cached state event across maintainer coordinates, freshly fetched
    // by this invocation. Re-signing the cached ref values as an event
    // that loses to its predecessor would leave the relays' view
    // unchanged; an event ordered after it replaces it everywhere.
    let old_state_event = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![get_filter_state_events(&repo_ref.coordinates(), true)],
    )
    .await
    .ok()
    .and_then(|events| event_ordering::latest_event(&events).cloned());

    let candidate = RepoState::build(
        repo_ref.identifier.clone(),
        cached_state.state,
        signer,
        old_state_event.as_ref(),
    )
    .await?;
    let candidate_event = candidate.event.clone();

    // Git-server reality must come from a same-invocation listing;
    // refs/remotes/* may be stale or absent. Requiring at least one
    // listable server keeps the acceptance gate meaningful: with no
    // reachable server every per-server plan would be vacuously empty
    // and the fresh event would broadcast without any git server
    // holding its data.
    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        nostr_url_decoded,
        None,
        private_signer,
    )
    .await;
    if remote_states.is_empty() {
        bail!(
            "failed to connect to git servers: {}",
            repo_ref.git_server.join(" ")
        );
    }

    // Fetch state objects missing locally from whichever listed server
    // has them, so they can be pushed to the servers that don't.
    let missing_refs = super::sync::fetch_missing_refs(
        git_repo,
        &candidate,
        &remote_states,
        nostr_url_decoded,
        private_signer,
    )
    .await?;
    if require_complete_state && !missing_refs.is_empty() {
        bail!(
            "failed to reproduce the complete repository state; missing {}",
            join_with_and(&missing_refs)
        );
    }

    // Plans source branch pushes from the candidate's own oids: the
    // nostr remote's tracking refs may not exist yet on a repository
    // that was announced elsewhere and only just initialised here.
    let (per_server_plans, state_refspecs) = super::sync::build_state_push_plans(
        git_repo,
        &super::sync::BranchPushSource::StateOids,
        &candidate.state,
        &remote_states,
        None,
        &missing_refs,
        &HashSet::new(),
    );

    // Match the plain `ngit sync` this arm used to run: grasp servers
    // are realigned to the state, vanilla servers stay
    // fast-forward-only.
    let force_policy = ServerForcePolicy::ForceOnlyOn(
        remote_states
            .iter()
            .filter_map(|(url, (_, is_grasp_server))| {
                if *is_grasp_server {
                    Some(url.clone())
                } else {
                    None
                }
            })
            .collect(),
    );

    let repo_relay_only = repository_relay_only(git_repo, repo_ref);
    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        user_ref.relays.write()
    };

    let mut ops = LiveOps {
        client,
        git_repo,
        term: &term,
        git_server_push_options: &[],
        decoded_nostr_url: nostr_url_decoded,
    };
    let mut transaction =
        StateTransaction::new(repo_ref, Some(candidate)).with_force_policy(force_policy);
    if let Err(failure) = transaction
        .execute(
            &mut ops,
            per_server_plans,
            &state_refspecs,
            &my_write_relays,
            repo_relay_only,
        )
        .await?
    {
        bail!("{}", failure.user_message());
    }
    // The transaction committed: the fresh event is now the
    // authoritative cached state.
    println!("republished repository state to nostr");

    if !missing_refs.is_empty() {
        println!(
            "skipped the following refs as could not find them locally or on any git servers: {}",
            join_with_and(&missing_refs)
        );
    }
    Ok(candidate_event)
}

/// Establish the state event built from the pre-existing `origin`'s
/// listing through a candidate state transaction: fetch listed objects
/// missing locally from the origin itself, prune refs whose objects
/// could not be obtained so the signed state never advertises objects no
/// server holds, stage the event on the grasp relays, push the
/// origin-derived refs to the repository's git servers, fan the event
/// out to the remaining relays and cache it only after a git server and
/// at least one relay accepted it.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn publish_origin_state(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    user_ref: &ngit::login::user::UserRef,
    client: &Client,
    signer: &Arc<ngit::signer::NgitSigner>,
    nostr_url_decoded: &NostrUrlDecoded,
    origin_url: &str,
    mut origin_state: HashMap<String, String>,
) -> Result<()> {
    let term = Term::stderr();
    let private_signer = repo_ref.private.then_some(signer);

    // The origin is the only server guaranteed to hold the objects its
    // listing advertised. Fetch the missing ones by ref name before
    // signing, so refs never fetched locally (e.g. tags after a
    // --no-tags or single-branch clone) can be pushed to the repo's git
    // servers.
    let refs_to_fetch = origin_refs_with_missing_objects(git_repo, &origin_state);
    if !refs_to_fetch.is_empty() {
        println!("fetching git data listed on the existing origin but missing locally...");
        if let Err(error) = fetch_refs_from_git_server(
            git_repo,
            &refs_to_fetch,
            origin_url,
            nostr_url_decoded,
            &term,
            is_grasp_server_clone_url(origin_url),
        ) {
            println!("failed to fetch from the existing origin: {error}");
        }
    }

    let skipped_origin_refs = prune_refs_with_missing_objects(git_repo, &mut origin_state);
    if !skipped_origin_refs.is_empty() {
        println!(
            "skipping refs listed on the existing origin whose git data could not be fetched: {}",
            join_with_and(&skipped_origin_refs)
        );
    }

    // Ordered after the newest cached state event across maintainer
    // coordinates. Normally none exists in this branch — a cached event
    // routes through the in-process sync instead — but the ordering
    // discipline keeps an unexpected predecessor authoritative.
    let old_state_event = get_events_from_local_cache(
        git_repo.get_path()?,
        vec![get_filter_state_events(&repo_ref.coordinates(), true)],
    )
    .await
    .ok()
    .and_then(|events| event_ordering::latest_event(&events).cloned());

    let candidate = RepoState::build(
        repo_ref.identifier.clone(),
        origin_state,
        signer,
        old_state_event.as_ref(),
    )
    .await?;

    // Git-server reality must come from a same-invocation listing;
    // refs/remotes/* may hold stale values from the pre-nostr origin.
    let remote_states = list_from_remotes(
        &term,
        git_repo,
        &repo_ref.git_server,
        nostr_url_decoded,
        None,
        private_signer,
    )
    .await;

    // Fetch state objects missing locally from whichever listed server
    // has them, so they can be pushed to the servers that don't.
    let missing_refs = super::sync::fetch_missing_refs(
        git_repo,
        &candidate,
        &remote_states,
        nostr_url_decoded,
        private_signer,
    )
    .await?;

    // Plans source branch pushes from the candidate's own oids: the
    // nostr remote has no tracking refs yet.
    let (per_server_plans, state_refspecs) = super::sync::build_state_push_plans(
        git_repo,
        &super::sync::BranchPushSource::StateOids,
        &candidate.state,
        &remote_states,
        None,
        &missing_refs,
        &HashSet::new(),
    );

    // Match the `ngit sync` (without --force) that init used to spawn
    // here: grasp servers are realigned to the state, vanilla servers
    // stay fast-forward-only.
    let force_policy = ServerForcePolicy::ForceOnlyOn(
        remote_states
            .iter()
            .filter_map(|(url, (_, is_grasp_server))| {
                if *is_grasp_server {
                    Some(url.clone())
                } else {
                    None
                }
            })
            .collect(),
    );

    let repo_relay_only = repository_relay_only(git_repo, repo_ref);
    let my_write_relays = if repo_relay_only {
        vec![]
    } else {
        user_ref.relays.write()
    };

    let mut ops = LiveOps {
        client,
        git_repo,
        term: &term,
        git_server_push_options: &[],
        decoded_nostr_url: nostr_url_decoded,
    };

    // The committed candidate is what the nostr `origin` remote serves:
    // the transaction only commits once a git server holds every
    // candidate ref (accepted push or already applied) and the state
    // event is the remote's source of truth for fetches. Its branches
    // are therefore the accepted refspecs to record as remote-tracking
    // refs, mirroring push_initial_branch. `refs/heads/pr/*` never
    // enters the per-server plans and stale pre-nostr tracking refs for
    // it must not be reintroduced.
    let tracking_refspecs: Vec<String> = candidate
        .state
        .iter()
        .filter(|(ref_name, _)| {
            ref_name.starts_with("refs/heads/") && !ref_name.starts_with("refs/heads/pr/")
        })
        .map(|(ref_name, oid)| format!("{oid}:{ref_name}"))
        .collect();

    let mut transaction =
        StateTransaction::new(repo_ref, Some(candidate)).with_force_policy(force_policy);
    if let Err(failure) = transaction
        .execute(
            &mut ops,
            per_server_plans,
            &state_refspecs,
            &my_write_relays,
            repo_relay_only,
        )
        .await?
    {
        bail!("{}", failure.user_message());
    }
    // The transaction committed: the origin-derived state is now the
    // authoritative cached state.
    record_accepted_push_refspecs(git_repo, "origin", &tracking_refspecs)
        .context("failed to update the origin remote-tracking refs after push")?;
    println!("published repository state from the existing origin's refs");

    if !missing_refs.is_empty() {
        println!(
            "skipped the following refs as could not find them locally or on any git servers: {}",
            join_with_and(&missing_refs)
        );
    }
    Ok(())
}

pub(super) fn repository_relay_only(git_repo: &Repo, repo_ref: &RepoRef) -> bool {
    repo_ref.private
        || git_repo
            .get_git_config_item("nostr.repo-relay-only", None)
            .ok()
            .flatten()
            .is_some_and(|value| value == "true")
}

/// Best-effort: keep the git server URL that `origin` pointed at before
/// init under a domain-derived remote name (e.g. `github` for
/// github.com), instead of silently discarding it when origin is
/// repointed at the nostr URL. Skipped when another remote already
/// carries the URL; failures only mean the URL isn't preserved.
fn preserve_replaced_origin_remote(git_repo: &Repo, previous_url: &str) {
    if previous_url.starts_with("nostr://") {
        return;
    }
    if let Ok(remotes) = git_repo.git_repo.remotes() {
        for name in remotes.iter().flatten() {
            let Some(name) = name else {
                continue;
            };
            if name == "origin" {
                continue;
            }
            let url = git_repo
                .git_repo
                .find_remote(name)
                .ok()
                .and_then(|r| r.url().ok().map(std::string::ToString::to_string));
            if url.is_some_and(|u| u.trim_end_matches('/') == previous_url.trim_end_matches('/')) {
                return;
            }
        }
    }
    let Some(base_name) = derive_remote_name_from_url(previous_url) else {
        return;
    };
    for attempt in 0..10 {
        let name = if attempt == 0 {
            base_name.clone()
        } else {
            format!("{base_name}-{}", attempt + 1)
        };
        // Taken names (with a different URL, per the scan above) fall
        // through to the next suffix.
        if git_repo.git_repo.find_remote(&name).is_err() {
            if git_repo.git_repo.remote(&name, previous_url).is_ok() {
                println!("kept the previous origin url as remote '{name}'");
            }
            return;
        }
    }
}

/// `github.com` → `github`; deeper hosts drop the public suffix and
/// dash-join the rest (`git.fiatjaf.com` → `git-fiatjaf`); IP hosts
/// keep every octet (`127.0.0.1` → `127-0-0-1`). Dots are avoided:
/// they read as hostnames rather than remote names. `None` when no
/// usable name can be derived.
fn derive_remote_name_from_url(url: &str) -> Option<String> {
    let domain = url.parse::<CloneUrl>().ok()?.domain();
    let sanitized: String = domain
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let labels: Vec<&str> = sanitized.split('.').filter(|l| !l.is_empty()).collect();
    let is_ip_address =
        !labels.is_empty() && labels.iter().all(|l| l.chars().all(|c| c.is_ascii_digit()));
    let name = if is_ip_address || labels.len() <= 1 {
        labels.join("-")
    } else {
        labels[..labels.len() - 1].join("-")
    };
    if name.is_empty() || name == "origin" {
        None
    } else {
        Some(name)
    }
}

/// `(ref name, listed oid)` pairs from an origin listing whose objects
/// are not in the local repository. Peeled `^{}` entries resolve to
/// their base ref: fetching the base ref delivers the peeled object too.
fn origin_refs_with_missing_objects(
    git_repo: &Repo,
    origin_state: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut missing: Vec<(String, String)> = vec![];
    for (key, value) in origin_state {
        if object_exists_locally(git_repo, value) {
            continue;
        }
        let base = key.trim_end_matches("^{}").to_string();
        let oid = origin_state
            .get(&base)
            .cloned()
            .unwrap_or_else(|| value.clone());
        if !missing.iter().any(|(name, _)| name == &base) {
            missing.push((base, oid));
        }
    }
    missing.sort();
    missing
}

/// Remove refs whose objects are still missing locally, together with
/// their peeled `^{}` twins, so the candidate state never advertises
/// objects that cannot be pushed to any git server. Returns the pruned
/// base ref names.
fn prune_refs_with_missing_objects(
    git_repo: &Repo,
    origin_state: &mut HashMap<String, String>,
) -> Vec<String> {
    let mut pruned: HashSet<String> = HashSet::new();
    for (key, value) in origin_state.iter() {
        if !object_exists_locally(git_repo, value) {
            pruned.insert(key.trim_end_matches("^{}").to_string());
        }
    }
    origin_state.retain(|key, _| !pruned.contains(key.trim_end_matches("^{}")));
    let mut pruned: Vec<String> = pruned.into_iter().collect();
    pruned.sort();
    pruned
}

/// Whether the odb holds the object, of any type. Values that aren't
/// oids (nothing fetchable) count as present.
fn object_exists_locally(git_repo: &Repo, oid: &str) -> bool {
    let Ok(parsed) = git2::Oid::from_str(oid) else {
        return true;
    };
    git_repo.git_repo.find_object(parsed, None).is_ok()
}

#[cfg(test)]
mod announcement_hosting_tests {
    use super::*;

    fn relay(url: &str) -> RelayUrl {
        RelayUrl::parse(url).unwrap()
    }

    /// Flags named by the refusal, in the details block and the suggestions
    /// together — a reader needs the flag to appear somewhere, not in a
    /// particular slot.
    fn refusal_text(missing_clone: bool, missing_relay: bool, mode: LaunchMode) -> String {
        let refusal = announcement_hosting_refusal(missing_clone, missing_relay, mode)
            .expect("expected the hosting invariant to refuse");
        let details: Vec<String> = refusal
            .details
            .iter()
            .map(|(flag, description)| format!("{flag} {description}"))
            .collect();
        format!(
            "{}\n{}\n{}",
            refusal.message,
            details.join("\n"),
            refusal.suggestions.join("\n"),
        )
    }

    #[test]
    fn an_announcement_with_a_relay_and_a_clone_url_is_accepted() {
        assert!(
            validate_announcement_hosting(
                &["https://git.example.com/x.git".to_string()],
                &[relay("wss://relay.example.com")],
                LaunchMode::Init,
            )
            .is_ok()
        );
    }

    /// Grasp-free hosting is legitimate: the invariant is about the two
    /// announcement fields being populated, not about how they were filled.
    #[test]
    fn hosting_supplied_only_by_additional_entries_is_accepted() {
        for mode in [LaunchMode::Init, LaunchMode::RepoEdit] {
            assert!(
                validate_announcement_hosting(
                    &["https://github.com/x/y.git".to_string()],
                    &[relay("wss://relay.damus.io")],
                    mode,
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn empty_hosting_fields_are_refused() {
        for (clone_urls, relays) in [
            (vec![], vec![relay("wss://relay.example.com")]),
            (vec!["https://git.example.com/x.git".to_string()], vec![]),
            (vec![], vec![]),
        ] {
            assert!(
                validate_announcement_hosting(&clone_urls, &relays, LaunchMode::Init).is_err(),
                "clone urls {clone_urls:?} and relays {relays:?} should be refused",
            );
        }
    }

    #[test]
    fn a_missing_relay_names_the_flags_that_add_one() {
        let text = refusal_text(false, true, LaunchMode::Init);
        assert!(
            text.contains("needs at least one relay"),
            "unexpected refusal: {text}",
        );
        assert!(
            text.contains("--grasp-server") && text.contains("--additional-relay"),
            "the refusal should name the flags that add a relay: {text}",
        );
        assert!(
            !text.contains("--additional-clone <URL>  where your git data"),
            "the git server is present, so its flag should not be detailed: {text}",
        );
    }

    #[test]
    fn a_missing_git_server_names_the_flags_that_add_one() {
        let text = refusal_text(true, false, LaunchMode::Init);
        assert!(
            text.contains("needs at least one git server"),
            "unexpected refusal: {text}",
        );
        assert!(
            text.contains("--grasp-server") && text.contains("--additional-clone"),
            "the refusal should name the flags that add a git server: {text}",
        );
    }

    #[test]
    fn empty_hosting_names_every_flag_that_supplies_it() {
        let text = refusal_text(true, true, LaunchMode::Init);
        assert!(
            text.contains("needs at least one relay and one git server"),
            "unexpected refusal: {text}",
        );
        assert!(
            text.contains("--grasp-server")
                && text.contains("--additional-relay")
                && text.contains("--additional-clone"),
            "the refusal should name every flag that supplies hosting: {text}",
        );
    }

    /// The suggestions have to be runnable as printed, and the two commands
    /// spell hosting differently.
    #[test]
    fn suggestions_match_the_command_that_is_publishing() {
        let init = refusal_text(true, true, LaunchMode::Init);
        assert!(
            init.contains("ngit init --grasp-server")
                && init.contains("ngit init --additional-relay <URL> --additional-clone <URL>"),
            "init should suggest its own flags: {init}",
        );
        let edit = refusal_text(true, true, LaunchMode::RepoEdit);
        assert!(
            edit.contains("ngit repo edit --add-grasp-server")
                && edit.contains("--add-additional-relay")
                && edit.contains("--add-additional-clone"),
            "repo edit should suggest its targeted add actions: {edit}",
        );
    }

    /// `ngit repo accept` can only change hosting through `--grasp-server`,
    /// so its refusal must not detail or suggest flags the command lacks.
    #[test]
    fn repo_accept_names_only_its_grasp_flag() {
        let accept = refusal_text(true, true, LaunchMode::RepoAccept);
        assert!(
            accept.contains("ngit repo accept --grasp-server"),
            "repo accept should suggest its grasp flag: {accept}",
        );
        assert!(
            !accept.contains("--additional-relay") && !accept.contains("--additional-clone"),
            "repo accept has no additional hosting flags to name: {accept}",
        );
    }
}

#[cfg(test)]
mod grasp_server_defaulting_tests {
    use nostr::prelude::Keys;

    use super::*;

    /// A `RepoRef` standing in for my own existing announcement.
    fn my_announcement(git_server: Vec<String>, relays: Vec<&str>) -> RepoRef {
        let me = Keys::generate().public_key();
        RepoRef {
            name: "test".to_string(),
            description: String::new(),
            identifier: "test-repo".to_string(),
            root_commit: "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2".to_string(),
            git_server,
            web: vec![],
            upstream: vec![],
            relays: relays
                .iter()
                .map(|relay| RelayUrl::parse(relay).unwrap())
                .collect(),
            blossoms: vec![],
            hashtags: vec![],
            private: false,
            selected_maintainer: me,
            maintainers: vec![me],
            maintainers_without_annoucnement: None,
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: vec![],
            role_tags: vec![],
            moderators: vec![],
            lead: None,
        }
    }

    /// The grasp-format clone URL an announcement carries for `npub`.
    fn grasp_clone_url(server: &str, npub: &str, identifier: &str) -> String {
        format!("https://{server}/{npub}/{identifier}.git")
    }

    #[test]
    fn an_absent_flag_is_unspecified() {
        assert_eq!(
            interpret_grasp_server_args(&[]),
            GraspServerArgs::Unspecified,
        );
    }

    #[test]
    fn an_empty_value_is_an_explicit_opt_out() {
        for values in [
            vec![String::new()],
            vec!["   ".to_string()],
            vec![String::new(), " ".to_string()],
        ] {
            assert_eq!(
                interpret_grasp_server_args(&values),
                GraspServerArgs::Explicit(vec![]),
            );
        }
    }

    #[test]
    fn named_servers_survive_a_stray_empty_value() {
        let selection = interpret_grasp_server_args(&[
            "grasp.example.com".to_string(),
            String::new(),
            "other.example.com".to_string(),
        ]);
        assert_eq!(
            selection,
            GraspServerArgs::Explicit(vec![
                "grasp.example.com".to_string(),
                "other.example.com".to_string(),
            ]),
        );
    }

    /// The reported bug: a fresh repository given only an additional clone URL
    /// must still fall back to the preferred/default grasp servers.
    #[test]
    fn additional_urls_alone_do_not_decide_grasp_hosting() {
        for (relays, clones) in [
            (vec![], vec!["https://github.com/x/y.git".to_string()]),
            (
                vec!["wss://relay.example.com".to_string()],
                vec!["https://github.com/x/y.git".to_string()],
            ),
            (vec!["wss://relay.example.com".to_string()], vec![]),
        ] {
            assert_eq!(
                grasp_servers_before_fallback(
                    &GraspServerArgs::Unspecified,
                    None,
                    &relays,
                    &clones,
                    "test-repo",
                ),
                None,
                "relays {relays:?} and clones {clones:?} should leave the \
                 fallback in charge",
            );
        }
    }

    #[test]
    fn an_explicit_opt_out_wins_over_the_fallback() {
        assert_eq!(
            grasp_servers_before_fallback(
                &GraspServerArgs::Explicit(vec![]),
                None,
                &["wss://relay.example.com".to_string()],
                &["https://github.com/x/y.git".to_string()],
                "test-repo",
            ),
            Some(vec![]),
        );
    }

    #[test]
    fn named_servers_are_used_verbatim() {
        assert_eq!(
            grasp_servers_before_fallback(
                &GraspServerArgs::Explicit(vec!["grasp.example.com".to_string()]),
                None,
                &[],
                &[],
                "test-repo",
            ),
            Some(vec!["grasp.example.com".to_string()]),
        );
    }

    #[test]
    fn grasp_urls_supplied_as_additional_clones_are_detected() {
        let npub = Keys::generate().public_key().to_bech32().unwrap();
        assert_eq!(
            grasp_servers_before_fallback(
                &GraspServerArgs::Unspecified,
                None,
                &["wss://grasp.example.com".to_string()],
                &[grasp_clone_url("grasp.example.com", &npub, "test-repo")],
                "test-repo",
            ),
            Some(vec!["grasp.example.com".to_string()]),
        );
    }

    /// A republish must keep the grasp servers my announcement already
    /// declares, even when this invocation also supplies an unrelated
    /// additional clone URL.
    #[test]
    fn my_existing_grasp_servers_survive_an_additional_clone() {
        let npub = Keys::generate().public_key().to_bech32().unwrap();
        let existing = my_announcement(
            vec![grasp_clone_url("grasp.example.com", &npub, "test-repo")],
            vec!["wss://grasp.example.com"],
        );
        assert_eq!(
            grasp_servers_before_fallback(
                &GraspServerArgs::Unspecified,
                Some(&existing),
                &[],
                &["https://github.com/x/y.git".to_string()],
                "test-repo",
            ),
            Some(vec!["grasp.example.com".to_string()]),
        );
    }

    /// An announcement that genuinely has no grasp servers states its own
    /// hosting; a republish must not graft defaults onto it.
    #[test]
    fn an_existing_announcement_without_grasp_servers_is_not_defaulted() {
        let existing = my_announcement(
            vec!["https://github.com/x/y.git".to_string()],
            vec!["wss://relay.example.com"],
        );
        assert_eq!(
            grasp_servers_before_fallback(
                &GraspServerArgs::Unspecified,
                Some(&existing),
                &[],
                &[],
                "test-repo",
            ),
            Some(vec![]),
        );
    }
}

#[cfg(test)]
mod apply_lead_to_maintainers_tests {
    use nostr::prelude::{Keys, Tag, event::FinalizeEvent};

    use super::*;

    fn test_repo_ref(maintainers: Vec<PublicKey>, lead: Option<PublicKey>) -> RepoRef {
        RepoRef {
            name: "test".to_string(),
            description: String::new(),
            identifier: "test-repo".to_string(),
            root_commit: "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2".to_string(),
            git_server: vec![],
            web: vec![],
            upstream: vec![],
            relays: vec![],
            blossoms: vec![],
            hashtags: vec![],
            private: false,
            selected_maintainer: maintainers[0],
            maintainers,
            maintainers_without_annoucnement: None,
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: vec![],
            role_tags: vec![],
            moderators: vec![],
            lead,
        }
    }

    /// A consolidated `RepoRef` anchored on `selected`, carrying one
    /// announcement per `(announcer, listed)` pair — each listing `listed`
    /// via the deprecated `maintainers` tag — with `maintainers` set to the
    /// recursive union, the way `get_repo_ref_from_cache` consolidates.
    fn consolidated_with_announcements(
        selected: PublicKey,
        announcements: &[(&Keys, &[PublicKey])],
    ) -> RepoRef {
        let mut maintainers = vec![selected];
        for (announcer, listed) in announcements {
            for pk in std::iter::once(announcer.public_key()).chain(listed.iter().copied()) {
                if !maintainers.contains(&pk) {
                    maintainers.push(pk);
                }
            }
        }
        let mut repo_ref = test_repo_ref(maintainers, None);
        repo_ref.selected_maintainer = selected;
        for (announcer, listed) in announcements {
            let mut maintainers_tag = vec!["maintainers".to_string()];
            maintainers_tag.extend(listed.iter().map(ToString::to_string));
            let event = nostr::prelude::EventBuilder::new(Kind::GitRepoAnnouncement, "")
                .tags(vec![
                    Tag::identifier("test-repo"),
                    Tag::parse(maintainers_tag).unwrap(),
                ])
                .finalize(*announcer)
                .unwrap();
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: Kind::GitRepoAnnouncement,
                        public_key: event.pubkey,
                        identifier: "test-repo".to_string(),
                    },
                    relays: vec![],
                },
                event,
            );
        }
        repo_ref
    }

    #[test]
    fn without_the_flag_my_own_assertion_is_carried_while_the_lead_stays_listed() {
        let me = Keys::generate().public_key();
        let lead = Keys::generate().public_key();
        let my_ref = test_repo_ref(vec![me, lead], Some(lead));

        let (maintainers, resolved) =
            apply_lead_to_maintainers(None, &me, vec![me, lead], Some(&my_ref), None).unwrap();
        assert_eq!(maintainers, vec![me, lead]);
        assert_eq!(resolved, Some(lead));

        // the lead was removed from the resolved listing: the assertion is
        // not carried forward
        let (maintainers, resolved) =
            apply_lead_to_maintainers(None, &me, vec![me], Some(&my_ref), None).unwrap();
        assert_eq!(maintainers, vec![me]);
        assert_eq!(resolved, None);
    }

    #[test]
    fn specifying_yourself_keeps_the_full_listing() {
        let me = Keys::generate().public_key();
        let other = Keys::generate().public_key();

        let (maintainers, resolved) =
            apply_lead_to_maintainers(Some(me), &me, vec![me, other], None, None).unwrap();
        assert_eq!(maintainers, vec![me, other]);
        assert_eq!(resolved, Some(me));
    }

    #[test]
    fn specifying_another_lead_keeps_only_active_self_and_lead_roles() {
        let me = Keys::generate().public_key();
        let lead = Keys::generate().public_key();
        let default_listed = Keys::generate().public_key();

        // With no existing announcement, the role update cannot remove an
        // established member from the active graph.
        let (maintainers, resolved) =
            apply_lead_to_maintainers(Some(lead), &me, vec![me, default_listed], None, None)
                .unwrap();
        assert_eq!(maintainers, vec![me, lead]);
        assert_eq!(resolved, Some(lead));
    }

    #[test]
    fn uncovered_drop_of_a_currently_listed_maintainer_is_refused() {
        let me = Keys::generate().public_key();
        let lead_keys = Keys::generate();
        let lead = lead_keys.public_key();
        let dropped = Keys::generate().public_key();
        let my_ref = test_repo_ref(vec![me, dropped], None);
        // the lead acknowledges me (their listing counts as cover) but does
        // not keep the dropped pubkey listed
        let consolidated = consolidated_with_announcements(me, &[(&lead_keys, &[me])]);

        // the pubkey losing authorized-maintainer status is identified
        // (and named in the cli_error printed to stderr)
        assert_eq!(
            members_losing_authorized_status_after_lead_change(
                &[me, lead],
                &lead,
                &me,
                Some(&my_ref),
                Some(&consolidated)
            ),
            vec![dropped]
        );
        assert!(
            apply_lead_to_maintainers(
                Some(lead),
                &me,
                vec![me, dropped],
                Some(&my_ref),
                Some(&consolidated),
            )
            .is_err()
        );
    }

    #[test]
    fn member_covered_by_a_reciprocal_lead_remains_authorized() {
        let me = Keys::generate().public_key();
        let lead_keys = Keys::generate();
        let lead = lead_keys.public_key();
        let dropped = Keys::generate().public_key();
        let my_ref = test_repo_ref(vec![me, dropped], None);
        // the lead keeps the dropped pubkey listed and acknowledges me, so
        // their announcement is reciprocal (authoritative) when my lead
        // relationship is published
        let consolidated = consolidated_with_announcements(me, &[(&lead_keys, &[dropped, me])]);

        let (maintainers, resolved) = apply_lead_to_maintainers(
            Some(lead),
            &me,
            vec![me, dropped],
            Some(&my_ref),
            Some(&consolidated),
        )
        .unwrap();
        assert_eq!(maintainers, vec![me, lead]);
        assert_eq!(resolved, Some(lead));
    }

    #[test]
    fn an_unauthoritative_leads_listing_covers_no_drops() {
        let me = Keys::generate().public_key();
        let lead_keys = Keys::generate();
        let lead = lead_keys.public_key();
        let dropped = Keys::generate().public_key();
        let my_ref = test_repo_ref(vec![me, dropped], None);
        // the lead keeps the dropped pubkey listed but is neither confirmed
        // nor acknowledging me: per NIP-34 their announcement is not
        // authoritative, so the dropped pubkey still loses authorized status
        let consolidated = consolidated_with_announcements(me, &[(&lead_keys, &[dropped])]);

        assert_eq!(
            members_losing_authorized_status_after_lead_change(
                &[me, lead],
                &lead,
                &me,
                Some(&my_ref),
                Some(&consolidated)
            ),
            vec![dropped]
        );
        assert!(
            apply_lead_to_maintainers(
                Some(lead),
                &me,
                vec![me, dropped],
                Some(&my_ref),
                Some(&consolidated),
            )
            .is_err()
        );
    }

    #[test]
    fn a_confirmed_leads_listing_covers_drops_without_acknowledging_me() {
        let me_keys = Keys::generate();
        let me = me_keys.public_key();
        let third_keys = Keys::generate();
        let third = third_keys.public_key();
        let lead_keys = Keys::generate();
        let lead = lead_keys.public_key();
        let dropped = Keys::generate().public_key();
        let my_ref = test_repo_ref(vec![me, third, lead, dropped], None);
        // the lead is confirmed through `third` (listed by me, acknowledging
        // a confirmed member) without listing me directly; their listing is
        // authoritative and keeps the dropped pubkey covered
        let consolidated = consolidated_with_announcements(
            me,
            &[
                (&me_keys, &[third, lead, dropped]),
                (&third_keys, &[me]),
                (&lead_keys, &[third, dropped]),
            ],
        );

        let (maintainers, resolved) = apply_lead_to_maintainers(
            Some(lead),
            &me,
            vec![me, third, lead, dropped],
            Some(&my_ref),
            Some(&consolidated),
        )
        .unwrap();
        assert_eq!(maintainers, vec![me, lead]);
        assert_eq!(resolved, Some(lead));
    }
}

#[cfg(test)]
mod derive_remote_name_from_url_tests {
    use super::derive_remote_name_from_url;

    #[test]
    fn two_label_domains_use_the_first_label() {
        assert_eq!(
            derive_remote_name_from_url("https://github.com/foo/bar.git").as_deref(),
            Some("github")
        );
        assert_eq!(
            derive_remote_name_from_url("https://codeberg.org/foo/bar").as_deref(),
            Some("codeberg")
        );
    }

    #[test]
    fn multi_label_domains_drop_the_suffix_and_dash_join_the_rest() {
        assert_eq!(
            derive_remote_name_from_url("https://git.fiatjaf.com/ngit").as_deref(),
            Some("git-fiatjaf")
        );
    }

    #[test]
    fn ip_hosts_keep_every_octet() {
        assert_eq!(
            derive_remote_name_from_url("http://127.0.0.1:8080/repo.git").as_deref(),
            Some("127-0-0-1")
        );
    }

    #[test]
    fn single_label_hosts_are_used_as_is() {
        assert_eq!(
            derive_remote_name_from_url("http://localhost:8080/repo.git").as_deref(),
            Some("localhost")
        );
    }

    #[test]
    fn ssh_scp_style_urls_derive_from_the_host() {
        assert_eq!(
            derive_remote_name_from_url("git@github.com:foo/bar.git").as_deref(),
            Some("github")
        );
    }
}

#[cfg(test)]
mod git_server_url_validation_tests {
    use super::validate_git_server_url;

    #[test]
    fn accepts_urls_dispatched_by_git_remote_helpers() {
        for url in [
            "htree://npub1example/project",
            "ext::%S /tmp/project.git",
            "custom+git://example.test/project",
        ] {
            assert_eq!(validate_git_server_url(url).unwrap(), url);
        }
    }

    #[test]
    fn retains_builtin_url_validation() {
        assert!(validate_git_server_url("https://example.test/project.git").is_ok());
        assert!(validate_git_server_url("not a git URL").is_err());
    }

    #[test]
    fn rejects_unsafe_or_reserved_helper_schemes() {
        for url in [
            "nostr://npub1example/project",
            "NoStR::npub1example/project",
            "fd::0,1/project",
            "ws://relay.example.com",
            "WSS://relay.example.com",
        ] {
            assert!(validate_git_server_url(url).is_err(), "{url}");
        }
    }
}
