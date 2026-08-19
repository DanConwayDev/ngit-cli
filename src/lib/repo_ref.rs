use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use console::Style;
use nostr::prelude::{
    FromBech32, Kind, PublicKey, RelayUrl, Tag, Timestamp, ToBech32, Url, nip01::Coordinate,
    nip19::Nip19Coordinate,
};
use serde::{Deserialize, Serialize};
use urlencoding::encode as pct_encode;

#[cfg(not(test))]
use crate::client::Client;
#[cfg(not(test))]
use crate::login::{existing::load_existing_login, user::discover_private_git_relay_list};
use crate::{
    UrlWithoutSlash,
    cli_interactor::{
        Interactor, InteractorPrompt, PromptChoiceParms, PromptConfirmParms, PromptInputParms,
    },
    client::{
        Connect, PrivateRelayProbeDecision, consolidate_fetch_outcome, get_repo_ref_from_cache,
        private_relay_probe_decision,
    },
    git::{
        Repo, RepoActions,
        nostr_url::{NostrUrlDecoded, use_nip05_git_config_cache_to_find_nip05_from_public_key},
    },
    login::user::{PrivateGitRelayDiscovery, get_user_details},
};

#[derive(Clone)]
pub struct RepoRef {
    pub name: String,
    pub description: String,
    pub identifier: String,
    pub root_commit: String,
    pub git_server: Vec<String>,
    pub web: Vec<String>,
    /// Informational NIP-34 `u` tags indicating this repository is a
    /// subordinate fork of another repository. Each inner vector contains the
    /// tag fields after the leading `u` tag name.
    pub upstream: Vec<Vec<String>>,
    pub relays: Vec<RelayUrl>,
    pub blossoms: Vec<Url>,
    pub hashtags: Vec<String>,
    /// Whether this announcement marks the repository as private.
    ///
    /// A consolidated [`RepoRef`] is private when any announcement in its
    /// recursive maintainer set carries `["private", "true"]`.
    pub private: bool,
    pub maintainers: Vec<PublicKey>,
    pub selected_maintainer: PublicKey,
    // set to None if not known
    pub maintainers_without_annoucnement: Option<Vec<PublicKey>>,
    pub events: HashMap<Nip19Coordinate, nostr::prelude::Event>,
    pub nostr_git_url: Option<NostrUrlDecoded>,
    /// Tags on the source announcement event whose first slot is not a name
    /// this version of ngit knows about. Round-tripped verbatim on republish
    /// so that tags added by a future ngit version or a third-party tool are
    /// not silently dropped. See [`is_known_tag_name`] for the allowlist of
    /// names this field excludes.
    pub extra_tags: Vec<Tag>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MaintainerEdge {
    pub from: PublicKey,
    pub to: PublicKey,
}

fn graph_reaches(from: PublicKey, target: PublicKey, edges: &[MaintainerEdge]) -> bool {
    let mut pending = vec![from];
    let mut seen = HashSet::new();
    while let Some(current) = pending.pop() {
        if !seen.insert(current) {
            continue;
        }
        if current == target {
            return true;
        }
        pending.extend(
            edges
                .iter()
                .filter_map(|edge| (edge.from == current).then_some(edge.to)),
        );
    }
    false
}

/// Names of tags ngit itself parses on `kind:30617` (`GitRepoAnnouncement`)
/// events. Used by [`RepoRef::try_from`] to decide whether a tag is "ours"
/// (consumed by a typed field, with duplicates collapsed on re-emission) or
/// foreign (preserved verbatim in [`RepoRef::extra_tags`]).
///
/// `alt` is in the list because [`RepoRef::to_event`] regenerates it from
/// `self.name`; a stale `alt` on the source event would otherwise survive
/// alongside the regenerated one.
pub fn is_known_tag_name(name: &str) -> bool {
    matches!(
        name,
        "d" | "name"
            | "description"
            | "clone"
            | "web"
            | "u"
            | "r"
            | "relays"
            | "t"
            | "blossoms"
            | "maintainers"
            | "private"
            | "alt"
    )
}

impl TryFrom<(nostr::prelude::Event, Option<PublicKey>)> for RepoRef {
    type Error = anyhow::Error;

    /*
     * this could do with a refactor to intergrate enhancements made by
     * `get_repo_ref_from_cache`. Other than tests, its only used there and the
     * changes made by that function are important.
     */
    fn try_from(
        (event, selected_maintainer): (nostr::prelude::Event, Option<PublicKey>),
    ) -> Result<Self> {
        // TODO: turn selected maintainer into NostrUrlDecoded
        if !event.kind.eq(&Kind::GitRepoAnnouncement) {
            bail!("incorrect kind");
        }

        let mut r = Self {
            name: String::new(),
            description: String::new(),
            identifier: String::new(),
            root_commit: String::new(),
            git_server: Vec::new(),
            web: Vec::new(),
            upstream: Vec::new(),
            relays: Vec::new(),
            blossoms: Vec::new(),
            hashtags: Vec::new(),
            private: false,
            maintainers: Vec::new(),
            selected_maintainer: selected_maintainer.unwrap_or(event.pubkey),
            maintainers_without_annoucnement: None,
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: Vec::new(),
        };

        for tag in event.tags.iter() {
            match tag.as_slice() {
                [t, id, ..] if t == "d" => r.identifier = id.clone(),
                [t, name, ..] if t == "name" => r.name = name.clone(),
                [t, description, ..] if t == "description" => r.description = description.clone(),
                [t, clone @ ..] if t == "clone" => {
                    for git_server in clone {
                        if !r.git_server.contains(git_server) {
                            r.git_server.push(git_server.clone());
                        }
                    }
                    r.git_server = clone.to_vec();
                }
                [t, web @ ..] if t == "web" => {
                    r.web = web.to_vec();
                }
                [t, upstream @ ..] if t == "u" && !upstream.is_empty() => {
                    r.upstream.push(upstream.to_vec());
                }
                [t, commit_id]
                    if t == "r"
                        && commit_id.len() == 40
                        && git2::Oid::from_str(commit_id).is_ok() =>
                {
                    r.root_commit = commit_id.clone();
                }
                [t, commit_id, marker]
                    if t == "r"
                        && marker == "euc"
                        && commit_id.len() == 40
                        && git2::Oid::from_str(commit_id).is_ok() =>
                {
                    r.root_commit = commit_id.clone();
                }
                [t, relays @ ..] if t == "relays" => {
                    for relay in relays {
                        if let Ok(relay_url) = RelayUrl::parse(relay) {
                            if !r.relays.contains(&relay_url) {
                                r.relays.push(relay_url);
                            }
                        }
                    }
                }
                [t, hashtag, ..] if t == "t" => r.hashtags.push(hashtag.clone()),
                [t, value] if t == "private" && value == "true" => r.private = true,
                [t, ..] if t == "buzz-channel" => {
                    // Buzz uses this foreign tag as the repository ACL: every
                    // Smart HTTP operation requires channel membership and a
                    // repository-scoped NIP-98 credential. Preserve the tag
                    // verbatim while applying ngit's private transport and
                    // relay-routing policy.
                    r.private = true;
                    r.extra_tags.push(tag.clone());
                }
                [t, blossoms @ ..] if t == "blossoms" => {
                    for b in blossoms {
                        if let Ok(b) = Url::parse(b) {
                            if !r.blossoms.contains(&b) {
                                r.blossoms.push(b);
                            }
                        }
                    }
                }
                [t, maintainers @ ..] if t == "maintainers" => {
                    if !maintainers.contains(&event.pubkey.to_string()) {
                        r.maintainers.push(event.pubkey);
                    }
                    for pk in maintainers {
                        r.maintainers.push(
                            PublicKey::from_str(pk)
                                .context(format!("failed to convert entry from maintainers tag {pk} into a valid nostr public key. it should be in hex format"))
                                .context("invalid repository event")?,
                        );
                    }
                }
                _ => {
                    // Catch-all: any tag that didn't match a typed arm above.
                    //
                    // - If the first slot is a *known* tag name, drop it. Either the typed arm
                    //   already consumed an earlier occurrence (this is a duplicate that would
                    //   otherwise smuggle past ngit's "one tag per known name on emission"
                    //   invariant) or the tag is malformed for its known shape. Either way, ngit's
                    //   typed field is the single source of truth on republish.
                    // - Otherwise the tag is foreign — preserve it verbatim so a future ngit
                    //   version's or third-party tool's tag isn't silently stripped on the next
                    //   republish.
                    let first = tag.as_slice().first().map(String::as_str);
                    if !first.is_some_and(is_known_tag_name) {
                        r.extra_tags.push(tag.clone());
                    }
                }
            }
        }

        // If no maintainers were added, add the event's public key
        if r.maintainers.is_empty() {
            r.maintainers.push(event.pubkey);
        }
        r.events = HashMap::new();
        r.events.insert(
            Nip19Coordinate {
                coordinate: Coordinate {
                    kind: event.kind,
                    identifier: event.tags.identifier().unwrap().to_string(),
                    public_key: event.pubkey,
                },
                relays: vec![],
            },
            event,
        );
        Ok(r)
    }
}

impl RepoRef {
    pub async fn to_event(&self, signer: &Arc<crate::NgitSigner>) -> Result<nostr::prelude::Event> {
        let builder =
            nostr::prelude::EventBuilder::new(nostr::event::Kind::GitRepoAnnouncement, "").tags(
                [
                    vec![
                        Tag::identifier(if self.identifier.to_string().is_empty() {
                            // fiatjaf thought a random string. its not in the draft nip.
                            // thread_rng()
                            //     .sample_iter(&Alphanumeric)
                            //     .take(15)
                            //     .map(char::from)
                            //     .collect()

                            // an identifier based on first commit is better so that users dont
                            // accidentally create two seperate identifiers for the same repo
                            // there is a hesitancy to use the commit id
                            // in another conversaion with fiatjaf he suggested the first 6
                            // character of the commit id
                            // here we are using 7 which is the standard for shorthand commit id
                            self.root_commit.to_string()[..7].to_string()
                        } else {
                            self.identifier.to_string()
                        }),
                        Tag::parse(["r", &self.root_commit, "euc"]).unwrap(),
                        Tag::parse(["name", &self.name]).unwrap(),
                        Tag::parse(["description", &self.description]).unwrap(),
                        Tag::parse([vec!["clone".to_string()], self.git_server.clone()].concat())
                            .unwrap(),
                        Tag::parse([vec!["web".to_string()], self.web.clone()].concat()).unwrap(),
                        Tag::parse(
                            [
                                vec!["relays".to_string()],
                                self.relays
                                    .iter()
                                    .map(|r| r.to_string())
                                    .collect::<Vec<_>>(),
                            ]
                            .concat(),
                        )
                        .unwrap(),
                        Tag::parse(
                            [
                                vec!["maintainers".to_string()],
                                self.maintainers
                                    .iter()
                                    .map(|pk| pk.to_string())
                                    .collect::<Vec<_>>(),
                            ]
                            .concat(),
                        )
                        .unwrap(),
                        Tag::parse(["alt", &format!("git repository: {}", self.name)]).unwrap(),
                    ],
                    self.hashtags
                        .iter()
                        .map(|h| Tag::parse(["t", h]).unwrap())
                        .collect(),
                    self.upstream
                        .iter()
                        .map(|upstream| {
                            Tag::parse([vec!["u".to_string()], upstream.clone()].concat()).unwrap()
                        })
                        .collect(),
                    if self.private {
                        vec![Tag::parse(["private", "true"]).unwrap()]
                    } else {
                        vec![]
                    },
                    if self.blossoms.is_empty() {
                        vec![]
                    } else {
                        vec![
                            Tag::parse(
                                [
                                    vec!["blossoms".to_string()],
                                    self.blossoms
                                        .iter()
                                        .map(|b| b.to_string_without_trailing_slash())
                                        .collect::<Vec<_>>(),
                                ]
                                .concat(),
                            )
                            .unwrap(),
                        ]
                    },
                    // Unknown tags carried over verbatim from the source
                    // announcement. See [`RepoRef::extra_tags`] and
                    // [`is_known_tag_name`]: ngit-known names never end up
                    // here (they round-trip through their typed field), so
                    // appending unconditionally cannot duplicate a typed
                    // tag emitted above.
                    self.extra_tags.clone(),
                    // code languages and hashtags
                ]
                .concat(),
            );
        let public_key = signer.get_public_key().await?;
        crate::client::sign_draft_event(
            crate::event_ordering::finalize_ordered_unsigned(
                builder,
                public_key,
                crate::event_ordering::latest_event(self.events.values()),
            )?,
            signer,
            "repo announcement".to_string(),
        )
        .await
        .context("failed to create repository reference event")
    }
    /// coordinates without relay hints
    pub fn coordinates(&self) -> HashSet<Nip19Coordinate> {
        let mut res = HashSet::new();
        res.insert(Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: self.selected_maintainer,
                identifier: self.identifier.clone(),
            },
            relays: vec![],
        });

