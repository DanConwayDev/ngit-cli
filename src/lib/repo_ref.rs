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
        Connect, PrivateRelayProbeDecision, consolidate_fetch_outcome, finish_fetch_progress,
        get_repo_ref_from_cache, private_relay_probe_decision,
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
    /// NIP-34 indexed role tags (`M` lead, `m` co-maintainer, `o` moderator)
    /// carried verbatim from the source announcement. Their currently-active
    /// entries populate `maintainers` and `moderators`; when any role tag is
    /// present the deprecated `maintainers` tag is ignored. On republish
    /// `M`/`m` entries are not re-emitted verbatim: the typed `maintainers`
    /// field is the source of truth for current membership (mirroring the
    /// deprecated tag) and [`RepoRef::generate_role_tags`] emits an active
    /// role tag per member (`M` for `lead`, `m` otherwise) plus closed
    /// per-letter records for removals and role transitions, using these
    /// tags only as the record of start/end history boundaries. `o` tags
    /// are preserved verbatim.
    pub role_tags: Vec<Tag>,
    /// Currently-active moderators from NIP-34 `o` role tags. Moderators are
    /// deliberately excluded from `maintainers`: per NIP-34 they can never
    /// publish authoritative repository state (kind 30618), so they must not
    /// reach the state-event authority checks built on the maintainer set.
    /// An `o` self-entry also stops an announcement author from implicitly
    /// asserting maintainership.
    pub moderators: Vec<PublicKey>,
    /// The lead this announcement asserts: the pubkey of its active `M`
    /// role entry, if any. Drives emission — [`RepoRef::generate_role_tags`]
    /// gives this pubkey the `M` letter when it is a current maintainer and
    /// every other maintainer stays `m`; `None` emits only `m` tags. The
    /// repository-wide lead across members' announcements is read from
    /// `events` by [`RepoRef::lead_maintainer`], not from this field.
    pub lead: Option<PublicKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MaintainerEdge {
    pub from: PublicKey,
    pub to: PublicKey,
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
            | "M"
            | "m"
            | "o"
    )
}

/// Whether a NIP-34 indexed role tag entry is currently active. A role tag
/// lists a pubkey followed by optional alternating start/end history
/// timestamps; the entry is active when the tag has fewer than four elements
/// or an odd number of elements (its last boundary is a start).
fn role_entry_is_active(slice: &[String]) -> bool {
    slice.len() < 4 || slice.len() % 2 == 1
}

/// Whether the name is a NIP-34 indexed role tag consumed by the role-tag
/// pass in [`RepoRef::try_from`].
fn is_role_tag_name(name: &str) -> bool {
    matches!(name, "M" | "m" | "o")
}

/// Close an active role entry: append `now` as an end boundary, inserting a
/// `0` start when the entry recorded no history (active from the beginning).
fn close_role_entry(entry: &mut Vec<String>, now: u64) {
    if entry.len().is_multiple_of(2) {
        entry.push("0".to_string());
    }
    entry.push(now.to_string());
}

/// Pubkeys named by currently-active role entries on `event`, paired with
/// their role tag letter (`M`, `m` or `o`), in tag order.
fn active_role_entries(event: &nostr::prelude::Event) -> Vec<(String, PublicKey)> {
    let mut entries = Vec::new();
    for tag in event.tags.iter() {
        let slice = tag.as_slice();
        let Some(name) = slice.first().filter(|name| is_role_tag_name(name)) else {
            continue;
        };
        if !role_entry_is_active(slice) {
            continue;
        }
        if let Some(pk) = slice
            .get(1)
            .and_then(|value| PublicKey::from_str(value).ok())
        {
            entries.push((name.clone(), pk));
        }
    }
    entries
}

/// Whether `event`'s author does not assert maintainership: at least one
/// role tag names the author but none of them is an active maintainer
/// (`M`/`m`) entry — the author left by ending their self-role, or their
/// announcement
/// acknowledges only moderatorship (`o`). Per NIP-34 the self-role takes
/// precedence over assignments in other announcements, so such an author
/// must not be consolidated as a maintainer — in particular a moderator's
/// acknowledgement announcement must not turn another member's maintainer
/// assignment into authoritative state. An author absent from all role tags
/// has *not* declined — they are implicitly a maintainer for the
/// repository's entire history.
pub fn announcement_author_declines_maintainership(event: &nostr::prelude::Event) -> bool {
    let author = event.pubkey.to_string();
    let mut author_has_entry = false;
    let mut author_has_active_maintainer_entry = false;
    for tag in event.tags.iter() {
        let slice = tag.as_slice();
        let Some(name) = slice.first().filter(|name| is_role_tag_name(name)) else {
            continue;
        };
        if slice.get(1) != Some(&author) {
            continue;
        }
        author_has_entry = true;
        if name != "o" && role_entry_is_active(slice) {
            author_has_active_maintainer_entry = true;
        }
    }
    author_has_entry && !author_has_active_maintainer_entry
}

/// Whether `event`'s author does not hold moderatorship by their own
/// account: at least one `o` tag names the author but none of those entries
/// is active — they left by ending their self-role. Per NIP-34 the
/// self-declaration takes precedence over an active `o` assignment in
/// another member's announcement, so such an author must not appear in the
/// consolidated moderator set. An author with no `o` self-entry makes no
/// statement about moderatorship and never declines it here (an
/// unacknowledged assignment is an invitation, which the moderator union
/// currently surfaces).
pub fn announcement_author_declines_moderatorship(event: &nostr::prelude::Event) -> bool {
    let author = event.pubkey.to_string();
    let mut author_has_o_entry = false;
    let mut author_has_active_o_entry = false;
    for tag in event.tags.iter() {
        let slice = tag.as_slice();
        if slice.first().map(String::as_str) != Some("o") || slice.get(1) != Some(&author) {
            continue;
        }
        author_has_o_entry = true;
        if role_entry_is_active(slice) {
            author_has_active_o_entry = true;
        }
    }
    author_has_o_entry && !author_has_active_o_entry
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
            role_tags: Vec::new(),
            moderators: Vec::new(),
            lead: None,
        };

        // NIP-34 indexed role tags: ["M"|"m"|"o", "<pubkey>", <alternating
        // start/end unix timestamps>...]. The lead/co-maintainer distinction
        // carries no meaning for ngit's authorization, so both collapse into
        // one maintainer set; moderators (`o`) are kept separate because they
        // can never publish authoritative repository state. Entries whose
        // history shows the role has ended are ignored entirely: role history
        // only ever concludes that a pubkey no longer holds the role, never
        // grants retroactive authority over historic events. Duplicate tags
        // for the same pubkey are consolidated: the pubkey holds a role while
        // any of its entries is active.
        let mut role_tags_present = false;
        let mut author_has_role_entry = false;
        let mut active_role_maintainers: Vec<PublicKey> = Vec::new();
        for tag in event.tags.iter() {
            let slice = tag.as_slice();
            let Some(name) = slice.first() else { continue };
            if !is_role_tag_name(name) {
                continue;
            }
            role_tags_present = true;
            r.role_tags.push(tag.clone());
            let Some(pk) = slice.get(1).filter(|value| !value.is_empty()) else {
                continue;
            };
            let pk = PublicKey::from_str(pk)
                .context(format!("failed to convert entry from `{name}` role tag {pk} into a valid nostr public key. it should be in hex format"))
                .context("invalid repository event")?;
            if pk == event.pubkey {
                author_has_role_entry = true;
            }
            if role_entry_is_active(slice) {
                if name == "o" {
                    if !r.moderators.contains(&pk) {
                        r.moderators.push(pk);
                    }
                } else {
                    if name == "M" && r.lead.is_none() {
                        r.lead = Some(pk);
                    }
                    if !active_role_maintainers.contains(&pk) {
                        active_role_maintainers.push(pk);
                    }
                }
            }
        }
        if role_tags_present {
            // per NIP-34 an author who appears in no role tag is implicitly a
            // maintainer for the repository's entire history. An author with
            // a role entry is exactly what it records: a maintainer, a
            // moderator (never a maintainer via the implicit rule), or — when
            // every entry has ended — a member who left.
            if !author_has_role_entry {
                r.maintainers.push(event.pubkey);
            }
            for pk in active_role_maintainers {
                if !r.maintainers.contains(&pk) {
                    r.maintainers.push(pk);
                }
            }
        }

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
                [t, ..] if is_role_tag_name(t) => {
                    // consumed by the role-tag pass above; re-emission is
                    // generated from the typed fields with `role_tags` as
                    // the history record (see `generate_role_tags`)
                }
                [t, maintainers @ ..] if t == "maintainers" => {
                    // deprecated per NIP-34: ignored entirely when indexed
                    // role tags are present
                    if !role_tags_present {
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

        // If no maintainers were added, add the event's public key. With role
        // tags present an empty set is deliberate: the author's own entries
        // have all ended (they left) or record only moderatorship, and no
        // other maintainer entry is active.
        if r.maintainers.is_empty() && !role_tags_present {
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
        let public_key = signer.get_public_key().await?;
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
                    // NIP-34 indexed role tags: one tag per active
                    // maintainer, generated from the typed fields (`M` for
                    // the lead, `m` for everyone else). They are the primary
                    // maintainer listing; the `maintainers` tag emitted
                    // above degrades to the same current members for older
                    // clients. History boundaries come from the source
                    // announcement's role tags and moderator (`o`) tags are
                    // preserved verbatim. See [`RepoRef::generate_role_tags`].
                    self.generate_role_tags(&public_key, Timestamp::now().as_secs()),
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

    /// Generate the NIP-34 indexed role tags for this announcement.
    ///
    /// The typed `maintainers` field is the source of truth for *current*
    /// membership, mirroring the deprecated `maintainers` tag: every active
    /// maintainer gets an active role tag — the letter `M` for the pubkey
    /// in `self.lead`, `m` for everyone else. Without a lead no active `M`
    /// tag is emitted. `self.role_tags` (the source announcement's role
    /// tags) supplies each pubkey's per-letter start/end history — per
    /// NIP-34 a pubkey MAY appear in one `M`, one `m`, and one `o` tag to
    /// record transitions between roles:
    ///
    /// - first use of role tags (none on the source announcement): plain
    ///   untimed entries with no start time;
    /// - a pubkey with an active prior entry under the letter being emitted
    ///   keeps that entry's history verbatim;
    /// - a promotion or demotion between `M` and `m` records a per-letter
    ///   transition boundary: the old letter's active entry is closed with
    ///   `now` and the new letter's entry opens at `now`, restarting the
    ///   pubkey's ended record under that letter when one exists;
    /// - a pubkey whose prior entries all ended is started again by appending
    ///   `now` as a fresh start boundary to their record under the letter being
    ///   emitted; when their only ended record is under the other letter it is
    ///   kept verbatim and a new record opens at `now`;
    /// - a pubkey newly added while role tags are already in use starts at
    ///   `now`; the author is exempt because an author absent from prior role
    ///   tags was implicitly a member for the repository's entire history;
    /// - a removed pubkey's active entry is closed under its own letter by
    ///   appending `now` as an end boundary (inserting a `0` start when the
    ///   entry recorded no history), and already-ended records are kept so a
    ///   later re-add restarts them rather than forgetting they ever held the
    ///   role;
    /// - moderator (`o`) tags are preserved verbatim — ngit does not yet assign
    ///   or end moderators.
    pub fn generate_role_tags(&self, author: &PublicKey, now: u64) -> Vec<Tag> {
        // Prior maintainer-role entries per pubkey, split per letter
        // (`[0]` = `M`, `[1]` = `m`) since a pubkey may appear in one tag of
        // each. Entries without a pubkey slot carry no information and are
        // dropped; moderator tags pass through untouched. Among duplicate
        // same-letter entries an active one wins, otherwise the first.
        fn upsert(slot: &mut Option<Vec<String>>, entry: Vec<String>) {
            match slot {
                Some(existing)
                    if role_entry_is_active(existing) || !role_entry_is_active(&entry) => {}
                _ => *slot = Some(entry),
            }
        }
        let mut prior: Vec<(String, [Option<Vec<String>>; 2])> = Vec::new();
        let mut moderator_tags: Vec<Tag> = Vec::new();
        for tag in &self.role_tags {
            let slice = tag.as_slice();
            let Some(name) = slice.first() else { continue };
            if name == "o" {
                moderator_tags.push(tag.clone());
                continue;
            }
            let Some(pk) = slice.get(1).filter(|value| !value.is_empty()) else {
                continue;
            };
            let index = usize::from(name != "M");
            if let Some((_, records)) = prior.iter_mut().find(|(p, _)| p == pk) {
                upsert(&mut records[index], slice.to_vec());
            } else {
                let mut records = [None, None];
                upsert(&mut records[index], slice.to_vec());
                prior.push((pk.clone(), records));
            }
        }

        let first_use_of_role_tags = self.role_tags.is_empty();
        let author_hex = author.to_string();
        let lead_hex = self.lead.map(|pk| pk.to_string());
        let mut tags: Vec<Tag> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for pk in &self.maintainers {
            let pk_hex = pk.to_string();
            if !seen.insert(pk_hex.clone()) {
                continue;
            }
            let is_lead = lead_hex.as_deref() == Some(pk_hex.as_str());
            let letter = if is_lead { "M" } else { "m" };
            let (current_index, other_index) = if is_lead { (0, 1) } else { (1, 0) };
            let records = prior.iter().find(|(p, _)| *p == pk_hex).map(|(_, r)| r);
            let current_record = records.and_then(|r| r[current_index].clone());
            let other_record = records.and_then(|r| r[other_index].clone());

            let mut parts = vec![letter.to_string(), pk_hex.clone()];
            if let Some(entry) = &current_record {
                parts.extend(entry[2..].iter().cloned());
                if !role_entry_is_active(entry) {
                    // stopped record under this letter: start again now (a
                    // re-add, or a transition back to this letter)
                    parts.push(now.to_string());
                }
            } else if other_record.is_some() || (!first_use_of_role_tags && pk_hex != author_hex) {
                // the record under this letter opens now: a per-letter
                // transition from the other letter, a re-add under a new
                // letter, or a pubkey newly added while role tags are in use
                parts.push(now.to_string());
            }
            tags.push(Tag::parse(parts).unwrap());

            // the other letter's record: a transition closes its active
            // entry; an already-ended one is kept so the transition history
            // survives
            if let Some(mut entry) = other_record {
                if role_entry_is_active(&entry) {
                    close_role_entry(&mut entry, now);
                }
                tags.push(Tag::parse(entry).unwrap());
            }
        }

        // removed maintainers: prior records whose pubkey is no longer in
        // the typed field are closed under their own letter; already-ended
        // records are kept
        for (pk_hex, records) in &prior {
            if !seen.insert(pk_hex.clone()) {
                continue;
            }
            for entry in records.iter().flatten() {
                let mut entry = entry.clone();
                if role_entry_is_active(&entry) {
                    close_role_entry(&mut entry, now);
                }
                tags.push(Tag::parse(entry).unwrap());
            }
        }

        tags.extend(moderator_tags);
        tags
    }

    /// Role-history records for a republish of this announcement (the
    /// author's own prior announcement), feeding
    /// [`RepoRef::generate_role_tags`].
    ///
    /// Returns the announcement's role tags verbatim. When the announcement
    /// predates maintainer role tags — its members are listed only via the
    /// deprecated `maintainers` tag or the author's implicit membership —
    /// untimed entries are materialized from its maintainer listing so that
    /// a member dropped by the republish is closed with an end boundary
    /// rather than silently unlisted. Continuing members still emit the same
    /// untimed entries a first use of role tags would produce; only a member
    /// added in the same republish gains a start boundary, which is accurate
    /// since the prior listing proves they were not a member before.
    pub fn role_history_for_republish(&self) -> Vec<Tag> {
        let mut tags = self.role_tags.clone();
        let has_maintainer_entries = tags
            .iter()
            .any(|tag| matches!(tag.as_slice().first().map(String::as_str), Some("M" | "m")));
        if !has_maintainer_entries {
            for pk in &self.maintainers {
                let letter = if self.lead == Some(*pk) { "M" } else { "m" };
                tags.push(Tag::parse([letter, &pk.to_string()]).unwrap());
            }
        }
        tags
    }

    /// End the author's own self-role in this announcement, per NIP-34's "a
    /// member MAY leave by ending their self-role": every active role entry
    /// naming `author` — `M`, `m` and `o` alike — is closed with `now` as an
    /// end boundary, and the author is removed from the typed membership
    /// fields so a republish emits the closed records instead of an active
    /// listing. The self-declaration takes precedence over maintainer
    /// assignments in other members' announcements.
    ///
    /// History is first materialized via
    /// [`RepoRef::role_history_for_republish`], and an author who was only
    /// implicitly a member next to existing role tags gains an untimed `m`
    /// entry to close — without a closed self-entry the republished event
    /// would carry no record of the author and NIP-34 would make them an
    /// implicit maintainer again.
    ///
    /// Returns whether an active role was ended; `false` means the author
    /// held no active role in this announcement (nothing to leave), and the
    /// announcement is left unchanged apart from the history
    /// materialization.
    pub fn end_self_role(&mut self, author: &PublicKey, now: u64) -> bool {
        self.role_tags = self.role_history_for_republish();
        let author_hex = author.to_string();
        if self.maintainers.contains(author)
            && !self.role_tags.iter().any(|tag| {
                let slice = tag.as_slice();
                matches!(slice.first().map(String::as_str), Some("M" | "m"))
                    && slice.get(1) == Some(&author_hex)
            })
        {
            // implicitly a member while role tags are already in use:
            // materialize the untimed self-entry the closure below ends
            self.role_tags.push(Tag::parse(["m", &author_hex]).unwrap());
        }
        let mut ended = false;
        for tag in &mut self.role_tags {
            let slice = tag.as_slice();
            if slice.get(1) == Some(&author_hex) && role_entry_is_active(slice) {
                let mut parts = slice.to_vec();
                close_role_entry(&mut parts, now);
                *tag = Tag::parse(parts).unwrap();
                ended = true;
            }
        }
        if !ended {
            return false;
        }
        self.maintainers.retain(|pk| pk != author);
        self.moderators.retain(|pk| pk != author);
        if self.lead == Some(*author) {
            self.lead = None;
        }
        true
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
        // moderators are members: proposals and status events tag their
        // announcements too (NIP-34's "include all current members'
        // repository announcements"), so cache lookups and fetch filters
        // must cover their coordinates
        for m in &self.moderators {
            res.insert(self.announcement_coordinate(m));
        }
        res
    }

    /// Members in announcement-tag order.
    ///
    /// The maintainer selected by the `nostr://` URL or explicit repo
    /// coordinate is always first, followed by the other confirmed
    /// maintainers and then confirmed moderators — per NIP-34, repository
    /// tags SHOULD include all current members' announcements. Invited
    /// (unaccepted) maintainers and assigned-but-unacknowledged moderators
    /// come last: their announcements may not exist yet and their events are
    /// not authoritative, but tagging them means in-flight PRs and issues
    /// already tag the new member during transitions. This keeps PR/issue
    /// repository `a` tags anchored to the reciprocal group while still
    /// tagging every listed member.
    pub fn members_for_announcement_tags(&self) -> Vec<PublicKey> {
        let confirmed_maintainers: HashSet<PublicKey> =
            self.confirmed_maintainers().into_iter().collect();
        let confirmed_moderators: HashSet<PublicKey> =
            self.confirmed_moderators().into_iter().collect();

        let mut ordered = Vec::new();
        let mut seen = HashSet::new();

        if seen.insert(self.selected_maintainer) {
            ordered.push(self.selected_maintainer);
        }

        for maintainer in &self.maintainers {
            if confirmed_maintainers.contains(maintainer) && seen.insert(*maintainer) {
                ordered.push(*maintainer);
            }
        }

        for moderator in &self.moderators {
            if confirmed_moderators.contains(moderator) && seen.insert(*moderator) {
                ordered.push(*moderator);
            }
        }

        for maintainer in &self.maintainers {
            if seen.insert(*maintainer) {
                ordered.push(*maintainer);
            }
        }

        for moderator in &self.moderators {
            if seen.insert(*moderator) {
                ordered.push(*moderator);
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
    /// Per NIP-34 a listed pubkey is only invited until their own announcement
    /// makes the relationship reciprocal, and an invited pubkey's events MUST
    /// NOT be treated as authoritative. Confirmed maintainers are therefore
    /// the authoritative set: see [`RepoRef::is_authorized_maintainer`].
    ///
    /// Membership grows as a fixpoint from the selected maintainer: a
    /// candidate is confirmed only when an already-confirmed member's
    /// announcement lists them *and* their own announcement lists an
    /// already-confirmed member. Mere reachability is not enough: in a cycle
    /// of unconfirmed invitees (A lists B, B lists C, C lists A) every
    /// invitee can reach the selected maintainer without any of them ever
    /// having acknowledged a confirmed member, so none is confirmed.
    pub fn confirmed_maintainers(&self) -> Vec<PublicKey> {
        let edges = self.maintainer_edges();
        // A member's own announcement takes precedence over assignments in
        // other announcements: no active `M`/`m` self-entry (they left, or
        // acknowledge only moderatorship) removes them from the candidate
        // set even when another member still lists them as a maintainer.
        let declined: HashSet<PublicKey> = self
            .events
            .values()
            .filter(|event| announcement_author_declines_maintainership(event))
            .map(|event| event.pubkey)
            .collect();
        let mut confirmed: HashSet<PublicKey> = HashSet::new();
        if !declined.contains(&self.selected_maintainer) {
            confirmed.insert(self.selected_maintainer);
        }
        loop {
            let mut changed = false;
            for candidate in &self.maintainers {
                if confirmed.contains(candidate) || declined.contains(candidate) {
                    continue;
                }
                let listed_by_member = edges
                    .iter()
                    .any(|edge| edge.to == *candidate && confirmed.contains(&edge.from));
                let acknowledges_member = edges
                    .iter()
                    .any(|edge| edge.from == *candidate && confirmed.contains(&edge.to));
                if listed_by_member && acknowledges_member {
                    confirmed.insert(*candidate);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.maintainers
            .iter()
            .copied()
            .filter(|maintainer| confirmed.contains(maintainer))
            .collect()
    }

    /// Whether `pubkey` holds maintainer authority.
    ///
    /// True only for confirmed maintainers: authoritative repository state
    /// (kind 30618) and maintainer-only actions such as merging are gated
    /// here. Invited maintainers do not qualify until they publish an
    /// announcement that makes the relationship reciprocal, and moderators
    /// (`o` role tags, [`RepoRef::moderators`]) never do: per NIP-34 they
    /// cannot publish authoritative repository state, so a moderator-only
    /// pubkey is excluded from the maintainer set this check is built on.
    /// Member actions — status (kinds 1630-1633), label, subject and
    /// cover-note events — are gated by [`RepoRef::is_authorized_member`]
    /// instead, which also counts confirmed moderators.
    pub fn is_authorized_maintainer(&self, pubkey: &PublicKey) -> bool {
        self.confirmed_maintainers().contains(pubkey)
    }

    /// Listed maintainers not reciprocally connected to the selected group.
    /// Their events are not authoritative until they accept; see
    /// [`RepoRef::is_authorized_maintainer`].
    pub fn invited_maintainers(&self) -> Vec<PublicKey> {
        let confirmed: HashSet<_> = self.confirmed_maintainers().into_iter().collect();
        self.maintainers
            .iter()
            .copied()
            .filter(|maintainer| !confirmed.contains(maintainer))
            .collect()
    }

    /// This announcement's coordinate for `public_key`, without relay hints.
    fn announcement_coordinate(&self, public_key: &PublicKey) -> Nip19Coordinate {
        Nip19Coordinate {
            coordinate: Coordinate {
                kind: Kind::GitRepoAnnouncement,
                public_key: *public_key,
                identifier: self.identifier.clone(),
            },
            relays: vec![],
        }
    }

    /// Moderators assigned by the confirmed maintainer group: pubkeys with an
    /// active `o` entry in a confirmed maintainer's announcement, in listing
    /// order.
    ///
    /// Per NIP-34 "An `o` role can only be assigned by an `M` or `m` member;
    /// the moderator's matching self-tag acknowledges rather than assigns
    /// it", so `o` entries in a moderator-only, invited-maintainer or
    /// outsider announcement assign nothing here.
    ///
    /// Assignment alone is an invitation, mirroring
    /// [`RepoRef::invited_maintainers`]: see
    /// [`RepoRef::confirmed_moderators`] for the acknowledged subset whose
    /// member actions are authorized. Self-leave precedence (an announcement
    /// recording only ended `o` self-entries) is applied by the consolidation
    /// in `get_repo_ref_from_cache`, which consults fetched announcements
    /// beyond this event map.
    pub fn assigned_moderators(&self) -> Vec<PublicKey> {
        let mut assigned = Vec::new();
        for maintainer in self.confirmed_maintainers() {
            let Some(event) = self.events.get(&self.announcement_coordinate(&maintainer)) else {
                continue;
            };
            for (letter, pk) in active_role_entries(event) {
                if letter == "o" && !assigned.contains(&pk) {
                    assigned.push(pk);
                }
            }
        }
        assigned
    }

    /// Moderators in the selected maintainer's reciprocally connected group:
    /// assigned an active `o` role by a confirmed maintainer *and*
    /// acknowledged by their own announcement.
    ///
    /// Per NIP-34 a pubkey is invited until their own announcement
    /// acknowledges the role and assigns a role to an existing member, so
    /// confirmation requires the moderator's announcement to carry an active
    /// `o` self-entry and an active role entry naming an already-confirmed
    /// member. That entry is read only as their acknowledgement of the
    /// group — a moderator's assignments assign nothing to others.
    /// Membership grows as a fixpoint so an acknowledgement toward another
    /// confirmed moderator also confirms.
    pub fn confirmed_moderators(&self) -> Vec<PublicKey> {
        let assigned = self.assigned_moderators();
        let mut members: HashSet<PublicKey> = self.confirmed_maintainers().into_iter().collect();
        let mut confirmed: Vec<PublicKey> = Vec::new();
        loop {
            let mut changed = false;
            for candidate in &assigned {
                if confirmed.contains(candidate) {
                    continue;
                }
                let Some(event) = self.events.get(&self.announcement_coordinate(candidate)) else {
                    continue;
                };
                let entries = active_role_entries(event);
                let acknowledges_role = entries
                    .iter()
                    .any(|(letter, pk)| letter == "o" && pk == candidate);
                let acknowledges_member = entries
                    .iter()
                    .any(|(_, pk)| pk != candidate && members.contains(pk));
                if acknowledges_role && acknowledges_member {
                    confirmed.push(*candidate);
                    members.insert(*candidate);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        confirmed
    }

    /// All current members: confirmed maintainers followed by confirmed
    /// moderators.
    ///
    /// Per NIP-34 all members can perform "other maintainer actions" —
    /// status (kinds 1630-1633), label, subject and cover-note events — but
    /// only maintainers may publish authoritative repository state, so
    /// state authority keeps using [`RepoRef::confirmed_maintainers`].
    pub fn confirmed_members(&self) -> Vec<PublicKey> {
        let mut members = self.confirmed_maintainers();
        for moderator in self.confirmed_moderators() {
            if !members.contains(&moderator) {
                members.push(moderator);
            }
        }
        members
    }

    /// Whether `pubkey` may author member actions: status (kinds 1630-1633),
    /// label, subject and cover-note events. True for confirmed maintainers
    /// and confirmed moderators alike. Authoritative repository state (kind
    /// 30618) and maintainer-only actions such as merging remain gated by
    /// [`RepoRef::is_authorized_maintainer`].
    pub fn is_authorized_member(&self, pubkey: &PublicKey) -> bool {
        self.confirmed_members().contains(pubkey)
    }

    /// The repository's lead, read directly from `M` role tags: the unique
    /// pubkey a confirmed member's announcement assigns an active `M` entry.
    ///
    /// Only members' events are authoritative, so `M` assignments in
    /// unconfirmed announcements are ignored — an outsider cannot make
    /// themselves lead by self-assertion. The assigned pubkey itself need
    /// not be confirmed yet: a freshly designated lead who has not accepted
    /// is still the lead (and invited), but a pubkey outside the current
    /// maintainer set — e.g. one whose own announcement declines
    /// maintainership — never is. Announcements without `M` tags assert no
    /// lead; in particular the deprecated `maintainers` fallback has none.
    /// When confirmed members disagree (active `M` assignments for two
    /// different pubkeys) no lead is reported, so UIs omit the lead
    /// indication rather than asserting a contested one.
    pub fn lead_maintainer(&self) -> Option<PublicKey> {
        let confirmed: HashSet<PublicKey> = self.confirmed_maintainers().into_iter().collect();
        let mut leads: Vec<PublicKey> = Vec::new();
        for event in self.events.values() {
            if !confirmed.contains(&event.pubkey) {
                continue;
            }
            for tag in event.tags.iter() {
                let slice = tag.as_slice();
                if slice.first().map(String::as_str) != Some("M") || !role_entry_is_active(slice) {
                    continue;
                }
                let Some(pk) = slice
                    .get(1)
                    .and_then(|value| PublicKey::from_str(value).ok())
                else {
                    continue;
                };
                if self.maintainers.contains(&pk) && !leads.contains(&pk) {
                    leads.push(pk);
                }
            }
        }
        if leads.len() == 1 { leads.pop() } else { None }
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
                finish_fetch_progress(&relay_reports, progress_reporter)?;
                let outcome = consolidate_fetch_outcome(relay_reports);
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
            role_tags: vec![],
            moderators: vec![],
            lead: None,
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
            role_tags: vec![],
            moderators: vec![],
            lead: None,
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
                repo_ref.members_for_announcement_tags(),
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
        async fn cycle_of_unreciprocated_invitees_confirms_no_one() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let invitee_b_keys = &*TEST_KEY_2_KEYS;
            let invitee_b = invitee_b_keys.public_key();
            let invitee_c_keys = nostr::prelude::Keys::generate();
            let invitee_c = invitee_c_keys.public_key();

            // A lists B, B lists C, C lists A: both invitees can *reach* the
            // selected maintainer through the cycle, but B never acknowledged
            // an already-confirmed member and C was never listed by one, so
            // neither is confirmed.
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, invitee_b, invitee_c], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, invitee_b]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(invitee_b_keys, vec![invitee_b, invitee_c]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&invitee_c_keys, vec![invitee_c, selected]).await,
            );

            assert_eq!(repo_ref.confirmed_maintainers(), vec![selected]);
            assert_eq!(repo_ref.invited_maintainers(), vec![invitee_b, invitee_c]);
            assert!(!repo_ref.is_authorized_maintainer(&invitee_b));
            assert!(!repo_ref.is_authorized_maintainer(&invitee_c));
        }

        #[tokio::test]
        async fn acceptance_toward_any_confirmed_member_confirms_recursively() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let co_keys = &*TEST_KEY_2_KEYS;
            let co = co_keys.public_key();
            let third_keys = nostr::prelude::Keys::generate();
            let third = third_keys.public_key();

            // A and B are reciprocal; B lists C and C acknowledges B: C is
            // listed by a confirmed member and acknowledges one, so
            // confirmation grows through B without C ever listing A.
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, co, third], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, co]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(co_keys, vec![co, selected, third]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&third_keys, vec![third, co]).await,
            );

            assert_eq!(repo_ref.confirmed_maintainers(), vec![selected, co, third]);
        }

        #[tokio::test]
        async fn only_confirmed_maintainers_are_authorized() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let invited = TEST_KEY_2_KEYS.public_key();
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, invited], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, invited]).await,
            );

            assert!(repo_ref.is_authorized_maintainer(&selected));
            assert!(!repo_ref.is_authorized_maintainer(&invited));

            // acceptance makes the relationship reciprocal and authorizes the
            // previously invited maintainer
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_2_KEYS, vec![invited, selected]).await,
            );
            assert!(repo_ref.is_authorized_maintainer(&invited));
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

        async fn announcement_with_lead(
            keys: &nostr::prelude::Keys,
            listed: Vec<PublicKey>,
            lead: Option<PublicKey>,
        ) -> nostr::prelude::Event {
            let signer = Arc::new(crate::NgitSigner::Keys(keys.clone()));
            let mut repo_ref = create_repo_ref_for_maintainer_order(listed, vec![]);
            repo_ref.selected_maintainer = keys.public_key();
            repo_ref.lead = lead;
            repo_ref.to_event(&signer).await.unwrap()
        }

        #[tokio::test]
        async fn lead_is_read_from_a_confirmed_members_active_m_tag() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let lead = TEST_KEY_2_KEYS.public_key();
            let mut repo_ref = create_repo_ref_for_maintainer_order(vec![selected, lead], vec![]);
            insert_event(
                &mut repo_ref,
                announcement_with_lead(&TEST_KEY_1_KEYS, vec![selected, lead], Some(lead)).await,
            );

            // a freshly designated lead who has not announced yet is still
            // the lead (and an invited maintainer)
            assert_eq!(repo_ref.lead_maintainer(), Some(lead));

            insert_event(
                &mut repo_ref,
                announcement_with_lead(&TEST_KEY_2_KEYS, vec![lead, selected], Some(lead)).await,
            );
            assert_eq!(repo_ref.lead_maintainer(), Some(lead));
        }

        #[tokio::test]
        async fn graph_structure_alone_infers_no_lead() {
            // the graph that the retired in-degree inference reported a lead
            // for: without any `M` tag on the wire there is no lead
            let selected = TEST_KEY_1_KEYS.public_key();
            let listed_most = TEST_KEY_2_KEYS.public_key();
            let third_keys = nostr::prelude::Keys::generate();
            let third = third_keys.public_key();
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, listed_most, third], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, listed_most, third]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_2_KEYS, vec![listed_most, selected]).await,
            );
            insert_event(
                &mut repo_ref,
                announcement(&third_keys, vec![third, listed_most]).await,
            );

            assert_eq!(repo_ref.lead_maintainer(), None);
        }

        #[tokio::test]
        async fn unconfirmed_m_assignments_do_not_create_a_lead() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let outsider_keys = nostr::prelude::Keys::generate();
            let outsider = outsider_keys.public_key();
            let mut repo_ref =
                create_repo_ref_for_maintainer_order(vec![selected, outsider], vec![]);
            insert_event(
                &mut repo_ref,
                announcement(&TEST_KEY_1_KEYS, vec![selected, outsider]).await,
            );
            // the invited outsider asserts themselves lead without ever
            // acknowledging a confirmed member: not authoritative
            insert_event(
                &mut repo_ref,
                announcement_with_lead(&outsider_keys, vec![outsider], Some(outsider)).await,
            );

            assert_eq!(repo_ref.lead_maintainer(), None);
        }

        #[tokio::test]
        async fn conflicting_m_assignments_yield_no_lead() {
            let selected = TEST_KEY_1_KEYS.public_key();
            let other = TEST_KEY_2_KEYS.public_key();
            let mut repo_ref = create_repo_ref_for_maintainer_order(vec![selected, other], vec![]);
            insert_event(
                &mut repo_ref,
                announcement_with_lead(&TEST_KEY_1_KEYS, vec![selected, other], Some(selected))
                    .await,
            );
            insert_event(
                &mut repo_ref,
                announcement_with_lead(&TEST_KEY_2_KEYS, vec![other, selected], Some(other)).await,
            );

            assert_eq!(repo_ref.lead_maintainer(), None);
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

    /// NIP-34 indexed role tags (`M`/`m`): activeness by element count,
    /// precedence over the deprecated `maintainers` tag, implicit author
    /// membership, leaving via an ended self-entry, and re-emission generated
    /// from the typed maintainer set (see [`RepoRef::generate_role_tags`]).
    mod role_tags {
        use nostr::prelude::{EventBuilder, event::FinalizeEvent};

        use super::*;

        fn tag(parts: &[&str]) -> Vec<String> {
            parts.iter().map(ToString::to_string).collect()
        }

        fn role_event(
            keys: &nostr::prelude::Keys,
            tags: Vec<Vec<String>>,
        ) -> nostr::prelude::Event {
            let mut event_tags = vec![Tag::identifier("test-repo")];
            for t in tags {
                event_tags.push(Tag::parse(t).unwrap());
            }
            EventBuilder::new(Kind::GitRepoAnnouncement, "")
                .tags(event_tags)
                .finalize(keys)
                .unwrap()
        }

        #[test]
        fn entry_activeness_follows_element_count() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let other = nostr::prelude::Keys::generate().public_key();
            let other_hex = other.to_string();

            // (history values after the pubkey, expected active)
            // Tag element counts: 2 -> active, 3 -> active, 4 -> ended,
            // 5 -> active, 6 -> ended.
            let cases: Vec<(Vec<&str>, bool)> = vec![
                (vec![], true),
                (vec!["100"], true),
                (vec!["100", "200"], false),
                (vec!["100", "200", "300"], true),
                (vec!["100", "200", "300", "400"], false),
            ];

            for (history, expected_active) in cases {
                let mut m_tag = vec!["m".to_string(), other_hex.clone()];
                m_tag.extend(history.iter().map(ToString::to_string));
                let event =
                    role_event(&keys, vec![tag(&["M", &author.to_string()]), m_tag.clone()]);
                let parsed = RepoRef::try_from((event, None)).unwrap();
                assert_eq!(
                    parsed.maintainers.contains(&other),
                    expected_active,
                    "history {history:?} expected active={expected_active}"
                );
            }
        }

        #[test]
        fn lead_and_co_maintainer_collapse_into_one_maintainer_set() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let listed_as_lead = nostr::prelude::Keys::generate().public_key();
            let listed_as_co = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["M", &listed_as_lead.to_string()]),
                    tag(&["m", &listed_as_co.to_string()]),
                ],
            );
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert_eq!(
                parsed.maintainers,
                vec![author, listed_as_lead, listed_as_co]
            );
        }

        #[test]
        fn deprecated_maintainers_tag_is_ignored_when_role_tags_present() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let legacy_listed = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["maintainers", &legacy_listed.to_string()]),
                ],
            );
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert_eq!(parsed.maintainers, vec![author]);
        }

        #[test]
        fn author_without_role_entry_is_implicitly_a_maintainer() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let other = nostr::prelude::Keys::generate().public_key();

            let event = role_event(&keys, vec![tag(&["m", &other.to_string()])]);
            let parsed = RepoRef::try_from((event.clone(), None)).unwrap();
            assert_eq!(parsed.maintainers, vec![author, other]);
            assert!(!announcement_author_declines_maintainership(&event));
        }

        #[test]
        fn author_with_only_ended_entries_has_left() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let lead = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["m", &author.to_string(), "0", "1700000000"]),
                ],
            );
            let parsed = RepoRef::try_from((event.clone(), None)).unwrap();
            assert_eq!(parsed.maintainers, vec![lead]);
            assert!(announcement_author_declines_maintainership(&event));

            // an active self-entry means the author has not left
            let active = role_event(&keys, vec![tag(&["M", &author.to_string()])]);
            assert!(!announcement_author_declines_maintainership(&active));

            // the deprecated format never records leaving
            let legacy = role_event(&keys, vec![tag(&["maintainers", &lead.to_string()])]);
            assert!(!announcement_author_declines_maintainership(&legacy));
        }

        #[test]
        fn role_transition_is_active_while_either_entry_is_active() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let demoted = nostr::prelude::Keys::generate().public_key();
            let gone = nostr::prelude::Keys::generate().public_key();

            // one `M` and one `m` tag for the same pubkey record a transition
            // between the roles: the pubkey is a maintainer while either
            // entry is active, and no longer one when both have ended
            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["M", &demoted.to_string(), "0", "100"]),
                    tag(&["m", &demoted.to_string(), "100"]),
                    tag(&["M", &gone.to_string(), "0", "100"]),
                    tag(&["m", &gone.to_string(), "100", "200"]),
                ],
            );
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert!(parsed.maintainers.contains(&demoted));
            assert!(!parsed.maintainers.contains(&gone));
        }

        #[test]
        fn duplicate_same_letter_entries_are_consolidated() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let returning = nostr::prelude::Keys::generate().public_key();
            let ended_twice = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["m", &returning.to_string(), "0", "100"]),
                    tag(&["m", &returning.to_string(), "200"]),
                    tag(&["m", &ended_twice.to_string(), "0", "100"]),
                    tag(&["m", &ended_twice.to_string(), "200", "300"]),
                ],
            );
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert!(parsed.maintainers.contains(&returning));
            assert!(!parsed.maintainers.contains(&ended_twice));
        }

        #[test]
        fn lead_field_follows_the_active_m_uppercase_entry() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let lead = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["m", &author.to_string()]),
                ],
            );
            assert_eq!(RepoRef::try_from((event, None)).unwrap().lead, Some(lead));

            // an ended `M` entry asserts no lead
            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string(), "0", "100"]),
                    tag(&["m", &author.to_string()]),
                ],
            );
            assert_eq!(RepoRef::try_from((event, None)).unwrap().lead, None);

            // `m`-only announcements assert no lead
            let event = role_event(&keys, vec![tag(&["m", &author.to_string()])]);
            assert_eq!(RepoRef::try_from((event, None)).unwrap().lead, None);
        }

        #[test]
        fn moderators_are_parsed_and_supersede_the_maintainers_tag() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let moderator = nostr::prelude::Keys::generate().public_key();
            let legacy_listed = nostr::prelude::Keys::generate().public_key();

            // an `o` tag alone counts as role-tag usage: the deprecated
            // `maintainers` tag is ignored and the author is implicitly the
            // sole maintainer
            let event = role_event(
                &keys,
                vec![
                    tag(&["o", &moderator.to_string()]),
                    tag(&["maintainers", &legacy_listed.to_string()]),
                ],
            );
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert_eq!(parsed.maintainers, vec![author]);
            assert_eq!(parsed.moderators, vec![moderator]);
        }

        #[test]
        fn ended_moderator_entries_are_ignored() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let former = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["o", &former.to_string(), "0", "100"]),
                ],
            );
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert!(parsed.moderators.is_empty());
            assert!(!parsed.maintainers.contains(&former));
        }

        #[test]
        fn moderator_self_entry_does_not_assert_maintainership() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let lead = nostr::prelude::Keys::generate().public_key();

            // a moderator's acknowledgement announcement: without `o` support
            // the author would appear in no known role tag and wrongly become
            // an implicit maintainer, making their state events authoritative
            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["o", &author.to_string()]),
                ],
            );
            let parsed = RepoRef::try_from((event.clone(), None)).unwrap();
            assert_eq!(parsed.maintainers, vec![lead]);
            assert_eq!(parsed.moderators, vec![author]);
            // an `o` self-entry acknowledges only moderatorship, which takes
            // precedence over maintainer assignments in other announcements
            assert!(announcement_author_declines_maintainership(&event));
        }

        #[test]
        fn author_with_only_ended_o_entries_declines_moderatorship() {
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let lead = nostr::prelude::Keys::generate().public_key();

            // an ended `o` self-entry records leaving moderatorship, which
            // takes precedence over another member's active assignment
            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["o", &author.to_string(), "0", "100"]),
                ],
            );
            assert!(announcement_author_declines_moderatorship(&event));

            // an active `o` self-entry acknowledges the role
            let active = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["o", &author.to_string()]),
                ],
            );
            assert!(!announcement_author_declines_moderatorship(&active));

            // no `o` self-entry makes no statement about moderatorship,
            // even when the announcement ends the author's maintainer role
            let maintainer_only = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["m", &author.to_string(), "0", "100"]),
                ],
            );
            assert!(!announcement_author_declines_moderatorship(
                &maintainer_only
            ));

            // an `o` entry naming someone else is an assignment, not a
            // statement about the author's own moderatorship
            let other = nostr::prelude::Keys::generate().public_key();
            let assigns_other = role_event(
                &keys,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["o", &other.to_string(), "0", "100"]),
                ],
            );
            assert!(!announcement_author_declines_moderatorship(&assigns_other));
        }

        #[test]
        fn leave_produced_announcement_declines_both_roles() {
            // `end_self_role` closes every active self-entry, so the
            // republished announcement of a maintainer-and-moderator who
            // left declines maintainership and moderatorship alike
            let keys = nostr::prelude::Keys::generate();
            let author = keys.public_key();
            let lead = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &keys,
                vec![
                    tag(&["M", &lead.to_string()]),
                    tag(&["m", &author.to_string(), "0", "100"]),
                    tag(&["o", &author.to_string(), "0", "100"]),
                ],
            );
            assert!(announcement_author_declines_maintainership(&event));
            assert!(announcement_author_declines_moderatorship(&event));
        }

        #[test]
        fn moderator_acknowledgement_does_not_confirm_maintainership() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let moderator_keys = nostr::prelude::Keys::generate();
            let moderator = moderator_keys.public_key();

            // the owner assigns `m` to the moderator's pubkey, but the
            // moderator's own announcement acknowledges only moderatorship:
            // the self-role takes precedence, so the acknowledgement edge
            // back to the owner must not confirm them as a maintainer with
            // authoritative state
            let owner_event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["m", &moderator.to_string()]),
                ],
            );
            let moderator_event = role_event(
                &moderator_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                ],
            );

            let mut repo_ref = RepoRef::try_from((owner_event, None)).unwrap();
            repo_ref.events.insert(
                Nip19Coordinate {
                    coordinate: Coordinate {
                        kind: Kind::GitRepoAnnouncement,
                        public_key: moderator,
                        identifier: "test-repo".to_string(),
                    },
                    relays: vec![],
                },
                moderator_event,
            );
            // as consolidated by get_repo_ref_from_cache before its
            // declines-maintainership retain
            repo_ref.maintainers = vec![owner, moderator];

            assert_eq!(repo_ref.confirmed_maintainers(), vec![owner]);
            assert!(repo_ref.is_authorized_maintainer(&owner));
            assert!(!repo_ref.is_authorized_maintainer(&moderator));
        }

        #[test]
        fn moderator_is_not_authorized_for_state_events() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let moderator = nostr::prelude::Keys::generate().public_key();

            let event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                ],
            );
            let repo_ref = RepoRef::try_from((event, None)).unwrap();
            assert!(repo_ref.is_authorized_maintainer(&owner));
            assert!(!repo_ref.is_authorized_maintainer(&moderator));
        }

        fn insert_announcement(repo_ref: &mut RepoRef, event: nostr::prelude::Event) {
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

        #[test]
        fn o_assignment_by_a_moderator_or_invited_maintainer_assigns_nothing() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let moderator_keys = nostr::prelude::Keys::generate();
            let moderator = moderator_keys.public_key();
            let invited_keys = nostr::prelude::Keys::generate();
            let invited = invited_keys.public_key();
            let assigned_by_moderator = nostr::prelude::Keys::generate().public_key();
            let assigned_by_invited = nostr::prelude::Keys::generate().public_key();

            let owner_event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["m", &invited.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                ],
            );
            // the moderator acknowledges their role but also tries to assign
            // `o` to a third pubkey: only `M`/`m` members can assign `o`
            let moderator_event = role_event(
                &moderator_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                    tag(&["o", &assigned_by_moderator.to_string()]),
                ],
            );
            // the invited maintainer never acknowledged a member, so their
            // announcement is not authoritative and assigns nothing either
            let invited_event = role_event(
                &invited_keys,
                vec![
                    tag(&["m", &invited.to_string()]),
                    tag(&["o", &assigned_by_invited.to_string()]),
                ],
            );

            let mut repo_ref = RepoRef::try_from((owner_event, None)).unwrap();
            insert_announcement(&mut repo_ref, moderator_event);
            insert_announcement(&mut repo_ref, invited_event);
            repo_ref.maintainers = vec![owner, invited];

            assert_eq!(repo_ref.assigned_moderators(), vec![moderator]);
            assert_eq!(repo_ref.confirmed_moderators(), vec![moderator]);
        }

        #[test]
        fn moderator_confirmation_requires_acknowledgement_toward_a_member() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let silent = nostr::prelude::Keys::generate().public_key();
            let moderator_keys = nostr::prelude::Keys::generate();
            let moderator = moderator_keys.public_key();

            let owner_event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &silent.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                ],
            );
            let mut repo_ref = RepoRef::try_from((owner_event, None)).unwrap();

            // `silent` has no announcement and `moderator`'s self-`o` names
            // no member: both are assigned (invited) but unconfirmed
            insert_announcement(
                &mut repo_ref,
                role_event(&moderator_keys, vec![tag(&["o", &moderator.to_string()])]),
            );
            assert_eq!(repo_ref.assigned_moderators(), vec![silent, moderator]);
            assert!(repo_ref.confirmed_moderators().is_empty());

            // acknowledging the role and an existing member confirms
            insert_announcement(
                &mut repo_ref,
                role_event(
                    &moderator_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &moderator.to_string()]),
                    ],
                ),
            );
            assert_eq!(repo_ref.confirmed_moderators(), vec![moderator]);

            // an ended self-`o` records leaving, never an acknowledgement
            insert_announcement(
                &mut repo_ref,
                role_event(
                    &moderator_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &moderator.to_string(), "0", "100"]),
                    ],
                ),
            );
            assert!(repo_ref.confirmed_moderators().is_empty());
        }

        #[test]
        fn moderator_confirmation_grows_through_acknowledged_moderators() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let first_keys = nostr::prelude::Keys::generate();
            let first = first_keys.public_key();
            let second_keys = nostr::prelude::Keys::generate();
            let second = second_keys.public_key();

            let owner_event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &first.to_string()]),
                    tag(&["o", &second.to_string()]),
                ],
            );
            let mut repo_ref = RepoRef::try_from((owner_event, None)).unwrap();
            insert_announcement(
                &mut repo_ref,
                role_event(
                    &first_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &first.to_string()]),
                    ],
                ),
            );
            // the second moderator acknowledges toward the first — an
            // existing member once the fixpoint confirms the first
            insert_announcement(
                &mut repo_ref,
                role_event(
                    &second_keys,
                    vec![
                        tag(&["o", &second.to_string()]),
                        tag(&["o", &first.to_string()]),
                    ],
                ),
            );

            assert_eq!(repo_ref.confirmed_moderators(), vec![first, second]);
        }

        #[test]
        fn announcement_tags_and_coordinates_cover_members_and_invitees() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let invited = nostr::prelude::Keys::generate().public_key();
            let moderator_keys = nostr::prelude::Keys::generate();
            let moderator = moderator_keys.public_key();
            let unacknowledged = nostr::prelude::Keys::generate().public_key();

            let owner_event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["m", &invited.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                    tag(&["o", &unacknowledged.to_string()]),
                ],
            );
            let mut repo_ref = RepoRef::try_from((owner_event, None)).unwrap();
            insert_announcement(
                &mut repo_ref,
                role_event(
                    &moderator_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &moderator.to_string()]),
                    ],
                ),
            );

            // members first (selected maintainer, then the confirmed
            // moderator), invitees last (unaccepted maintainer, then the
            // unacknowledged moderator)
            assert_eq!(
                repo_ref.members_for_announcement_tags(),
                vec![owner, moderator, invited, unacknowledged]
            );

            // coordinate sets used for cache lookups and fetch filters
            // cover the moderators' announcements too
            for pk in [owner, invited, moderator, unacknowledged] {
                assert!(
                    repo_ref
                        .coordinates()
                        .iter()
                        .any(|c| c.public_key == pk && c.identifier == "test-repo")
                );
            }
        }

        #[test]
        fn confirmed_moderators_are_authorized_members_but_not_maintainers() {
            let owner_keys = nostr::prelude::Keys::generate();
            let owner = owner_keys.public_key();
            let moderator_keys = nostr::prelude::Keys::generate();
            let moderator = moderator_keys.public_key();
            let outsider = nostr::prelude::Keys::generate().public_key();

            let owner_event = role_event(
                &owner_keys,
                vec![
                    tag(&["M", &owner.to_string()]),
                    tag(&["o", &moderator.to_string()]),
                ],
            );
            let mut repo_ref = RepoRef::try_from((owner_event, None)).unwrap();

            // assigned but unacknowledged: an invited moderator has no
            // member authority
            assert!(!repo_ref.is_authorized_member(&moderator));

            insert_announcement(
                &mut repo_ref,
                role_event(
                    &moderator_keys,
                    vec![
                        tag(&["M", &owner.to_string()]),
                        tag(&["o", &moderator.to_string()]),
                    ],
                ),
            );
            assert_eq!(repo_ref.confirmed_members(), vec![owner, moderator]);
            assert!(repo_ref.is_authorized_member(&owner));
            assert!(repo_ref.is_authorized_member(&moderator));
            assert!(!repo_ref.is_authorized_member(&outsider));
            // members are not maintainers: repository state stays barred
            assert!(!repo_ref.is_authorized_maintainer(&moderator));
        }

        #[tokio::test]
        async fn round_trip_emits_m_tags_for_active_maintainers_and_degrades() {
            let author = TEST_KEY_1_KEYS.public_key();
            let active = nostr::prelude::Keys::generate().public_key();
            let ended = nostr::prelude::Keys::generate().public_key();
            let moderator = nostr::prelude::Keys::generate().public_key();

            let source_tags = vec![
                tag(&["M", &author.to_string()]),
                tag(&["m", &active.to_string(), "100"]),
                tag(&["m", &ended.to_string(), "0", "100"]),
                tag(&["o", &moderator.to_string()]),
            ];
            let event = role_event(&TEST_KEY_1_KEYS, source_tags);
            let parsed = RepoRef::try_from((event, None)).unwrap();
            assert_eq!(parsed.maintainers, vec![author, active]);
            let re_emitted = parsed.to_event(&TEST_KEY_1_SIGNER).await.unwrap();

            // one role tag per active maintainer with its history carried
            // over (the author's lead entry is re-asserted as `M`), the
            // removed maintainer's ended record preserved, and the `o` tag
            // kept
            let emitted_role_tags: Vec<Vec<String>> = re_emitted
                .tags
                .iter()
                .map(|t| t.as_slice().to_vec())
                .filter(|s| {
                    s.first()
                        .is_some_and(|name| name == "M" || name == "m" || name == "o")
                })
                .collect();
            assert_eq!(
                emitted_role_tags,
                vec![
                    tag(&["M", &author.to_string()]),
                    tag(&["m", &active.to_string(), "100"]),
                    tag(&["m", &ended.to_string(), "0", "100"]),
                    tag(&["o", &moderator.to_string()]),
                ],
            );

            // the deprecated tag degrades to exactly the same current
            // members as the active `m` entries and never includes
            // moderators or ended records
            let maintainers_tag = re_emitted
                .tags
                .iter()
                .find(|t| t.as_slice()[0].eq("maintainers"))
                .unwrap();
            assert_eq!(
                maintainers_tag.as_slice()[1..].to_vec(),
                vec![author.to_string(), active.to_string()],
            );
        }

        /// The history rules of [`RepoRef::generate_role_tags`], pinned with
        /// a deterministic `now`.
        mod generation {
            use super::*;

            const NOW: u64 = 1_700_000_000;

            fn generate(
                role_tags: Vec<Vec<String>>,
                maintainers: Vec<PublicKey>,
                author: &PublicKey,
            ) -> Vec<Vec<String>> {
                generate_with_lead(role_tags, maintainers, None, author)
            }

            fn generate_with_lead(
                role_tags: Vec<Vec<String>>,
                maintainers: Vec<PublicKey>,
                lead: Option<PublicKey>,
                author: &PublicKey,
            ) -> Vec<Vec<String>> {
                let mut repo_ref = create_repo_ref_for_maintainer_order(maintainers, vec![]);
                repo_ref.lead = lead;
                repo_ref.role_tags = role_tags
                    .into_iter()
                    .map(|t| Tag::parse(t).unwrap())
                    .collect();
                repo_ref
                    .generate_role_tags(author, NOW)
                    .iter()
                    .map(|t| t.as_slice().to_vec())
                    .collect()
            }

            fn now() -> String {
                NOW.to_string()
            }

            #[test]
            fn first_use_of_role_tags_emits_untimed_entries() {
                let author = nostr::prelude::Keys::generate().public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(vec![], vec![author, other], &author),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &other.to_string()]),
                    ],
                );
            }

            #[test]
            fn newly_added_maintainer_starts_now_once_role_tags_are_in_use() {
                let author = nostr::prelude::Keys::generate().public_key();
                let added = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![tag(&["m", &author.to_string()])],
                        vec![author, added],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &added.to_string(), &now()]),
                    ],
                );
            }

            #[test]
            fn author_without_prior_entry_was_implicit_and_stays_untimed() {
                let author = nostr::prelude::Keys::generate().public_key();
                let listed = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![tag(&["m", &listed.to_string()])],
                        vec![author, listed],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &listed.to_string()]),
                    ],
                );
            }

            #[test]
            fn stopped_maintainer_is_started_again_now() {
                let author = nostr::prelude::Keys::generate().public_key();
                let returning = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["m", &returning.to_string(), "0", "100"]),
                        ],
                        vec![author, returning],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &returning.to_string(), "0", "100", &now()]),
                    ],
                );
            }

            #[test]
            fn removed_maintainer_is_ended_now_with_zero_start_fallback() {
                let author = nostr::prelude::Keys::generate().public_key();
                let untimed = nostr::prelude::Keys::generate().public_key();
                let timed = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["m", &untimed.to_string()]),
                            tag(&["m", &timed.to_string(), "50"]),
                        ],
                        vec![author],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &untimed.to_string(), "0", &now()]),
                        tag(&["m", &timed.to_string(), "50", &now()]),
                    ],
                );
            }

            #[test]
            fn already_ended_records_are_preserved() {
                let author = nostr::prelude::Keys::generate().public_key();
                let former = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["m", &former.to_string(), "0", "100"]),
                        ],
                        vec![author],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &former.to_string(), "0", "100"]),
                    ],
                );
            }

            #[test]
            fn moderator_tags_are_preserved_verbatim() {
                let author = nostr::prelude::Keys::generate().public_key();
                let moderator = nostr::prelude::Keys::generate().public_key();
                let former = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["o", &moderator.to_string()]),
                            tag(&["o", &former.to_string(), "0", "100"]),
                        ],
                        vec![author],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["o", &moderator.to_string()]),
                        tag(&["o", &former.to_string(), "0", "100"]),
                    ],
                );
            }

            #[test]
            fn the_lead_gets_the_uppercase_m_tag() {
                let author = nostr::prelude::Keys::generate().public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate_with_lead(vec![], vec![author, other], Some(author), &author),
                    vec![
                        tag(&["M", &author.to_string()]),
                        tag(&["m", &other.to_string()]),
                    ],
                );
            }

            #[test]
            fn newly_designated_lead_starts_now_once_role_tags_are_in_use() {
                let author = nostr::prelude::Keys::generate().public_key();
                let lead = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate_with_lead(
                        vec![tag(&["m", &author.to_string()])],
                        vec![author, lead],
                        Some(lead),
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["M", &lead.to_string(), &now()]),
                    ],
                );
            }

            #[test]
            fn promotion_and_demotion_record_per_letter_boundaries() {
                // the lead moves from the author to the other maintainer:
                // each old letter's active entry is closed and the new
                // letter's entry opens now, so a pubkey's record of each
                // role survives the transition (a pubkey MAY appear in one
                // `M` and one `m` tag)
                let author = nostr::prelude::Keys::generate().public_key();
                let promoted = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate_with_lead(
                        vec![
                            tag(&["M", &author.to_string()]),
                            tag(&["m", &promoted.to_string(), "100"]),
                        ],
                        vec![author, promoted],
                        Some(promoted),
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string(), &now()]),
                        tag(&["M", &author.to_string(), "0", &now()]),
                        tag(&["M", &promoted.to_string(), &now()]),
                        tag(&["m", &promoted.to_string(), "100", &now()]),
                    ],
                );
            }

            #[test]
            fn restart_of_a_returning_lead_continues_the_m_uppercase_record() {
                let author = nostr::prelude::Keys::generate().public_key();
                let returning = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate_with_lead(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["M", &returning.to_string(), "0", "100"]),
                            tag(&["m", &returning.to_string(), "100", "200"]),
                        ],
                        vec![author, returning],
                        Some(returning),
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["M", &returning.to_string(), "0", "100", &now()]),
                        tag(&["m", &returning.to_string(), "100", "200"]),
                    ],
                );
            }

            #[test]
            fn demotion_closes_the_lead_record_and_opens_a_co_maintainer_one() {
                // no lead asserted any more: the author's active `M` entry
                // is closed with its recorded start preserved and their `m`
                // record opens now
                let author = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![tag(&["M", &author.to_string(), "100"])],
                        vec![author],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string(), &now()]),
                        tag(&["M", &author.to_string(), "100", &now()]),
                    ],
                );
            }

            #[test]
            fn restart_continues_the_co_maintainer_record_over_a_former_lead() {
                let author = nostr::prelude::Keys::generate().public_key();
                let returning = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["M", &returning.to_string(), "0", "100"]),
                            tag(&["m", &returning.to_string(), "100", "200"]),
                        ],
                        vec![author, returning],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &returning.to_string(), "100", "200", &now()]),
                        tag(&["M", &returning.to_string(), "0", "100"]),
                    ],
                );
            }

            #[test]
            fn removed_maintainers_are_closed_under_their_own_letter() {
                // a removed lead's record closes as `M`, not `m`, and a
                // removed pubkey with records under both letters keeps both
                let author = nostr::prelude::Keys::generate().public_key();
                let former_lead = nostr::prelude::Keys::generate().public_key();
                let former_both = nostr::prelude::Keys::generate().public_key();
                assert_eq!(
                    generate(
                        vec![
                            tag(&["m", &author.to_string()]),
                            tag(&["M", &former_lead.to_string()]),
                            tag(&["M", &former_both.to_string(), "0", "100"]),
                            tag(&["m", &former_both.to_string(), "100"]),
                        ],
                        vec![author],
                        &author,
                    ),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["M", &former_lead.to_string(), "0", &now()]),
                        tag(&["M", &former_both.to_string(), "0", "100"]),
                        tag(&["m", &former_both.to_string(), "100", &now()]),
                    ],
                );
            }
        }

        /// [`RepoRef::role_history_for_republish`]: verbatim pass-through
        /// once maintainer role tags exist, and materialization of untimed
        /// entries from a deprecated-listing announcement so a member
        /// dropped on republish closes with an end boundary instead of
        /// silently vanishing.
        mod republish_history {
            use super::*;

            const NOW: u64 = 1_700_000_000;

            fn history_of(repo_ref: &RepoRef) -> Vec<Vec<String>> {
                repo_ref
                    .role_history_for_republish()
                    .iter()
                    .map(|t| t.as_slice().to_vec())
                    .collect()
            }

            #[test]
            fn existing_maintainer_role_tags_pass_through_verbatim() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let former = nostr::prelude::Keys::generate().public_key();
                let moderator = nostr::prelude::Keys::generate().public_key();
                let source = vec![
                    tag(&["m", &author.to_string(), "100"]),
                    tag(&["m", &former.to_string(), "0", "100"]),
                    tag(&["o", &moderator.to_string()]),
                ];
                let parsed = RepoRef::try_from((role_event(&keys, source.clone()), None)).unwrap();
                assert_eq!(history_of(&parsed), source);
            }

            #[test]
            fn deprecated_listing_materializes_untimed_entries() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![tag(&[
                        "maintainers",
                        &author.to_string(),
                        &other.to_string(),
                    ])],
                );
                let parsed = RepoRef::try_from((event, None)).unwrap();
                assert_eq!(
                    history_of(&parsed),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &other.to_string()]),
                    ],
                );
            }

            #[test]
            fn moderator_only_role_tags_still_materialize_the_implicit_author() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let moderator = nostr::prelude::Keys::generate().public_key();
                let event = role_event(&keys, vec![tag(&["o", &moderator.to_string()])]);
                let parsed = RepoRef::try_from((event, None)).unwrap();
                // an `o`-only announcement never asserted the author as a
                // maintainer via role tags, but they are one implicitly
                assert_eq!(
                    history_of(&parsed),
                    vec![
                        tag(&["o", &moderator.to_string()]),
                        tag(&["m", &author.to_string()]),
                    ],
                );
            }

            #[test]
            fn dropping_a_deprecated_maintainer_closes_their_materialized_entry() {
                // the init republish pipeline: source history from the prior
                // deprecated-listing announcement, drop a member from the
                // typed field, and the generated role tags record the
                // removal instead of unlisting them
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let dropped = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![tag(&[
                        "maintainers",
                        &author.to_string(),
                        &dropped.to_string(),
                    ])],
                );
                let mut parsed = RepoRef::try_from((event, None)).unwrap();
                parsed.role_tags = parsed.role_history_for_republish();
                parsed.maintainers = vec![author];
                assert_eq!(
                    parsed
                        .generate_role_tags(&author, NOW)
                        .iter()
                        .map(|t| t.as_slice().to_vec())
                        .collect::<Vec<_>>(),
                    vec![
                        tag(&["m", &author.to_string()]),
                        tag(&["m", &dropped.to_string(), "0", &NOW.to_string()]),
                    ],
                );
            }
        }

        /// [`RepoRef::end_self_role`]: leaving closes every active self
        /// entry with an end boundary, removes the author from the typed
        /// membership, and the republished role tags carry the closed
        /// record instead of an active listing.
        mod end_self_role {
            use super::*;

            const NOW: u64 = 1_700_000_000;

            fn generated(repo_ref: &RepoRef, author: &PublicKey) -> Vec<Vec<String>> {
                repo_ref
                    .generate_role_tags(author, NOW)
                    .iter()
                    .map(|t| t.as_slice().to_vec())
                    .collect()
            }

            #[test]
            fn implicit_author_from_deprecated_listing_gains_a_closed_entry() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![tag(&[
                        "maintainers",
                        &author.to_string(),
                        &other.to_string(),
                    ])],
                );
                let mut parsed = RepoRef::try_from((event, None)).unwrap();
                assert!(parsed.end_self_role(&author, NOW));
                assert_eq!(parsed.maintainers, vec![other]);
                // without the closed self-entry the author would fall back
                // to being an implicit maintainer on the republished event
                assert_eq!(
                    generated(&parsed, &author),
                    vec![
                        tag(&["m", &other.to_string()]),
                        tag(&["m", &author.to_string(), "0", &NOW.to_string()]),
                    ],
                );
            }

            #[test]
            fn active_self_entry_is_closed_with_history_preserved() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![
                        tag(&["m", &author.to_string(), "100"]),
                        tag(&["m", &other.to_string()]),
                    ],
                );
                let mut parsed = RepoRef::try_from((event, None)).unwrap();
                assert!(parsed.end_self_role(&author, NOW));
                assert_eq!(parsed.maintainers, vec![other]);
                assert_eq!(
                    generated(&parsed, &author),
                    vec![
                        tag(&["m", &other.to_string()]),
                        tag(&["m", &author.to_string(), "100", &NOW.to_string()]),
                    ],
                );
            }

            #[test]
            fn leaving_lead_closes_the_m_uppercase_entry_and_clears_the_lead() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![
                        tag(&["M", &author.to_string()]),
                        tag(&["m", &other.to_string()]),
                    ],
                );
                let mut parsed = RepoRef::try_from((event, None)).unwrap();
                assert!(parsed.end_self_role(&author, NOW));
                assert_eq!(parsed.lead, None);
                assert_eq!(
                    generated(&parsed, &author),
                    vec![
                        tag(&["m", &other.to_string()]),
                        tag(&["M", &author.to_string(), "0", &NOW.to_string()]),
                    ],
                );
            }

            #[test]
            fn moderator_self_entry_is_closed_and_moderatorship_removed() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let lead = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![
                        tag(&["M", &lead.to_string()]),
                        tag(&["o", &author.to_string()]),
                    ],
                );
                let mut parsed = RepoRef::try_from((event, None)).unwrap();
                assert!(parsed.end_self_role(&author, NOW));
                assert!(parsed.moderators.is_empty());
                assert_eq!(
                    generated(&parsed, &author),
                    vec![
                        tag(&["M", &lead.to_string()]),
                        tag(&["o", &author.to_string(), "0", &NOW.to_string()]),
                    ],
                );
            }

            #[test]
            fn returns_false_when_the_author_holds_no_active_role() {
                let keys = nostr::prelude::Keys::generate();
                let author = keys.public_key();
                let other = nostr::prelude::Keys::generate().public_key();
                let event = role_event(
                    &keys,
                    vec![
                        tag(&["m", &author.to_string(), "0", "100"]),
                        tag(&["m", &other.to_string()]),
                    ],
                );
                let mut parsed = RepoRef::try_from((event, None)).unwrap();
                assert!(!parsed.end_self_role(&author, NOW));
                assert_eq!(parsed.maintainers, vec![other]);
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
            async fn maintainer_role_tags() {
                let event = create().await;
                let m_tags: Vec<&[String]> = event
                    .tags
                    .iter()
                    .map(Tag::as_slice)
                    .filter(|tag| tag.first().is_some_and(|name| name == "m"))
                    .collect();
                // first use of role tags on this announcement: one untimed
                // `m` tag per maintainer, same members as the deprecated
                // `maintainers` tag
                assert_eq!(
                    m_tags,
                    vec![
                        &["m".to_string(), TEST_KEY_1_KEYS.public_key().to_string()][..],
                        &["m".to_string(), TEST_KEY_2_KEYS.public_key().to_string()][..],
                    ],
                );
            }

            #[tokio::test]
            async fn lead_maintainer_gets_the_uppercase_m_role_tag() {
                let mut repo_ref = create_repo_ref_for_maintainer_order(
                    vec![TEST_KEY_1_KEYS.public_key(), TEST_KEY_2_KEYS.public_key()],
                    vec![],
                );
                repo_ref.lead = Some(TEST_KEY_2_KEYS.public_key());
                let event = repo_ref.to_event(&TEST_KEY_1_SIGNER).await.unwrap();
                let role_tags: Vec<&[String]> = event
                    .tags
                    .iter()
                    .map(Tag::as_slice)
                    .filter(|tag| tag.first().is_some_and(|name| name == "M" || name == "m"))
                    .collect();
                assert_eq!(
                    role_tags,
                    vec![
                        &["m".to_string(), TEST_KEY_1_KEYS.public_key().to_string()][..],
                        &["M".to_string(), TEST_KEY_2_KEYS.public_key().to_string()][..],
                    ],
                );
            }

            #[tokio::test]
            async fn no_other_tags() {
                assert_eq!(create().await.tags.len(), 11)
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