        for m in &self.maintainers {
            res.insert(Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: *m,
                    identifier: self.identifier.clone(),
                },
                relays: vec![],
            });
        }
        res
    }

    /// Maintainers in announcement-tag order.
    ///
    /// The maintainer selected by the `nostr://` URL or explicit repo
    /// coordinate is always first. Confirmed maintainers come before invited
    /// maintainers. This keeps PR/issue repository `a` tags anchored to the
    /// reciprocal group while still tagging every authorized maintainer.
    pub fn maintainers_for_announcement_tags(&self) -> Vec<PublicKey> {
        let confirmed: HashSet<PublicKey> = self.confirmed_maintainers().into_iter().collect();

        let mut ordered = Vec::new();
        let mut seen = HashSet::new();

        if seen.insert(self.selected_maintainer) {
            ordered.push(self.selected_maintainer);
        }

        for maintainer in &self.maintainers {
            if confirmed.contains(maintainer) && seen.insert(*maintainer) {
                ordered.push(*maintainer);
            }
        }

        for maintainer in &self.maintainers {
            if !confirmed.contains(maintainer) && seen.insert(*maintainer) {
                ordered.push(*maintainer);
            }
        }

        ordered
    }

    /// Directed maintainer relationships from the announcements we know.
    pub fn maintainer_edges(&self) -> Vec<MaintainerEdge> {
        let mut edges = Vec::new();
        let mut seen = HashSet::new();
        for event in self.events.values() {
            let Ok(event_ref) = RepoRef::try_from((event.clone(), None)) else {
                continue;
            };
            for to in event_ref.maintainers {
                if to != event.pubkey && seen.insert((event.pubkey, to)) {
                    edges.push(MaintainerEdge {
                        from: event.pubkey,
                        to,
                    });
                }
            }
        }
        edges.sort_by_key(|edge| (edge.from.to_hex(), edge.to.to_hex()));
        edges
    }

    /// Maintainers in the selected maintainer's reciprocally connected group.
    ///
    /// Every maintainer reachable from the selected announcement is authorized.
    /// Reciprocal connectivity is only the accepted-membership framing used in
    /// user-facing output; it does not restrict event authority.
    pub fn confirmed_maintainers(&self) -> Vec<PublicKey> {
        let edges = self.maintainer_edges();
        self.maintainers
            .iter()
            .copied()
            .filter(|maintainer| {
                *maintainer == self.selected_maintainer
                    || graph_reaches(*maintainer, self.selected_maintainer, &edges)
            })
            .collect()
    }

    /// Authorized maintainers not reciprocally connected to the selected group.
    pub fn invited_maintainers(&self) -> Vec<PublicKey> {
        let confirmed: HashSet<_> = self.confirmed_maintainers().into_iter().collect();
        self.maintainers
            .iter()
            .copied()
            .filter(|maintainer| !confirmed.contains(maintainer))
            .collect()
    }

    /// The unique confirmed maintainer with the highest positive in-degree.
    /// Ties and graphs with no maintainer-to-maintainer edges have no lead.
    pub fn lead_maintainer(&self) -> Option<PublicKey> {
        let confirmed = self.confirmed_maintainers();
        let confirmed_set: HashSet<_> = confirmed.iter().copied().collect();
        let mut counts: HashMap<PublicKey, usize> = confirmed
            .iter()
            .copied()
            .map(|maintainer| (maintainer, 0))
            .collect();
        for edge in self.maintainer_edges() {
            if confirmed_set.contains(&edge.from) && confirmed_set.contains(&edge.to) {
                *counts.entry(edge.to).or_default() += 1;
            }
        }
        let highest = counts.values().copied().max().unwrap_or_default();
        if highest == 0 {
            return None;
        }
        let mut leaders = counts
            .into_iter()
            .filter_map(|(maintainer, count)| (count == highest).then_some(maintainer));
        let lead = leaders.next()?;
        leaders.next().is_none().then_some(lead)
    }

    /// coordinates without relay hints
    pub fn coordinate_with_hint(&self) -> Nip19Coordinate {
        Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: self.selected_maintainer,
                identifier: self.identifier.clone(),
            },
            relays: if let Some(relay) = self.relays.first() {
                vec![relay.clone()]
            } else {
                vec![]
            },
        }
    }

    /// coordinates without relay hints
    pub fn coordinates_with_timestamps(&self) -> Vec<(Nip19Coordinate, Option<Timestamp>)> {
        self.coordinates()
            .iter()
            .map(|c| (c.clone(), self.events.get(c).map(|e| e.created_at)))
            .collect::<Vec<(Nip19Coordinate, Option<Timestamp>)>>()
    }

    pub fn set_nostr_git_url(&mut self, nostr_git_url: NostrUrlDecoded) {
        self.nostr_git_url = Some(nostr_git_url)
    }

    pub fn to_nostr_git_url(&self, git_repo: &Option<&Repo>) -> NostrUrlDecoded {
        if let Some(nostr_git_url) = &self.nostr_git_url {
            return nostr_git_url.clone();
        }
        let c = self.coordinate_with_hint();
        NostrUrlDecoded {
            original_string: String::new(),
            nip05: use_nip05_git_config_cache_to_find_nip05_from_public_key(
                &c.public_key,
                git_repo,
            )
            .unwrap_or_default(),
            coordinate: c,
            protocol: None,
            ssh_key_file: None,
        }
    }

    pub fn grasp_servers(&self) -> Vec<String> {
        detect_existing_grasp_servers(Some(self), &[], &[], &self.identifier)
    }

    // returns false if already present so didn't need adding
    pub fn add_grasp_server(&mut self, clone_url: &str) -> Result<bool> {
        if !is_grasp_server_clone_url(clone_url) {
            bail!("invalid grasp server clone url. does not end with .git");
        }

        let relay_url = RelayUrl::parse(
            &format_grasp_server_url_as_relay_url(clone_url)
                .context("invalid grasp server clone url")?,
        )
        .context("invalid grasp server clone url")?;

        if !self.relays.contains(&relay_url) {
            self.relays.push(relay_url);
        }
        if !self.git_server.contains(&clone_url.to_string()) {
            self.git_server.push(clone_url.to_string());
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Describes where the resolved repository coordinate came from. Used to
/// print a diagnostic line so operators can catch mis-targeting BEFORE a
/// repo-scoped event is published (see suggested-fix 5 in the source issue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoCoordinateSource {
    /// Explicit `--repo` argument that matched a configured remote name.
    RepoArgRemoteName(String),
    /// Explicit `--repo` argument parsed as an naddr.
    RepoArgNaddr,
    /// Explicit `--repo` argument parsed as a `nostr://` URL.
    RepoArgNostrUrl,
    /// `nostr.repo` git config value.
    NostrRepoConfig,
    /// The current branch's tracked upstream remote.
    TrackedUpstreamRemote(String),
    /// The `origin` remote.
    OriginRemote,
    /// A single remaining distinct coordinate among nostr:// remotes.
    SingleRemainingRemote(String),
    /// A deterministic user choice made in interactive mode.
    InteractiveSelection(String),
    /// The `maintainers.yaml` file (legacy).
    MaintainersYaml,
    /// A coordinate obtained by prompting the user (interactive fallback).
    UserPrompt,
    /// A new repository coordinate derived during `ngit init`.
    NewRepository,
}

impl RepoCoordinateSource {
    /// Short human-readable label for CLI diagnostic output.
    pub fn label(&self) -> String {
        match self {
            Self::RepoArgRemoteName(name) => format!("--repo {name} (git remote)"),
            Self::RepoArgNaddr => "--repo <naddr>".to_string(),
            Self::RepoArgNostrUrl => "--repo <nostr:// url>".to_string(),
            Self::NostrRepoConfig => "git config nostr.repo".to_string(),
            Self::TrackedUpstreamRemote(name) => {
                format!("tracked upstream of current branch ({name})")
            }
            Self::OriginRemote => "origin remote".to_string(),
            Self::SingleRemainingRemote(name) => {
                format!("sole remaining nostr:// remote ({name})")
            }
            Self::InteractiveSelection(name) => format!("interactive selection ({name})"),
            Self::MaintainersYaml => "maintainers.yaml".to_string(),
            Self::UserPrompt => "interactive prompt".to_string(),
            Self::NewRepository => "new repository".to_string(),
        }
    }
}

/// Result of resolving the target repository coordinate.
#[derive(Debug, Clone)]
pub struct ResolvedRepoCoordinate {
    pub coordinate: Nip19Coordinate,
    pub source: RepoCoordinateSource,
    /// Configured `nostr://` remote selected by the same resolution decision.
    /// Coordinate-only sources such as `nostr.repo` may leave this unset until
    /// a caller specifically needs a matching Git remote.
    pub remote: Option<ResolvedNostrRemote>,
}

#[derive(Debug, Clone)]
pub struct ResolvedNostrRemote {
    pub name: String,
    pub decoded_url: NostrUrlDecoded,
}

/// Environment variable used to pass a global `--repo` override from the
/// binary layer into the lib without threading it through every subcommand.
pub const NGIT_REPO_ENV: &str = "NGIT_REPO";

/// Environment variable used to signal interactive mode (set by the binary
/// when `-i` is supplied). When absent, the resolver refuses to prompt.
pub const NGIT_INTERACTIVE_ENV: &str = "NGIT_INTERACTIVE_MODE";

#[derive(Debug, Clone, Default)]
struct RepoCoordinateResolutionOptions {
    repo_override: Option<String>,
    interactive: bool,
}

impl RepoCoordinateResolutionOptions {
    fn from_env() -> Self {
        Self {
            repo_override: std::env::var(NGIT_REPO_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
            interactive: std::env::var(NGIT_INTERACTIVE_ENV).is_ok_and(|value| !value.is_empty()),
        }
    }
}

/// Resolve the target repository coordinate for a repo-scoped operation.
///
/// Documented priority (per repository-coordinate-resolution policy):
///
/// 1. Explicit `--repo <REMOTE|NADDR|NOSTR-URL>` (via the [`NGIT_REPO_ENV`] env
///    var). The value is first matched against configured remote names, then
///    parsed as an naddr, then as a `nostr://` URL.
/// 2. `nostr.repo` git config value (canonical naddr).
/// 3. The current branch's tracked upstream remote, when it is a valid
///    `nostr://` remote.
/// 4. The `origin` remote, when it is a valid `nostr://` remote.
/// 5. The sole distinct coordinate found among the remaining `nostr://`
///    remotes.
///
/// If multiple coordinates remain after applying these rules, this function
/// errors by default and explains how to disambiguate. Interactive selection
/// is only offered when `-i` was explicitly requested (via
/// [`NGIT_INTERACTIVE_ENV`]). There is no implicit prompt or "first remote
/// wins" fallback.
pub async fn resolve_repo_coordinate(git_repo: &Repo) -> Result<ResolvedRepoCoordinate> {
    resolve_repo_coordinate_with_options(git_repo, &RepoCoordinateResolutionOptions::from_env())
        .await
}

async fn resolve_repo_coordinate_with_options(
    git_repo: &Repo,
    options: &RepoCoordinateResolutionOptions,
) -> Result<ResolvedRepoCoordinate> {
    try_resolve_repo_coordinate_with_options(git_repo, options)
        .await?
        .context("no nostr git remotes or git config \"nostr.repo\" value")
}

pub async fn try_resolve_repo_coordinate(
    git_repo: &Repo,
) -> Result<Option<ResolvedRepoCoordinate>> {
    try_resolve_repo_coordinate_with_options(git_repo, &RepoCoordinateResolutionOptions::from_env())
        .await
}

async fn try_resolve_repo_coordinate_with_options(
    git_repo: &Repo,
    options: &RepoCoordinateResolutionOptions,
) -> Result<Option<ResolvedRepoCoordinate>> {
    // 1. Explicit --repo override.
    if let Some(raw) = options.repo_override.as_deref() {
        return resolve_repo_override(git_repo, raw).await.map(Some);
    }

    // 2. nostr.repo git config.
    if let Some(c) = get_repo_coordinates_from_git_config(git_repo)? {
        return Ok(Some(ResolvedRepoCoordinate {
            coordinate: c,
            source: RepoCoordinateSource::NostrRepoConfig,
            remote: None,
        }));
    }

    let nostr_remotes = get_nostr_remotes(git_repo).await?;

    if nostr_remotes.is_empty() {
        // Legacy fallback: maintainers.yaml.
        return Ok(get_repo_coordinates_from_maintainers_yaml(git_repo)
            .await
            .map(|c| ResolvedRepoCoordinate {
                coordinate: c,
                source: RepoCoordinateSource::MaintainersYaml,
                remote: None,
            })
            .ok());
    }

    // 3. Tracked upstream of the current branch, if it is a nostr:// remote.
    if let Some(remote) = find_tracked_upstream_nostr_remote(git_repo, &nostr_remotes) {
        return Ok(Some(ResolvedRepoCoordinate {
            coordinate: remote.decoded_url.coordinate.clone(),
            source: RepoCoordinateSource::TrackedUpstreamRemote(remote.name.clone()),
            remote: Some(remote),
        }));
    }

    // 4. `origin`, if it is a nostr:// remote.
    if let Some(decoded_url) = nostr_remotes.get("origin").cloned() {
        let remote = ResolvedNostrRemote {
            name: "origin".to_string(),
            decoded_url,
        };
        return Ok(Some(ResolvedRepoCoordinate {
            coordinate: remote.decoded_url.coordinate.clone(),
            source: RepoCoordinateSource::OriginRemote,
            remote: Some(remote),
        }));
    }

    // 5. Sole distinct coordinate among the remaining nostr:// remotes.
    let distinct_remotes = distinct_nostr_remotes(&nostr_remotes);
    if distinct_remotes.len() == 1 {
        let remote = distinct_remotes.into_iter().next().unwrap();
        return Ok(Some(ResolvedRepoCoordinate {
            coordinate: remote.decoded_url.coordinate.clone(),
            source: RepoCoordinateSource::SingleRemainingRemote(remote.name.clone()),
            remote: Some(remote),
        }));
    }

    // Ambiguous: multiple distinct coordinates and no explicit selection.
    if options.interactive {
        // Interactive: deterministic ordering (sorted by remote name), no
        // "first remote wins" fallback.
        let sorted = sorted_nostr_remotes(&nostr_remotes);
        let labels = get_nostr_git_remote_selection_labels_ordered(git_repo, &sorted).await?;
        let choice_index = Interactor::default().choice(
            PromptChoiceParms::default()
                .with_prompt("select nostr repository from those listed as git remotes")
                .with_default(0)
                .with_choices(labels),
        )?;
        let remote = sorted
            .get(choice_index)
            .context("invalid interactive choice index")?
            .clone();
        return Ok(Some(ResolvedRepoCoordinate {
            coordinate: remote.decoded_url.coordinate.clone(),
            source: RepoCoordinateSource::InteractiveSelection(remote.name.clone()),
            remote: Some(remote),
        }));
    }

    bail!(format_ambiguous_error(&nostr_remotes));
}

async fn resolve_repo_override(git_repo: &Repo, raw: &str) -> Result<ResolvedRepoCoordinate> {
    // 1a. Match against configured remote names.
    let nostr_remotes = get_nostr_remotes(git_repo).await?;
    if let Some(decoded_url) = nostr_remotes.get(raw).cloned() {
        let remote = ResolvedNostrRemote {
            name: raw.to_string(),
            decoded_url,
        };
        return Ok(ResolvedRepoCoordinate {
            coordinate: remote.decoded_url.coordinate.clone(),
            source: RepoCoordinateSource::RepoArgRemoteName(raw.to_string()),
            remote: Some(remote),
        });
    }

    // 1b. Parse as an naddr.
    if let Ok(c) = Nip19Coordinate::from_bech32(raw) {
        let remote = find_matching_nostr_remote(&nostr_remotes, &c);
        return Ok(ResolvedRepoCoordinate {
            coordinate: c,
            source: RepoCoordinateSource::RepoArgNaddr,
            remote,
        });
    }

    // 1c. Parse as a nostr:// URL.
    if let Ok(nostr_url) = NostrUrlDecoded::parse_and_resolve(raw, &Some(git_repo)).await {
        let remote = find_matching_nostr_remote(&nostr_remotes, &nostr_url.coordinate);
        return Ok(ResolvedRepoCoordinate {
            coordinate: nostr_url.coordinate,
            source: RepoCoordinateSource::RepoArgNostrUrl,
            remote,
        });
    }

    bail!(
        "--repo value {raw:?} did not match any configured git remote name and is neither a valid naddr nor a nostr:// URL"
    )
}

/// Return configured Nostr remotes sorted by name. This is the canonical
/// ordering for both interactive-prompt display and user-visible listings.
fn sorted_nostr_remotes(
    nostr_remotes: &HashMap<String, NostrUrlDecoded>,
) -> Vec<ResolvedNostrRemote> {
    let mut remotes: Vec<ResolvedNostrRemote> = nostr_remotes
        .iter()
        .map(|(name, decoded_url)| ResolvedNostrRemote {
            name: name.clone(),
            decoded_url: decoded_url.clone(),
        })
        .collect();
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    remotes
}

/// Collapse the remote map to at-most-one entry per distinct coordinate
/// (pubkey + identifier), keeping the lexicographically-first remote name
/// as the representative. Returned in deterministic order.
fn distinct_nostr_remotes(
    nostr_remotes: &HashMap<String, NostrUrlDecoded>,
) -> Vec<ResolvedNostrRemote> {
    let sorted = sorted_nostr_remotes(nostr_remotes);
    let mut seen: HashSet<(PublicKey, String)> = HashSet::new();
    let mut out = vec![];
    for remote in sorted {
        let coordinate = &remote.decoded_url.coordinate;
        let key = (coordinate.public_key, coordinate.identifier.clone());
        if seen.insert(key) {
            out.push(remote);
        }
    }
    out
}

fn coordinates_match(a: &Nip19Coordinate, b: &Nip19Coordinate) -> bool {
    a.coordinate == b.coordinate
}

fn find_matching_nostr_remote(
    nostr_remotes: &HashMap<String, NostrUrlDecoded>,
    coordinate: &Nip19Coordinate,
) -> Option<ResolvedNostrRemote> {
    sorted_nostr_remotes(nostr_remotes)
        .into_iter()
        .find(|remote| coordinates_match(&remote.decoded_url.coordinate, coordinate))
}

/// If the currently checked-out branch has a tracked upstream whose remote
/// resolves to a `nostr://` coordinate, return that remote.
fn find_tracked_upstream_nostr_remote(
    git_repo: &Repo,
    nostr_remotes: &HashMap<String, NostrUrlDecoded>,
) -> Option<ResolvedNostrRemote> {
    let branch = git_repo.get_checked_out_branch_name().ok()?;
    let remote = git_repo.get_upstream_remote_for_branch(&branch).ok()??;
    nostr_remotes
        .get(&remote)
        .cloned()
        .map(|decoded_url| ResolvedNostrRemote {
            name: remote,
            decoded_url,
        })
}

fn format_ambiguous_error(nostr_remotes: &HashMap<String, NostrUrlDecoded>) -> String {
    let sorted = sorted_nostr_remotes(nostr_remotes);
    let mut lines = vec![
        "multiple nostr:// git remotes disagree on the target repository and no explicit selection was made".to_string(),
        String::new(),
        "remotes and their coordinates:".to_string(),
    ];
    for remote in &sorted {
        let naddr = remote
            .decoded_url
            .coordinate
            .to_bech32()
            .unwrap_or_else(|_| "<invalid>".to_string());
        lines.push(format!("  {}: {naddr}", remote.name));
    }
    lines.push(String::new());
    lines.push("disambiguate by one of:".to_string());
    lines.push(
        "  * pass `--repo <REMOTE-NAME|naddr|nostr:// URL>` as a global argument".to_string(),
    );
    lines.push("  * set `git config nostr.repo <naddr>` in this repository".to_string());
    lines.push("  * remove or rename conflicting `nostr://` remotes".to_string());
    lines.push("  * re-run with `-i` for interactive selection".to_string());
    lines.join("\n")
}

async fn get_nostr_git_remote_selection_labels_ordered(
    git_repo: &Repo,
    ordered: &[ResolvedNostrRemote],
) -> Result<Vec<String>> {
    let mut res = vec![];
    for remote in ordered {
        let coordinate = &remote.decoded_url.coordinate;
        res.push(format!(
            "{} - {}/{}",
            remote.name,
            get_user_details(
                &coordinate.public_key,
                None,
                Some(git_repo.get_path()?),
                true,
                false
            )
            .await?
            .metadata
            .name,
            coordinate.identifier
        ));
    }
    Ok(res)
}

pub async fn get_resolved_repo_coordinate_when_remote_unknown(
    git_repo: &Repo,
    #[cfg(test)] client: &mut crate::client::MockConnect,
    #[cfg(not(test))] client: &mut Client,
) -> Result<ResolvedRepoCoordinate> {
    let options = RepoCoordinateResolutionOptions::from_env();
    match try_resolve_repo_coordinate_with_options(git_repo, &options).await? {
        Some(resolved) => Ok(resolved),
        None if options.interactive => {
            let private_discovery = {
                #[cfg(test)]
                {
                    PrivateGitRelayDiscovery::Absent
                }
                #[cfg(not(test))]
                {
                    if let Ok((signer, user_ref, _)) = load_existing_login(
                        &Some(git_repo),
                        &None,
                        &None,
                        &None,
                        None,
                        true,
                        false,
                        false,
                    )
                    .await
                    {
                        client.set_signer(signer.clone()).await;
                        let mut discovery_relays = user_ref.relays.read();
                        for relay in user_ref.relays.write() {
                            if !discovery_relays.contains(&relay) {
                                discovery_relays.push(relay);
                            }
                        }
                        if discovery_relays.is_empty() {
                            discovery_relays.extend(client.get_relay_default_set().iter().cloned());
                        }
                        discover_private_git_relay_list(client, discovery_relays, &signer).await
                    } else {
                        PrivateGitRelayDiscovery::Absent
                    }
                }
            };
            let c =
                get_repo_coordinate_from_user_prompt(git_repo, client, &private_discovery).await?;
            Ok(ResolvedRepoCoordinate {
                coordinate: c.clone(),
                source: RepoCoordinateSource::UserPrompt,
                remote: find_nostr_remote_for_coordinate(git_repo, &c).await?,
            })
        }
        None => bail!("no nostr git remotes or git config \"nostr.repo\" value"),
    }
}

pub async fn get_repo_coordinates_when_remote_unknown(
    git_repo: &Repo,
    #[cfg(test)] client: &mut crate::client::MockConnect,
    #[cfg(not(test))] client: &mut Client,
) -> Result<Nip19Coordinate> {
    get_resolved_repo_coordinate_when_remote_unknown(git_repo, client)
        .await
        .map(|resolved| resolved.coordinate)
}

pub async fn get_repo_coordinates_for_publishing(
    git_repo: &Repo,
    #[cfg(test)] client: &mut crate::client::MockConnect,
    #[cfg(not(test))] client: &mut Client,
) -> Result<Nip19Coordinate> {
    get_resolved_repo_coordinate_for_publishing(git_repo, client)
        .await
        .map(|resolved| resolved.coordinate)
}

pub async fn get_resolved_repo_coordinate_for_publishing(
    git_repo: &Repo,
    #[cfg(test)] client: &mut crate::client::MockConnect,
    #[cfg(not(test))] client: &mut Client,
) -> Result<ResolvedRepoCoordinate> {
    let resolved = get_resolved_repo_coordinate_when_remote_unknown(git_repo, client).await?;
    print_selected_repo(&resolved);
    Ok(resolved)
}

pub async fn try_and_get_repo_coordinates_when_remote_unknown(
    git_repo: &Repo,
) -> Result<Nip19Coordinate> {
    resolve_repo_coordinate(git_repo)
        .await
        .map(|r| r.coordinate)
}

/// Print a single line identifying the selected target repository, so an
/// operator can immediately see if a repo-scoped event is about to be
/// published against the wrong coordinate.
pub fn print_selected_repo(resolved: &ResolvedRepoCoordinate) {
    // Suppress in test builds to keep unit tests deterministic; integration
    // tests can opt-in with NGIT_PRINT_SELECTED_REPO=1.
    if cfg!(test) && std::env::var("NGIT_PRINT_SELECTED_REPO").is_err() {
        return;
    }
    let dim = Style::new().color256(247);
    let naddr = resolved
        .coordinate
        .to_bech32()
        .unwrap_or_else(|_| "<invalid naddr>".to_string());
    eprintln!(
        "{}",
        dim.apply_to(format!(
            "target repository: {} (source: {})",
            naddr,
            resolved.source.label(),
        ))
    );
}

fn get_repo_coordinates_from_git_config(git_repo: &Repo) -> Result<Option<Nip19Coordinate>> {
    git_repo
        .get_git_config_item("nostr.repo", Some(false))?
        .map(|value| {
            Nip19Coordinate::from_bech32(&value)
                .context("git config item \"nostr.repo\" is not an naddr")
        })
        .transpose()
}

async fn get_nostr_remotes(git_repo: &Repo) -> Result<HashMap<String, NostrUrlDecoded>> {
    let mut nostr_remotes = HashMap::new();
    for remote_name in git_repo
        .git_repo
        .remotes()?
        .iter()
        .filter_map(|r| r.ok().flatten())
    {
        if let Ok(remote_url) = git_repo.git_repo.find_remote(remote_name)?.url() {
            if let Ok(nostr_url_decoded) =
                NostrUrlDecoded::parse_and_resolve(remote_url, &Some(git_repo)).await
            {
                nostr_remotes.insert(remote_name.to_string(), nostr_url_decoded);
            }
        }
    }
    Ok(nostr_remotes)
}

async fn find_nostr_remote_for_coordinate(
    git_repo: &Repo,
    coordinate: &Nip19Coordinate,
) -> Result<Option<ResolvedNostrRemote>> {
    Ok(find_matching_nostr_remote(
        &get_nostr_remotes(git_repo).await?,
        coordinate,
    ))
}

pub async fn get_nostr_remote_for_resolved_coordinate(
    git_repo: &Repo,
    resolved: &ResolvedRepoCoordinate,
) -> Result<Option<ResolvedNostrRemote>> {
    if let Some(remote) = &resolved.remote {
        return Ok(Some(remote.clone()));
    }
    find_nostr_remote_for_coordinate(git_repo, &resolved.coordinate).await
}

async fn get_repo_coordinates_from_maintainers_yaml(git_repo: &Repo) -> Result<Nip19Coordinate> {
    let repo_config = get_repo_config_from_yaml(git_repo)?;

    Ok(Nip19Coordinate {
        coordinate: Coordinate {
            identifier: repo_config
                .identifier
                .context("maintainers.yaml doesnt list the identifier")?,
            kind: Kind::GitRepoAnnouncement,
            public_key: PublicKey::from_bech32(
                repo_config
                    .maintainers
                    .first()
                    .context("maintainers.yaml doesnt list any maintainers")?,
            )
            .context("maintainers.yaml doesn't list the first maintainer using a valid npub")?,
        },
        relays: repo_config
            .relays
            .iter()
            .filter_map(|url| RelayUrl::parse(url).ok())
            .collect(),
    })
}

async fn get_repo_coordinate_from_user_prompt(
    git_repo: &Repo,
    #[cfg(test)] client: &mut crate::client::MockConnect,
    #[cfg(not(test))] client: &mut Client,
    private_discovery: &PrivateGitRelayDiscovery,
) -> Result<Nip19Coordinate> {
    // TODO: present list of events filter by root_commit
    // TODO: fallback to search based on identifier
    let dim = Style::new().color256(247);
    eprintln!(
        "{}",
        dim.apply_to(
            "hint: https://gitworkshop.dev/search lists repositories and their nostr address"
        ),
    );
    let git_repo_path = git_repo.get_path()?;
    let coordinate = {
        loop {
            let input = Interactor::default()
                .input(PromptInputParms::default().with_prompt("nostr repository"))?;
            let coordinate = if let Ok(c) = Nip19Coordinate::from_bech32(&input) {
                c
            } else if let Ok(nostr_url) =
                NostrUrlDecoded::parse_and_resolve(&input, &Some(git_repo)).await
            {
                nostr_url.coordinate
            } else {
                eprintln!("not a valid naddr or git nostr remote URL starting nostr://");
                continue;
            };
            let term = console::Term::stderr();
            term.write_line("searching for repository...")?;
            if let PrivateGitRelayDiscovery::Unavailable(error) = private_discovery {
                bail!("private Git relay discovery is unavailable: {error}");
            }
            let mut repository_relays_only = private_discovery.requires_repository_only_probe();
            let report = loop {
                let mut private_coordinate = coordinate.clone();
                private_coordinate.relays = private_discovery.relays().to_vec();
                let fetch_coordinate = if repository_relays_only {
                    &private_coordinate
                } else {
                    &coordinate
                };
                let (relay_reports, progress_reporter) = client
                    .fetch_all(
                        Some(git_repo_path),
                        Some(fetch_coordinate),
                        &HashSet::from_iter(vec![coordinate.public_key]),
                        &HashSet::new(),
                        repository_relays_only,
                    )
                    .await?;
                let outcome = consolidate_fetch_outcome(relay_reports);
                if !outcome.had_errors && !outcome.report.to_string().is_empty() {
                    let _ = progress_reporter.clear();
                }
                if repository_relays_only {
                    let discovered_privacy =
                        get_repo_ref_from_cache(Some(git_repo_path), &coordinate)
                            .await
                            .ok()
                            .map(|repo_ref| repo_ref.private);
                    let private_probe_completed =
                        outcome.all_required_relays_completed(private_discovery.relays().len());
                    match private_relay_probe_decision(discovered_privacy, private_probe_completed)
                    {
                        PrivateRelayProbeDecision::UsePrivateResult => {}
                        PrivateRelayProbeDecision::RetryPublicDiscovery => {
                            repository_relays_only = false;
                            continue;
                        }
                        PrivateRelayProbeDecision::FailClosed => {
                            bail!(
                                "private repository relay probe failed; refusing to query public discovery relays"
                            );
                        }
                    }
                }
                break outcome.report;
            };
            if report.to_string().is_empty() {
                eprintln!("couldn't find repository");
                continue;
            } else {
                eprintln!("repository found");
                break coordinate;
            }
        }
    };
    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &coordinate).await?;

    if Interactor::default().confirm(
        PromptConfirmParms::default()
            .with_default(true)
            .with_prompt("set git remote \"origin\" to nostr repository url?"),
    )? {
        set_or_create_git_remote_with_nostr_url("origin", &repo_ref, git_repo)?;
    } else if Interactor::default().confirm(
        PromptConfirmParms::default()
            .with_default(true)
            .with_prompt("set up new git remote for the nostr repository?"),
    )? {
        let name =
            Interactor::default().input(PromptInputParms::default().with_prompt("remote name"))?;
        set_or_create_git_remote_with_nostr_url(&name, &repo_ref, git_repo)?;
    }
    git_repo.save_git_config_item("nostr.repo", &coordinate.to_bech32()?, false)?;
    Ok(coordinate)
}

fn set_or_create_git_remote_with_nostr_url(
    name: &str,
    repo_ref: &RepoRef,
    git_repo: &Repo,
) -> Result<()> {
    let url = repo_ref.to_nostr_git_url(&Some(git_repo)).to_string();
    if git_repo.git_repo.remote_set_url(name, &url).is_err() {
        git_repo.git_repo.remote(name, &url)?;
    }
    eprintln!("set git remote \"{name}\" to {url}");
    Ok(())
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct RepoConfigYaml {
    pub identifier: Option<String>,
    pub maintainers: Vec<String>,
    pub relays: Vec<String>,
}

pub fn get_repo_config_from_yaml(git_repo: &Repo) -> Result<RepoConfigYaml> {
    let path = git_repo.get_path()?.join("maintainers.yaml");
    let file = File::open(path)
        .context("should open maintainers.yaml if it exists")
        .context("maintainers.yaml doesnt exist")?;
    let reader = BufReader::new(file);
    let repo_config_yaml: RepoConfigYaml = serde_yaml::from_reader(reader)
        .context("should read maintainers.yaml with serde_yaml")
        .context("maintainers.yaml incorrectly formatted")?;
    Ok(repo_config_yaml)
}

pub fn extract_pks(pk_strings: Vec<String>) -> Result<Vec<PublicKey>> {
    let mut pks: Vec<PublicKey> = vec![];
    for s in pk_strings {
        pks.push(PublicKey::from_bech32(&s).context(format!(
            "failed to convert {s} into a valid nostr public key"
        ))?);
    }
    Ok(pks)
}

pub fn save_repo_config_to_yaml(
    git_repo: &Repo,
    identifier: String,
    maintainers: Vec<PublicKey>,
    relays: Vec<String>,
) -> Result<()> {
    let path = git_repo.get_path()?.join("maintainers.yaml");
    let file = if path.exists() {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .context("failed to open maintainers.yaml file with write and truncate options")?
    } else {
        std::fs::File::create(path).context("failed to create maintainers.yaml file")?
    };
    let mut maintainers_npubs = vec![];
    for m in maintainers {
        maintainers_npubs.push(
            m.to_bech32()
                .context("failed to convert public key into npub")?,
        );
    }
    serde_yaml::to_writer(
        file,
        &RepoConfigYaml {
            identifier: Some(identifier),
            maintainers: maintainers_npubs,
            relays,
        },
    )
    .context("failed to write maintainers to maintainers.yaml file serde_yaml")
}

pub fn detect_existing_grasp_servers(
    repo_ref: Option<&RepoRef>,
    args_relays: &[String],
    args_clone_url: &[String],
    identifier: &str,
) -> Vec<String> {
    // Collect clone URLs from arguments or repo_ref
    let clone_urls: Vec<String> = if !args_clone_url.is_empty() {
        args_clone_url.to_vec()
    } else if let Some(repo) = repo_ref {
        repo.git_server.clone()
    } else {
        Vec::new()
    };

    // Collect relays from arguments or repo_ref
    let relays: Vec<RelayUrl> = if !args_relays.is_empty() {
        args_relays
            .iter()
            .filter_map(|r| RelayUrl::parse(r).ok())
            .collect()
    } else if let Some(repo) = repo_ref {
        repo.relays.clone()
    } else {
        Vec::new()
    };

    let mut existing_grasp_servers = Vec::new();
    for url in &clone_urls {
        let Ok(formatted_as_grasp_server_url) = normalize_grasp_server_url(url) else {
            continue;
        };
        if existing_grasp_servers.contains(&formatted_as_grasp_server_url) {
            continue;
        }

        let clone_url_is_grasp_server_format = if let Ok(npub) = extract_npub(url) {
            url.contains(&format!("/{npub}/{}.git", pct_encode(identifier)))
        } else {
            false
        };
        if !clone_url_is_grasp_server_format {
            continue;
        }

        let matches_relay = relays.iter().any(|r| {
            normalize_grasp_server_url(&r.to_string())
                .is_ok_and(|r| r.eq(&formatted_as_grasp_server_url))
        });
        if !matches_relay {
            continue;
        }

        existing_grasp_servers.push(formatted_as_grasp_server_url);
    }
    existing_grasp_servers
}

pub fn normalize_grasp_server_url(url: &str) -> Result<String> {
    // Parse the URL and handle errors
    let mut parsed = Url::parse(url)
        .or_else(|_| Url::parse(&format!("https://{url}")))
        .context(format!("{url} not a valid ngit relay URL"))?;
    if parsed.host_str().is_none() {
        // so sub.domain.org gets identifier as host in "sub.domain.org"
        parsed = Url::parse(&format!("https://{url}"))?;
    }

    // Extract the scheme, host, port, and path
    let scheme = parsed.scheme();
    let host = parsed.host_str().context(format!(
        "{url} not a ngit relay url reference: missing host in URL {parsed}"
    ))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = parsed.path();

    // Normalize the URL based on the scheme and path
    let mut normalized_url = match scheme {
        "ws" | "http" => format!("http://{host}{port}{path}"),
        _ => format!("{host}{port}{path}"),
    };

    // If the normalized URL contains "npub1", remove "npub1" and everything after
    // it
    if let Some(pos) = normalized_url.find("npub1") {
        normalized_url.truncate(pos); // Keep everything before "npub1"
    }
    // Return the normalized URL
    Ok(normalized_url.trim_end_matches('/').to_string())
}

pub fn extract_npub(s: &str) -> Result<&str> {
    // Find the starting index of "npub1"
    if let Some(start) = s.find("npub1") {
        let mut end = start + 5; // Start after "npub1"

        // Move the end index to include valid characters (0-9, a-z)
        while end < s.len() && s[end..=end].chars().all(|c| c.is_ascii_alphanumeric()) {
            end += 1;
        }
        // Extract the npub substring
        let npub = &s[start..end];
        // Attempt to create a PublicKey from the extracted npub
        PublicKey::from_bech32(npub).context("invalid npub")?;
        Ok(npub)
    } else {
        bail!("No npub found")
    }
}

pub fn is_grasp_server_in_list(url: &str, grasp_servers: &[String]) -> bool {
    if !grasp_servers.is_empty() {
        grasp_servers
            .iter()
            .any(|s| s.trim_end_matches('/') == url.trim_end_matches('/'))
    } else {
        false
    }
}

pub fn is_grasp_server_clone_url(url: &str) -> bool {
    // Must start with http:// or https://
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return false;
    }

    // Must end with .git or .git/
    if !url.ends_with(".git") && !url.ends_with(".git/") {
        return false;
    }

    // Must contain a valid npub
    let npub = match extract_npub(url) {
        Ok(npub) => npub,
        Err(_) => return false,
    };

    // Must have format: /{npub}/<repo-name>.git
    // The npub must be followed by a slash and then a non-empty repo name
    let npub_pattern = format!("/{}/", npub);
    if let Some(npub_pos) = url.find(&npub_pattern) {
        // Get the part after /{npub}/
        let after_npub = &url[npub_pos + npub_pattern.len()..];

        // Remove trailing slash if present
        let after_npub = after_npub.trim_end_matches('/');

        // Must have a non-empty repo name that ends with .git
        if after_npub.is_empty() || after_npub == ".git" {
            return false;
        }

        // Repo name must be at least 1 character before .git
        if !after_npub.ends_with(".git") {
            return false;
        }

        let repo_name = &after_npub[..after_npub.len() - 4]; // Remove .git
        !repo_name.is_empty()
    } else {
        false
    }
}

pub fn format_grasp_server_url_as_relay_url(url: &str) -> Result<String> {
    let grasp_server_url = normalize_grasp_server_url(url)?;
    if grasp_server_url.contains("http://") {
        return Ok(grasp_server_url.replace("http://", "ws://"));
    }
    // .onion hosts cannot terminate TLS, default to ws:// so the relay
    // can be reached over Tor.
    if crate::git::nostr_url::host_is_onion(&grasp_server_url) {
        return Ok(format!("ws://{grasp_server_url}"));
    }
    Ok(format!("wss://{grasp_server_url}"))
}

/// Relay URLs paired with the GRASP servers in `git_servers`,
/// deduplicated while preserving order. Non-GRASP entries and URLs
/// whose relay form cannot be derived are skipped.
pub fn grasp_server_relay_urls(git_servers: &[String]) -> Vec<RelayUrl> {
    git_servers
        .iter()
        .filter_map(|git_server_url| {
            if !is_grasp_server_clone_url(git_server_url) {
                return None;
            }
            format_grasp_server_url_as_relay_url(git_server_url)
                .ok()
                .and_then(|relay_url| RelayUrl::parse(&relay_url).ok())
        })
        .fold(Vec::new(), |mut relays, relay| {
            if !relays.iter().any(|existing| existing == &relay) {
                relays.push(relay);
            }
            relays
        })
}

pub fn format_grasp_server_url_as_clone_url(
    grasp_server: &str,
    public_key: &PublicKey,
    identifier: &str,
) -> Result<String> {
    let grasp_server_url = normalize_grasp_server_url(grasp_server)?;

    let prefix = if grasp_server_url.contains("http://") {
        ""
    } else if crate::git::nostr_url::host_is_onion(&grasp_server_url) {
        // .onion hosts cannot terminate TLS, default to http:// so the
        // clone URL can be reached over Tor.
        "http://"
    } else {
        "https://"
    };
    Ok(format!(
        "{prefix}{grasp_server_url}/{}/{}.git",
        public_key.to_bech32()?,
        pct_encode(identifier)
    ))
}

/// GRASP-06 `/prs/<signer-npub>/<percent-encoded-identifier>.git` endpoint URL
/// on `grasp_server`. The signer is the PR event signer (the contributor),
/// not a maintainer — ngit-grasp's policy rejects npub != signer.
///
/// Different from [`format_grasp_server_url_as_clone_url`], which builds the
/// GRASP-01 `/{npub}/{id}.git` repo-announcement endpoint.
///
/// See `/persistent/clones/grasp/06.md` § "Git Smart HTTP Service".
pub fn format_grasp_server_url_as_grasp06_prs_url(
    grasp_server: &str,
    signer: &PublicKey,
    identifier: &str,
) -> Result<String> {
    let grasp_server_url = normalize_grasp_server_url(grasp_server)?;

    let prefix = if grasp_server_url.contains("http://") {
        ""
    } else if crate::git::nostr_url::host_is_onion(&grasp_server_url) {
        // .onion hosts cannot terminate TLS, default to http:// so the
        // clone URL can be reached over Tor.
        "http://"
    } else {
        "https://"
    };
    Ok(format!(
        "{prefix}{grasp_server_url}/prs/{}/{}.git",
        signer.to_bech32()?,
        pct_encode(identifier)
    ))
}

/// Find the latest announcement event (by `created_at`) across all maintainer
/// events and parse it into a `RepoRef` for shared metadata (name, description,
/// web, etc.).
pub fn latest_event_repo_ref(repo_ref: &RepoRef) -> Option<RepoRef> {
    repo_ref
        .events
        .values()
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
        .and_then(|e| RepoRef::try_from((e.clone(), None)).ok())
}

/// Derive clone-URLs and relay URLs from selected grasp servers.
///
/// For each grasp server, adds or replaces the corresponding clone URL in
/// `git_servers` and prepends a relay URL in `relays`. Grasp-derived
/// infrastructure always takes priority — the other lists contain *additional*
/// infrastructure beyond what grasp servers provide.
pub fn apply_grasp_infrastructure(
    grasp_servers: &[String],
    git_servers: &mut Vec<String>,
    relays: &mut Vec<String>,
    public_key: &PublicKey,
    identifier: &str,
) -> Result<()> {
    for (grasp_relay_insert_idx, grasp_server) in grasp_servers.iter().enumerate() {
        // Always add grasp-derived clone URL
        let clone_url = format_grasp_server_url_as_clone_url(grasp_server, public_key, identifier)?;

        let grasp_server_clone_root = if clone_url.contains("https://") {
            format!("https://{grasp_server}")
        } else {
            grasp_server.to_string()
        };

        let matching_positions: Vec<usize> = git_servers
            .iter()
            .enumerate()
            .filter_map(|(idx, url)| {
                if url.contains(&grasp_server_clone_root) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();

        if matching_positions.is_empty() {
            git_servers.push(clone_url);
        } else {
            git_servers[matching_positions[0]] = clone_url;
            for &position in matching_positions.iter().skip(1).rev() {
                git_servers.remove(position);
            }
        }

        // Prepend grasp-derived relay in order (for relay hint) so that the
        // first grasp server in the list ends up at relays[0].
        let relay_url = format_grasp_server_url_as_relay_url(grasp_server)?;
        if !relays.contains(&relay_url) {
            relays.insert(grasp_relay_insert_idx, relay_url);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use once_cell::sync::Lazy;

    use super::*;

    // Two stable nsec values are used so the public-key-ordered
    // `maintainers` tag is deterministic.
    static TEST_KEY_1_NSEC: &str =
        "nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq";
    static TEST_KEY_2_NSEC: &str =
        "nsec1ypglg6nj6ep0g2qmyfqcv2al502gje3jvpwye6mthmkvj93tqkesknv6qm";

    static TEST_KEY_1_KEYS: Lazy<nostr::prelude::Keys> =
        Lazy::new(|| nostr::prelude::Keys::from_str(TEST_KEY_1_NSEC).unwrap());
    static TEST_KEY_2_KEYS: Lazy<nostr::prelude::Keys> =
        Lazy::new(|| nostr::prelude::Keys::from_str(TEST_KEY_2_NSEC).unwrap());

    static TEST_KEY_1_SIGNER: Lazy<Arc<crate::NgitSigner>> = Lazy::new(|| {
        Arc::new(crate::NgitSigner::Keys(
            nostr::prelude::Keys::from_str(TEST_KEY_1_NSEC).unwrap(),
        ))
    });

    async fn create() -> nostr::prelude::Event {
        RepoRef {
            identifier: "123412341".to_string(),
            name: "test name".to_string(),
            description: "test description".to_string(),
            root_commit: "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2".to_string(),
            git_server: vec!["https://localhost:1000".to_string()],
            web: vec![
                "https://exampleproject.xyz".to_string(),
                "https://gitworkshop.dev/123".to_string(),
            ],
            upstream: vec![],
            relays: vec![
                RelayUrl::parse("ws://relay1.io").unwrap(),
                RelayUrl::parse("ws://relay2.io").unwrap(),
            ],
            blossoms: vec![],
            hashtags: vec![],
            private: false,
            selected_maintainer: TEST_KEY_1_KEYS.public_key(),
            maintainers_without_annoucnement: None,
            maintainers: vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: vec![],
        }
        .to_event(&TEST_KEY_1_SIGNER)
        .await
        .unwrap()
    }

    fn create_repo_ref_for_maintainer_order(
        maintainers: Vec<PublicKey>,
        requested: Vec<PublicKey>,
    ) -> RepoRef {
        RepoRef {
            identifier: "123412341".to_string(),
            name: "test name".to_string(),
            description: "test description".to_string(),
            root_commit: "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2".to_string(),
            git_server: vec!["https://localhost:1000".to_string()],
            web: vec![],
            upstream: vec![],
            relays: vec![],
            blossoms: vec![],
            hashtags: vec![],
            private: false,
            selected_maintainer: TEST_KEY_1_KEYS.public_key(),
            maintainers_without_annoucnement: Some(requested),
            maintainers,
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: vec![],
        }
    }

    mod maintainer_order {
        use super::*;

        #[tokio::test]
        async fn announcement_tags_start_with_selected_and_put_invited_last() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let accepted = TEST_KEY_2_KEYS.public_key();
            let requested = PublicKey::from_hex(
                "00000001505e7e48927046e9bbaa728b1f3b511227e2200c578d6e6bb0c77eb9",
            )
            .unwrap();

            let mut repo_ref = create_repo_ref_for_maintainer_order(
                vec![requested, selected, accepted],
                vec![requested],
            );
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, accepted, requested]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_2_KEYS, vec![accepted, selected]).await,
            );

            assert_eq!(
                repo_ref.maintainers_for_announcement_tags(),
                vec![selected, accepted, requested]
            );
        }

        async fn announcement(
            keys: &nostr::prelude::Keys,
            listed: Vec<PublicKey>,
        ) -> nostr::prelude::Event {
            let signer = Arc::new(crate::NgitSigner::Keys(keys.clone()));
            let mut repo_ref = create_repo_ref_for_maintainer_order(listed, vec![]);
            repo_ref.selected_maintainer = keys.public_key();
            repo_ref.to_event(&signer).await.unwrap()
        }

        fn insert_event(repo_ref: &mut RepoRef, event: nostr::prelude::Event) {
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: Kind::GitRepoAnnouncement,
                        public_key: event.pubkey,
                        identifier: repo_ref.identifier.clone(),
                    },
                    relays: vec![],
                },
                event,
            );
        }

        #[tokio::test]
        async fn announcement_does_not_confirm_an_unreciprocated_invitation() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let invited = TEST_KEY_2_KEYS.public_key();
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, invited], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, invited]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_2_KEYS, vec![invited]).await,
            );

            assert_eq!(repo_ref.confirmed_maintainers(), vec![selected]);
            assert_eq!(repo_ref.invited_maintainers(), vec![invited]);
        }

        #[tokio::test]
        async fn reciprocal_relationship_confirms_both_maintainers() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let other = TEST_KEY_2_KEYS.public_key();
            let mut repo_ref = create_repo_ref_for_maintainer_order(vec![selected, other], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, other]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_2_KEYS, vec![other, selected]).await,
            );

            assert_eq!(repo_ref.confirmed_maintainers(), vec![selected, other]);
            assert!(repo_ref.invited_maintainers().is_empty());
        }

        #[tokio::test]
        async fn lead_requires_unique_highest_confirmed_listing_count() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let lead = TEST_KEY_2_KEYS.public_key();
            let third_keys = nostr::prelude::Keys::generate();
            let third = third_keys.public_key();
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, lead, third], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, lead, third]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_2_KEYS, vec![lead, selected]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&third_keys, vec![third, lead]).await,
            );

            assert_eq!(repo_ref.lead_maintainer(), Some(lead));
        }
    }

    mod try_from {
        use nostr::prelude::{Event, EventBuilder, event::FinalizeEvent};

        use super::*;

        async fn create_with_private_tag(values: &[&str]) -> Event {
            let base = create().await;
            let mut tags: Vec<Tag> = base.tags.iter().cloned().collect();
            tags.push(
                Tag::parse(
                    [
                        vec!["private".to_string()],
                        values.iter().map(ToString::to_string).collect(),
                    ]
                    .concat(),
                )
                .unwrap(),
            );
            EventBuilder::new(base.kind, base.content)
                .tags(tags)
                .finalize(&*TEST_KEY_1_KEYS)
                .unwrap()
        }

        #[tokio::test]
        async fn identifier() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .identifier,
                "123412341",
            )
        }

        #[tokio::test]
        async fn name() {
            assert_eq!(
                RepoRef::try_from((create().await, None)).unwrap().name,
                "test name",
            )
        }

        #[tokio::test]
        async fn description() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .description,
                "test description",
            )
        }

        #[tokio::test]
        async fn root_commit_is_r_tag() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .root_commit,
                "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2",
            )
        }

        mod root_commit_is_empty_if_no_r_tag_which_is_sha1_format {
            use super::*;
            async fn create_with_incorrect_first_commit_ref(s: &str) -> nostr::prelude::Event {
                nostr::prelude::Event::from_json(
                    create()
                        .await
                        .as_json()
                        .replace("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2", s),
                )
                .unwrap()
            }

            #[tokio::test]
            async fn less_than_40_characters() {
                let s = "5e664e5a7845cd1373";
                assert_eq!(
                    RepoRef::try_from((create_with_incorrect_first_commit_ref(s).await, None))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[tokio::test]
            async fn more_than_40_characters() {
                let s = "5e664e5a7845cd1373c79f580ca4fe29ab5b34d2111111111";
                assert_eq!(
                    RepoRef::try_from((create_with_incorrect_first_commit_ref(s).await, None))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }

            #[tokio::test]
            async fn not_hex_characters() {
                let s = "xxx64e5a7845cd1373c79f580ca4fe29ab5b34d2";
                assert_eq!(
                    RepoRef::try_from((create_with_incorrect_first_commit_ref(s).await, None))
                        .unwrap()
                        .root_commit,
                    "",
                )
            }
        }

        #[tokio::test]
        async fn git_server() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .git_server,
                vec!["https://localhost:1000"],
            )
        }

        #[tokio::test]
        async fn web() {
            assert_eq!(
                RepoRef::try_from((create().await, None)).unwrap().web,
                vec![
                    "https://exampleproject.xyz".to_string(),
                    "https://gitworkshop.dev/123".to_string()
                ],
            )
        }

        #[tokio::test]
        async fn upstream() {
            let base = create().await;
            let mut tags: Vec<Tag> = base.tags.iter().cloned().collect();
            tags.push(
                Tag::parse([
                    "u",
                    "30617:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:upstream",
                    "wss://relay.example",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                ])
                .unwrap(),
            );
            let event = nostr::prelude::EventBuilder::new(base.kind, base.content)
                .tags(tags)
                .finalize(&*TEST_KEY_1_KEYS)
                .unwrap();

            assert_eq!(
                RepoRef::try_from((event, None)).unwrap().upstream,
                vec![vec![
                    "30617:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:upstream"
                        .to_string(),
                    "wss://relay.example".to_string(),
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_string(),
                ]],
            )
        }

        #[tokio::test]
        async fn relays() {
            assert_eq!(
                RepoRef::try_from((create().await, None)).unwrap().relays,
                vec![
                    RelayUrl::parse("ws://relay1.io").unwrap(),
                    RelayUrl::parse("ws://relay2.io").unwrap(),
                ],
            )
        }

        #[tokio::test]
        async fn maintainers() {
            assert_eq!(
                RepoRef::try_from((create().await, None))
                    .unwrap()
                    .maintainers,
                vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
            )
        }

        #[tokio::test]
        async fn private_requires_exact_true_tag() {
            assert!(
                RepoRef::try_from((create_with_private_tag(&["true"]).await, None))
                    .unwrap()
                    .private
            );

            for values in [&["false"][..], &["TRUE"][..], &["true", "extra"][..]] {
                let parsed =
                    RepoRef::try_from((create_with_private_tag(values).await, None)).unwrap();
                assert!(!parsed.private);
                assert!(
                    parsed.extra_tags.is_empty(),
                    "malformed known private tag must not leak into extra_tags"
                );
            }
        }
    }

    mod to_event {
        use super::*;
        mod tags {
            use super::*;

            #[tokio::test]
            async fn identifier() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("d") && t.as_slice()[1].eq("123412341"))
                )
            }

            #[tokio::test]
            async fn name() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("name") && t.as_slice()[1].eq("test name"))
                )
            }

            #[tokio::test]
            async fn alt() {
                assert!(create().await.tags.iter().any(|t| t.as_slice()[0].eq("alt")
                    && t.as_slice()[1].eq("git repository: test name")))
            }

            #[tokio::test]
            async fn description() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("description")
                            && t.as_slice()[1].eq("test description"))
                )
            }

            #[tokio::test]
            async fn root_commit_as_reference() {
                assert!(create().await.tags.iter().any(|t| t.as_slice()[0].eq("r")
                    && t.as_slice()[1].eq("5e664e5a7845cd1373c79f580ca4fe29ab5b34d2")))
            }

            #[tokio::test]
            async fn git_server() {
                assert!(
                    create()
                        .await
                        .tags
                        .iter()
                        .any(|t| t.as_slice()[0].eq("clone")
                            && t.as_slice()[1].eq("https://localhost:1000"))
                )
            }

            #[tokio::test]
            async fn relays() {
                let event = create().await;
                let relays_tag: &nostr::prelude::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0].eq("relays"))
                    .unwrap();
                assert_eq!(relays_tag.as_slice().len(), 3);
                assert_eq!(relays_tag.as_slice()[1], "ws://relay1.io");
                assert_eq!(relays_tag.as_slice()[2], "ws://relay2.io");
            }

            #[tokio::test]
            async fn web() {
                let event = create().await;
                let web_tag: &nostr::prelude::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0].eq("web"))
                    .unwrap();
                assert_eq!(web_tag.as_slice().len(), 3);
                assert_eq!(web_tag.as_slice()[1], "https://exampleproject.xyz");
                assert_eq!(web_tag.as_slice()[2], "https://gitworkshop.dev/123");
            }

            #[tokio::test]
            async fn upstream() {
                let mut repo_ref = RepoRef::try_from((create().await, None)).unwrap();
                repo_ref.upstream = vec![vec![
                    "https://example.com/upstream.git".to_string(),
                    "wss://relay.example".to_string(),
                    TEST_KEY_2_KEYS.public_key().to_string(),
                ]];

                let event = repo_ref.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
                let upstream_tag: &nostr::prelude::Tag =
                    event.tags.iter().find(|t| t.as_slice()[0].eq("u")).unwrap();
                assert_eq!(upstream_tag.as_slice().len(), 4);
                assert_eq!(
                    upstream_tag.as_slice()[1],
                    "https://example.com/upstream.git"
                );
                assert_eq!(upstream_tag.as_slice()[2], "wss://relay.example");
                assert_eq!(
                    upstream_tag.as_slice()[3],
                    TEST_KEY_2_KEYS.public_key().to_string()
                );
            }

            #[tokio::test]
            async fn upstream_is_not_emitted_by_default() {
                assert!(!create().await.tags.iter().any(|t| t.as_slice()[0].eq("u")))
            }

            #[tokio::test]
            async fn maintainers() {
                let event = create().await;
                let maintainers_tag: &nostr::prelude::Tag = event
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0].eq("maintainers"))
                    .unwrap();
                assert_eq!(maintainers_tag.as_slice().len(), 3);
                assert_eq!(
                    maintainers_tag.as_slice()[1],
                    TEST_KEY_1_KEYS.public_key().to_string()
                );
                assert_eq!(
                    maintainers_tag.as_slice()[2],
                    TEST_KEY_2_KEYS.public_key().to_string()
                );
            }

            #[tokio::test]
            async fn private_is_emitted_only_when_enabled() {
                let mut repo_ref = RepoRef::try_from((create().await, None)).unwrap();
                assert!(
                    !repo_ref
                        .to_event(&TEST_KEY_1_SIGNER)
                        .await
                        .unwrap()
                        .tags
                        .iter()
                        .any(|tag| tag.as_slice().first().is_some_and(|name| name == "private"))
                );

                repo_ref.private = true;
                let event = repo_ref.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
                let private_tags: Vec<&[String]> = event
                    .tags
                    .iter()
                    .map(Tag::as_slice)
                    .filter(|tag| tag.first().is_some_and(|name| name == "private"))
                    .collect();
                assert_eq!(
                    private_tags,
                    vec![&["private".to_string(), "true".to_string()][..]]
                );
            }

            #[tokio::test]
            async fn no_other_tags() {
                assert_eq!(create().await.tags.len(), 9)
            }
        }
    }

    /// Round-trip behaviour of [`RepoRef::extra_tags`]: unknown tags on the
    /// source event survive `try_from` → `to_event`, known-name duplicates
    /// do not, and the [`is_known_tag_name`] allowlist matches what
    /// [`RepoRef::to_event`] actually emits.
    ///
    /// CLI-level behaviour (`--clean` flag, yellow warning, inheritance from
    /// the latest event across maintainers) is tested separately in
    /// `tests/init_preserves_unknown_tags.rs`. These tests pin only the
    /// library-level invariant the CLI relies on.
    mod extra_tags_round_trip {
        use nostr::prelude::{EventBuilder, event::FinalizeEvent};

        use super::*;

        /// Build the canonical fixture event from [`create`], then re-sign a
        /// copy with `extra` appended after its existing tags. Uses
        /// [`EventBuilder`] (not [`nostr::prelude::Event::from_json`] string
        /// surgery) so the new tags land on a valid signed event the
        /// same shape ngit itself produces.
        async fn create_with_extra_tags(extra: Vec<Tag>) -> nostr::prelude::Event {
            let base = create().await;
            let mut tags: Vec<Tag> = base.tags.iter().cloned().collect();
            tags.extend(extra);
            EventBuilder::new(base.kind, base.content)
                .tags(tags)
                .finalize(&*TEST_KEY_1_KEYS)
                .unwrap()
        }

        /// `is_known_tag_name` returns true for every tag name
        /// [`RepoRef::to_event`] emits. If a future change adds a new
        /// typed tag without updating the allowlist, that tag would end up
        /// in `extra_tags` on round-trip and get emitted twice — once from
        /// the typed field, once from `extra_tags`. This test catches that
        /// drift by parsing the canonical fixture event and asserting no
        /// emitted tag name leaks into `extra_tags`.
        #[tokio::test]
        async fn allowlist_matches_emitted_tag_names() {
            let parsed = RepoRef::try_from((create().await, None)).unwrap();
            let leaked: Vec<&str> = parsed
                .extra_tags
                .iter()
                .filter_map(|t| t.as_slice().first().map(String::as_str))
                .collect();
            assert!(
                leaked.is_empty(),
                "tag name(s) {leaked:?} leaked into extra_tags from the \
                 canonical fixture — every tag name `to_event` emits must \
                 be in `is_known_tag_name`",
            );
        }

        /// A single-value unknown tag (`["example", "value"]`) survives
        /// parse → re-emit verbatim.
        #[tokio::test]
        async fn preserves_single_value_unknown_tag() {
            let extras = vec![Tag::parse(["example", "value"]).unwrap()];
            let event = create_with_extra_tags(extras).await;
            let parsed = RepoRef::try_from((event, None)).unwrap();
            let re_emitted = parsed.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
            let matching: Vec<&[String]> = re_emitted
                .tags
                .iter()
                .map(nostr::prelude::Tag::as_slice)
                .filter(|s| s.first().map(String::as_str) == Some("example"))
                .collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(
                matching[0],
                &["example".to_string(), "value".to_string()][..],
            );
        }

        /// A multi-value unknown tag (`["multi", "v1", "v2"]`) survives as
        /// one tag with both values, not split or truncated.
        #[tokio::test]
        async fn preserves_multi_value_unknown_tag() {
            let extras = vec![Tag::parse(["multi", "v1", "v2"]).unwrap()];
            let event = create_with_extra_tags(extras).await;
            let parsed = RepoRef::try_from((event, None)).unwrap();
            let re_emitted = parsed.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
            let matching: Vec<&[String]> = re_emitted
                .tags
                .iter()
                .map(nostr::prelude::Tag::as_slice)
                .filter(|s| s.first().map(String::as_str) == Some("multi"))
                .collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(
                matching[0],
                &["multi".to_string(), "v1".to_string(), "v2".to_string()][..],
            );
        }

        #[tokio::test]
        async fn buzz_channel_acl_requires_private_transport_and_round_trips() {
            let buzz_channel =
                Tag::parse(["buzz-channel", "11111111-1111-4111-8111-111111111111"]).unwrap();
            let event = create_with_extra_tags(vec![buzz_channel.clone()]).await;
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert!(parsed.private);
            assert!(parsed.extra_tags.contains(&buzz_channel));

            let re_emitted = parsed.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
            assert_eq!(
                re_emitted
                    .tags
                    .iter()
                    .filter(|tag| {
                        tag.as_slice().first().map(String::as_str) == Some("buzz-channel")
                    })
                    .count(),
                1,
            );
        }

        /// Two separate tags with the *same* unknown name survive as two
        /// distinct tags. Required by any schema that uses repeated tags
        /// of the same name (NIP-style `t`/`r`/etc. shape).
        #[tokio::test]
        async fn preserves_repeated_unknown_tag_name() {
            let extras = vec![
                Tag::parse(["repeat", "v1"]).unwrap(),
                Tag::parse(["repeat", "v2"]).unwrap(),
            ];
            let event = create_with_extra_tags(extras).await;
            let parsed = RepoRef::try_from((event, None)).unwrap();
            let re_emitted = parsed.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
            let matching: Vec<&[String]> = re_emitted
                .tags
                .iter()
                .map(nostr::prelude::Tag::as_slice)
                .filter(|s| s.first().map(String::as_str) == Some("repeat"))
                .collect();
            assert_eq!(matching.len(), 2);
            let values: Vec<&str> = matching
                .iter()
                .filter_map(|t| t.get(1).map(String::as_str))
                .collect();
            assert!(values.contains(&"v1"));
            assert!(values.contains(&"v2"));
        }

        /// A duplicate of a *known* tag name on the source event must not
        /// leak into `extra_tags` — `to_event` would otherwise emit two
        /// `name` tags (one from the typed field, one from extras). The
        /// typed field is the single source of truth for known names.
        #[tokio::test]
        async fn drops_duplicate_known_name_tag_from_extras() {
            let extras = vec![Tag::parse(["name", "smuggled"]).unwrap()];
            let event = create_with_extra_tags(extras).await;
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert!(
                parsed.extra_tags.is_empty(),
                "duplicate `name` tag leaked into extra_tags: {:?}",
                parsed.extra_tags,
            );
            let re_emitted = parsed.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
            let name_tags: Vec<&[String]> = re_emitted
                .tags
                .iter()
                .map(nostr::prelude::Tag::as_slice)
                .filter(|s| s.first().map(String::as_str) == Some("name"))
                .collect();
            assert_eq!(
                name_tags.len(),
                1,
                "expected exactly one `name` tag after round-trip; got {name_tags:?}",
            );
        }
    }

    #[test]
    fn normalize_grasp_server_url_all_checks() -> Result<()> {
        let test_cases = vec![
            ("https://sub.domain.org", "sub.domain.org"),
            ("wss://sub.domain.org", "sub.domain.org"),
            ("sub.domain.org", "sub.domain.org"),
            ("http://sub.domain.org", "http://sub.domain.org"),
            ("ws://sub.domain.org", "http://sub.domain.org"),
            ("http://localhost", "http://localhost"),
            ("localhost", "localhost"),
            ("https://sub.domain.org:8080", "sub.domain.org:8080"),
            ("http://sub.domain.org:8080", "http://sub.domain.org:8080"),
            ("sub.domain.org:8080", "sub.domain.org:8080"),
            ("https://sub.domain.org/path/to", "sub.domain.org/path/to"),
            (
                "https://sub.domain.org:8080/path/to",
                "sub.domain.org:8080/path/to",
            ),
            (
                "https://sub.domain.org/npub143675782648/to.git",
                "sub.domain.org",
            ),
            (
                "https://sub.domain.org/path/npub143675782648/to.git",
                "sub.domain.org/path",
            ),
            ("https://sub.domain.org/", "sub.domain.org"),
            ("http://sub.domain.org/", "http://sub.domain.org"),
        ];

        for (input, expected) in test_cases {
            let normalized = normalize_grasp_server_url(input)?;
            assert_eq!(normalized, expected);
        }
        Ok(())
    }

    mod is_grasp_server_in_list {
        use super::*;

        #[test]
        fn detects_in_list() {
            assert!(is_grasp_server_in_list(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/example-repo.git",
                &[
                    "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/example-repo.git".to_string(),
                    "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/example-repo2.git".to_string(),
                ],
            ))
        }

        #[test]
        fn ignores_not_in_list() {
            assert!(!is_grasp_server_in_list(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/example-repo3.git",
                &[
                    "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/example-repo.git".to_string(),
                    "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/example-repo2.git".to_string(),
                ],
            ))
        }
    }

    mod grasp_server_relay_urls {
        use super::*;

        const NPUB: &str = "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr";

        #[test]
        fn derives_relay_scheme_from_clone_url_scheme() {
            let git_servers = vec![
                format!("https://relay.ngit.dev/{NPUB}/my-repo.git"),
                format!("http://localhost:8080/{NPUB}/my-repo.git"),
            ];

            assert_eq!(
                grasp_server_relay_urls(&git_servers),
                vec![
                    RelayUrl::parse("wss://relay.ngit.dev").unwrap(),
                    RelayUrl::parse("ws://localhost:8080").unwrap(),
                ]
            );
        }

        #[test]
        fn skips_non_grasp_git_servers() {
            let git_servers = vec![
                "https://github.com/user/my-repo.git".to_string(),
                format!("https://relay.ngit.dev/{NPUB}/my-repo.git"),
            ];

            assert_eq!(
                grasp_server_relay_urls(&git_servers),
                vec![RelayUrl::parse("wss://relay.ngit.dev").unwrap()]
            );
        }

        #[test]
        fn dedupes_servers_sharing_a_relay_preserving_order() {
            let git_servers = vec![
                format!("https://b.example/{NPUB}/my-repo.git"),
                format!("https://a.example/{NPUB}/my-repo.git"),
                format!("https://b.example/{NPUB}/other-repo.git"),
            ];

            assert_eq!(
                grasp_server_relay_urls(&git_servers),
                vec![
                    RelayUrl::parse("wss://b.example").unwrap(),
                    RelayUrl::parse("wss://a.example").unwrap(),
                ]
            );
        }

        #[test]
        fn empty_input_yields_no_relays() {
            assert!(grasp_server_relay_urls(&[]).is_empty());
        }
    }

    mod is_grasp_server_clone_url {
        use super::*;

        #[test]
        fn valid_https_url() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn valid_http_url() {
            assert!(is_grasp_server_clone_url(
                "http://localhost:8080/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/test-repo.git"
            ));
        }

        #[test]
        fn valid_with_trailing_slash() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git/"
            ));
        }

        #[test]
        fn valid_with_nested_path() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/path/to/server/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn valid_with_port() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev:8080/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn invalid_missing_git_extension() {
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo"
            ));
        }

        #[test]
        fn invalid_no_npub() {
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/my-repo.git"
            ));
        }

        #[test]
        fn invalid_npub_not_in_path() {
            // npub exists but not in the path structure (e.g., in query string or fragment)
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/my-repo.git?npub=npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr"
            ));
        }

        #[test]
        fn invalid_wrong_protocol() {
            assert!(!is_grasp_server_clone_url(
                "ftp://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn invalid_no_protocol() {
            assert!(!is_grasp_server_clone_url(
                "relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn invalid_wss_protocol() {
            assert!(!is_grasp_server_clone_url(
                "wss://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn invalid_npub_not_followed_by_slash() {
            // npub must be followed by a slash before the repo name
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejrmy-repo.git"
            ));
        }

        #[test]
        fn invalid_no_repo_name_after_npub() {
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/.git"
            ));
        }

        #[test]
        fn invalid_empty_repo_name() {
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr.git"
            ));
        }

        #[test]
        fn invalid_malformed_npub() {
            assert!(!is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub123invalid/my-repo.git"
            ));
        }

        #[test]
        fn valid_repo_name_with_hyphens() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-awesome-repo.git"
            ));
        }

        #[test]
        fn valid_repo_name_with_underscores() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my_repo.git"
            ));
        }

        #[test]
        fn valid_repo_name_with_numbers() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/repo123.git"
            ));
        }

        // GRASP-06 /prs/{npub}/{id}.git form

        #[test]
        fn valid_grasp06_prs_http_url() {
            // /prs/<npub>/<id>.git should be accepted — uses same HTTP push path
            assert!(is_grasp_server_clone_url(
                "http://localhost:8080/prs/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }

        #[test]
        fn valid_grasp06_prs_https_url() {
            assert!(is_grasp_server_clone_url(
                "https://relay.ngit.dev/prs/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/my-repo.git"
            ));
        }
    }

    mod format_grasp_server_url_as_grasp06_prs_url {
        use nostr::key::Keys;

        use super::*;

        fn test_pk() -> PublicKey {
            Keys::parse("nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq")
                .unwrap()
                .public_key()
        }

        #[test]
        fn ws_scheme_maps_to_http() {
            // ws:// grasp servers normalize to http://
            let url = format_grasp_server_url_as_grasp06_prs_url(
                "ws://127.0.0.1:8080",
                &test_pk(),
                "my-repo",
            )
            .unwrap();
            let npub = test_pk().to_bech32().unwrap();
            assert_eq!(url, format!("http://127.0.0.1:8080/prs/{npub}/my-repo.git"));
        }

        #[test]
        fn bare_host_maps_to_https() {
            // bare host (no scheme) → https://
            let url =
                format_grasp_server_url_as_grasp06_prs_url("relay.ngit.dev", &test_pk(), "my-repo")
                    .unwrap();
            let npub = test_pk().to_bech32().unwrap();
            assert_eq!(
                url,
                format!("https://relay.ngit.dev/prs/{npub}/my-repo.git")
            );
        }

        #[test]
        fn identifier_is_pct_encoded() {
            // spaces and special chars in identifier must be percent-encoded
            let url =
                format_grasp_server_url_as_grasp06_prs_url("relay.ngit.dev", &test_pk(), "my repo")
                    .unwrap();
            let npub = test_pk().to_bech32().unwrap();
            assert_eq!(
                url,
                format!("https://relay.ngit.dev/prs/{npub}/my%20repo.git")
            );
        }

        #[test]
        fn onion_host_maps_to_http() {
            // bare .onion host → http:// (Tor doesn't terminate TLS).
            let url = format_grasp_server_url_as_grasp06_prs_url(
                "nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion",
                &test_pk(),
                "my-repo",
            )
            .unwrap();
            let npub = test_pk().to_bech32().unwrap();
            assert_eq!(
                url,
                format!(
                    "http://nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion/prs/{npub}/my-repo.git"
                )
            );
        }
    }

    mod format_grasp_server_url_as_relay_url {
        use super::*;

        #[test]
        fn bare_host_maps_to_wss() {
            assert_eq!(
                format_grasp_server_url_as_relay_url("relay.ngit.dev").unwrap(),
                "wss://relay.ngit.dev".to_string(),
            );
        }

        #[test]
        fn http_scheme_maps_to_ws() {
            assert_eq!(
                format_grasp_server_url_as_relay_url("http://127.0.0.1:8080").unwrap(),
                "ws://127.0.0.1:8080".to_string(),
            );
        }

        #[test]
        fn onion_host_maps_to_ws() {
            // .onion hosts can't terminate TLS — must default to ws://.
            assert_eq!(
                format_grasp_server_url_as_relay_url(
                    "nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion"
                )
                .unwrap(),
                "ws://nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion".to_string(),
            );
        }
    }

    mod format_grasp_server_url_as_clone_url {
        use nostr::key::Keys;

        use super::*;

        fn test_pk() -> PublicKey {
            Keys::parse("nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq")
                .unwrap()
                .public_key()
        }

        #[test]
        fn onion_host_maps_to_http() {
            let url = format_grasp_server_url_as_clone_url(
                "nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion",
                &test_pk(),
                "my-repo",
            )
            .unwrap();
            let npub = test_pk().to_bech32().unwrap();
            assert_eq!(
                url,
                format!(
                    "http://nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion/{npub}/my-repo.git"
                )
            );
        }
    }

    /// Unit tests for the repository-coordinate resolution priority policy.
    ///
    /// Documented priority under test:
    ///   1. `--repo` — remote name → naddr → nostr:// URL
    ///   2. `nostr.repo` git config
    ///   3. tracked upstream of current branch (nostr:// remote)
    ///   4. `origin` (nostr:// remote)
    ///   5. sole distinct nostr:// remote coordinate
    ///
    /// Ambiguous → error unless `-i` was passed (interactive).
    mod resolve_repo_coordinate_priority {
        use super::*;
        use crate::git::{Repo, RepoActions, test_helpers::GitTestRepo};

        // Distinct pubkeys for the different "maintainers" in each test
        // scenario. These are the actual bytes from the recently-reported
        // ngit repo-coordinate ambiguity incident:
        //   * Dan Conway (canonical ngit) —
        //     a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d
        //   * A second maintainer (fake canonical / co-maintainer / etc.)
        //     43185edecc31be95d78f2b5b7b8974bfc0fddfe6836d67ff09e2c5c78116b4f0
        //
        // The exact bytes don't matter for these tests; only that they are
        // distinct valid secp256k1 x-only public keys.
        const PUBKEY_A_HEX: &str =
            "a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d";
        const PUBKEY_B_HEX: &str =
            "43185edecc31be95d78f2b5b7b8974bfc0fddfe6836d67ff09e2c5c78116b4f0";

        fn pk(hex: &str) -> PublicKey {
            PublicKey::from_hex(hex).unwrap()
        }

        fn naddr(pubkey_hex: &str, identifier: &str) -> String {
            Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::GitRepoAnnouncement,
                    public_key: pk(pubkey_hex),
                    identifier: identifier.to_string(),
                },
                relays: vec![],
            }
            .to_bech32()
            .unwrap()
        }

        fn nostr_url(pubkey_hex: &str, identifier: &str) -> String {
            // The npub form is what NostrUrlDecoded::parse_and_resolve
            // accepts without any network calls.
            let npub = pk(pubkey_hex).to_bech32().unwrap();
            format!("nostr://{npub}/{identifier}")
        }

        /// Build a `Repo` around a `GitTestRepo`. The default `GitTestRepo`
        /// pre-seeds `nostr.repo` — we clear it here so tests that need to
        /// exercise other priority tiers see the expected "empty" state,
        /// and re-set it in tests that specifically exercise tier 2.
        fn setup_repo() -> (GitTestRepo, Repo) {
            let test_repo = GitTestRepo::default();
            let _ = test_repo.git_repo.config().unwrap().remove("nostr.repo");
            let repo = Repo::from_path(&test_repo.dir).unwrap();
            (test_repo, repo)
        }

        async fn resolve(
            repo: &Repo,
            repo_override: Option<String>,
        ) -> Result<ResolvedRepoCoordinate> {
            resolve_repo_coordinate_with_options(
                repo,
                &RepoCoordinateResolutionOptions {
                    repo_override,
                    interactive: false,
                },
            )
            .await
        }

        // ---- Tier 1: --repo override --------------------------------------

        #[tokio::test]
        async fn tier1_repo_arg_matches_remote_name() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            let resolved = resolve(&repo, Some("upstream".to_string())).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(
                resolved.source,
                RepoCoordinateSource::RepoArgRemoteName("upstream".to_string())
            );
            assert_eq!(
                resolved.remote.as_ref().map(|remote| remote.name.as_str()),
                Some("upstream")
            );
        }

        #[tokio::test]
        async fn tier1_repo_arg_preserves_selected_alias_for_downstream_git_operations() {
            let (test_repo, repo) = setup_repo();
            let shared_coordinate = nostr_url(PUBKEY_A_HEX, "my-repo");
            test_repo.add_remote("origin", &shared_coordinate).unwrap();
            test_repo
                .add_remote("upstream", &shared_coordinate)
                .unwrap();

            let resolved = resolve(&repo, Some("upstream".to_string())).await.unwrap();
            assert_eq!(
                resolved.remote.as_ref().map(|remote| remote.name.as_str()),
                Some("upstream"),
                "an explicit remote must not be replaced by another alias of the same coordinate"
            );
        }

        #[tokio::test]
        async fn tier1_repo_arg_parses_as_naddr() {
            let (test_repo, repo) = setup_repo();
            // Add a conflicting remote — it must be ignored because --repo
            // took the naddr path.
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            let resolved = resolve(&repo, Some(naddr(PUBKEY_A_HEX, "my-repo")))
                .await
                .unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(resolved.source, RepoCoordinateSource::RepoArgNaddr);
            assert!(
                resolved.remote.is_none(),
                "a coordinate-only override must not borrow a disagreeing remote"
            );
        }

        #[tokio::test]
        async fn tier1_repo_arg_parses_as_nostr_url() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            let resolved = resolve(&repo, Some(nostr_url(PUBKEY_A_HEX, "my-repo")))
                .await
                .unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(resolved.source, RepoCoordinateSource::RepoArgNostrUrl);
        }

        #[tokio::test]
        async fn tier1_repo_arg_invalid_returns_error() {
            let (_test_repo, repo) = setup_repo();

            let err = resolve(&repo, Some("not-a-remote-nor-naddr-nor-url".to_string()))
                .await
                .err()
                .unwrap();
            let msg = format!("{err}");
            assert!(
                msg.contains("--repo value")
                    && msg.contains("neither a valid naddr nor a nostr:// URL"),
                "unexpected error: {msg}"
            );
        }

        #[tokio::test]
        async fn tier1_invalid_repo_arg_still_errors_in_interactive_mode() {
            let (_test_repo, repo) = setup_repo();
            let err = resolve_repo_coordinate_with_options(
                &repo,
                &RepoCoordinateResolutionOptions {
                    repo_override: Some("not-a-remote-nor-naddr-nor-url".to_string()),
                    interactive: true,
                },
            )
            .await
            .unwrap_err();

            assert!(
                err.to_string().contains("--repo value"),
                "unexpected error: {err}"
            );
        }

        // ---- Tier 2: nostr.repo config ------------------------------------

        #[tokio::test]
        async fn tier2_nostr_repo_config_wins_over_remotes() {
            let (test_repo, repo) = setup_repo();
            // Configure two disagreeing nostr:// remotes so that without
            // `nostr.repo` the resolver would either be ambiguous or pick
            // a remote.
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();
            // Explicit config → PUBKEY_A_HEX (upstream's coordinate).
            repo.save_git_config_item("nostr.repo", &naddr(PUBKEY_A_HEX, "my-repo"), false)
                .unwrap();

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(resolved.source, RepoCoordinateSource::NostrRepoConfig);
            let remote = get_nostr_remote_for_resolved_coordinate(&repo, &resolved)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                remote.name, "upstream",
                "downstream Git operations must use the remote matching nostr.repo"
            );
        }

        #[tokio::test]
        async fn tier2_invalid_nostr_repo_config_does_not_fall_through() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();
            repo.save_git_config_item("nostr.repo", "not-an-naddr", false)
                .unwrap();

            let err = resolve(&repo, None).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("git config item \"nostr.repo\" is not an naddr"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn tier2_config_without_matching_remote_does_not_borrow_origin() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            repo.save_git_config_item("nostr.repo", &naddr(PUBKEY_A_HEX, "my-repo"), false)
                .unwrap();

            let resolved = resolve(&repo, None).await.unwrap();
            assert!(
                get_nostr_remote_for_resolved_coordinate(&repo, &resolved)
                    .await
                    .unwrap()
                    .is_none(),
                "downstream Git operations must not borrow a remote for another coordinate"
            );
        }

        // ---- Tier 3: tracked upstream remote ------------------------------

        #[tokio::test]
        async fn tier3_tracked_upstream_remote_wins_over_origin() {
            let (test_repo, repo) = setup_repo();
            // Two disagreeing nostr remotes: origin and upstream.
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();

            // Populate & make current branch track `upstream/main`.
            test_repo.populate().unwrap();
            // Create a matching upstream/main remote-tracking ref by copying
            // the local main ref.
            let head_oid = test_repo
                .git_repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id();
            test_repo
                .git_repo
                .reference(
                    "refs/remotes/upstream/main",
                    head_oid,
                    true,
                    "seed upstream/main for test",
                )
                .unwrap();
            // Set the local `main` branch to track `upstream/main`.
            {
                let mut branch = test_repo
                    .git_repo
                    .find_branch("main", git2::BranchType::Local)
                    .unwrap();
                branch.set_upstream(Some("upstream/main")).unwrap();
            }

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(
                resolved.source,
                RepoCoordinateSource::TrackedUpstreamRemote("upstream".to_string())
            );
        }

        #[tokio::test]
        async fn tier3_configured_remote_wins_without_remote_tracking_ref() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();

            test_repo.populate().unwrap();
            let mut config = test_repo.git_repo.config().unwrap();
            config.set_str("branch.main.remote", "upstream").unwrap();
            config
                .set_str("branch.main.merge", "refs/heads/main")
                .unwrap();
            assert!(
                test_repo
                    .git_repo
                    .find_reference("refs/remotes/upstream/main")
                    .is_err(),
                "regression setup must not create the remote-tracking ref"
            );

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(
                resolved.source,
                RepoCoordinateSource::TrackedUpstreamRemote("upstream".to_string())
            );
        }

        #[tokio::test]
        async fn tier3_tracked_upstream_remote_name_with_slash_wins_over_origin() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("team/upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();

            test_repo.populate().unwrap();
            let head_oid = test_repo
                .git_repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id();
            test_repo
                .git_repo
                .reference(
                    "refs/remotes/team/upstream/main",
                    head_oid,
                    true,
                    "seed team/upstream/main for test",
                )
                .unwrap();
            let mut branch = test_repo
                .git_repo
                .find_branch("main", git2::BranchType::Local)
                .unwrap();
            branch.set_upstream(Some("team/upstream/main")).unwrap();

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(
                resolved.source,
                RepoCoordinateSource::TrackedUpstreamRemote("team/upstream".to_string())
            );
        }

        // ---- Tier 4: origin remote ----------------------------------------

        #[tokio::test]
        async fn tier4_origin_wins_when_no_tracked_upstream() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("origin", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(resolved.source, RepoCoordinateSource::OriginRemote);
        }

        // ---- Tier 5: sole remaining distinct coordinate -------------------

        #[tokio::test]
        async fn tier5_single_remaining_remote_wins() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            assert_eq!(
                resolved.source,
                RepoCoordinateSource::SingleRemainingRemote("upstream".to_string())
            );
        }

        #[tokio::test]
        async fn tier5_multiple_remotes_same_coordinate_collapses_to_single() {
            let (test_repo, repo) = setup_repo();
            // Two remotes, SAME coordinate — must resolve unambiguously.
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("mirror", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();

            let resolved = resolve(&repo, None).await.unwrap();
            assert_eq!(resolved.coordinate.public_key, pk(PUBKEY_A_HEX));
            // Deterministic representative: lex-first remote name ("mirror").
            assert_eq!(
                resolved.source,
                RepoCoordinateSource::SingleRemainingRemote("mirror".to_string())
            );
        }

        // ---- Ambiguity: multiple distinct coordinates, no --repo, no -i ---

        #[tokio::test]
        async fn ambiguous_multiple_remotes_no_origin_errors_by_default() {
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("upstream", &nostr_url(PUBKEY_A_HEX, "my-repo"))
                .unwrap();
            test_repo
                .add_remote("co", &nostr_url(PUBKEY_B_HEX, "my-repo"))
                .unwrap();

            let err = resolve(&repo, None).await.err().unwrap();
            let msg = format!("{err}");
            assert!(
                msg.contains("multiple nostr:// git remotes disagree")
                    && msg.contains("--repo")
                    && msg.contains("nostr.repo"),
                "unexpected error:\n{msg}"
            );
        }

        // ---- Regression: the exact incident that motivated this policy ---

        #[tokio::test]
        async fn regression_two_disagreeing_remotes_no_config_no_origin_errors() {
            // Recreates the amethyst NIP-05 Namecoin PR mis-target incident:
            // - `nostr` remote → maintainer A (canonical)
            // - `origin` remote → maintainer B (fork)
            // - no nostr.repo, no tracked-upstream ambiguity resolver
            // With the pre-fix HashMap-random-first-wins logic, either
            // could have been picked. With the new policy, `origin` (tier 4)
            // is chosen when it is nostr://; here we deliberately name the
            // remotes such that neither tier 3 nor 4 matches.
            let (test_repo, repo) = setup_repo();
            test_repo
                .add_remote("nostr", &nostr_url(PUBKEY_A_HEX, "amethyst"))
                .unwrap();
            test_repo
                .add_remote("co-maintainer", &nostr_url(PUBKEY_B_HEX, "amethyst"))
                .unwrap();

            let err = resolve(&repo, None).await.err().unwrap();
            let msg = format!("{err}");
            assert!(
                msg.contains("multiple nostr:// git remotes disagree"),
                "expected ambiguity error, got:\n{msg}"
            );
        }
    }
}
