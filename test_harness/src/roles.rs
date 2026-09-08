//! NIP-34 role-tag announcement fixtures.
//!
//! ngit's own emission paths only produce a narrow band of the role-tag
//! grammar (`ngit init` / `ngit repo accept` emit the typed maintainer set;
//! ngit never assigns moderators or fabricates history boundaries). The
//! integration tests for the indexed maintainer-role model therefore need to
//! *fabricate* kind-30617 announcements with arbitrary `M`/`m`/`o` role tags
//! — assignment graphs, acknowledgements, ended entries, deprecated
//! `maintainers`-only listings — and publish them where ngit's discovery
//! will find them.
//!
//! Three layers:
//!
//! - [`RoleEntry`] — one role tag (`["M"|"m"|"o", <pubkey-hex>,
//!   <boundaries>...]`), with constructors for the common shapes.
//! - [`Harness::publish_fabricated_announcement`] /
//!   [`Harness::republish_announcement_with_roles`] — sign and publish a
//!   kind-30617 carrying exactly the requested role tags, either built from
//!   scratch or by amending an existing announcement's non-role tags.
//! - [`Harness::publish_repo_with_role_graph`] — one-call scenario: a real
//!   grasp-hosted repository whose announcement graph carries a lead (`M`), an
//!   accepted co-maintainer (`m` + reciprocal announcement), an acknowledged
//!   moderator (`o` + self-`o` acknowledgement) and an
//!   assigned-but-unacknowledged moderator.
//!
//! ## Discovery contract
//!
//! Fabricated member announcements are published to the harness's vanilla
//! `"default"` relay. For ngit to discover them, that relay must be in the
//! repository's relay set — [`Harness::publish_repo_with_role_graph`]
//! arranges this via [`PublishRepoOpts::extra_repo_relays`], and tests that
//! compose the lower-level helpers themselves must do the same (or publish
//! to a relay the announcement already lists). Publishing member
//! announcements to a grasp is deliberately avoided: ngit-grasp routes
//! announcements from npubs without git data into purgatory, where no REQ
//! can see them (see `tests/repo_accept.rs`'s module doc).

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;

use crate::{
    harness::Harness,
    repo::Repo,
    scenarios::{CloneLogin, PublishRepoOpts, PublishedRepo},
};

/// Which NIP-34 role letter a [`RoleEntry`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleLetter {
    /// `M` — lead maintainer.
    Lead,
    /// `m` — co-maintainer.
    CoMaintainer,
    /// `o` — moderator.
    Moderator,
}

impl RoleLetter {
    fn tag_name(self) -> &'static str {
        match self {
            RoleLetter::Lead => "M",
            RoleLetter::CoMaintainer => "m",
            RoleLetter::Moderator => "o",
        }
    }
}

/// One indexed role tag on a fabricated kind-30617:
/// `["M"|"m"|"o", <pubkey-hex>, <alternating start/end unix timestamps>...]`.
///
/// Per NIP-34 an entry is currently **active** when the tag has fewer than
/// four elements or a numeric history ends with a start boundary. A final
/// `defer` is an inactive historical copy. The constructors cover the common
/// shapes; arbitrary histories go through [`RoleEntry::with_boundaries`] or
/// [`RoleEntry::with_role_boundaries`].
#[derive(Clone, Debug)]
pub struct RoleEntry {
    pub letter: RoleLetter,
    pub pubkey: PublicKey,
    /// Alternating start/end boundaries, rendered verbatim after the pubkey
    /// slot. Empty means "active from the beginning" (untimed).
    pub boundaries: Vec<RoleBoundary>,
}

/// One fabricated role-history boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleBoundary {
    Timestamp(u64),
    /// Historical-only interval whose current assignment is deferred to
    /// another announcement.
    Defer,
}

impl std::fmt::Display for RoleBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timestamp(value) => value.fmt(f),
            Self::Defer => f.write_str("defer"),
        }
    }
}

impl RoleEntry {
    /// Untimed `M` entry — lead from the beginning, still active.
    pub fn lead(pubkey: PublicKey) -> Self {
        Self {
            letter: RoleLetter::Lead,
            pubkey,
            boundaries: vec![],
        }
    }

    /// Untimed `m` entry — co-maintainer from the beginning, still active.
    pub fn co_maintainer(pubkey: PublicKey) -> Self {
        Self {
            letter: RoleLetter::CoMaintainer,
            pubkey,
            boundaries: vec![],
        }
    }

    /// Untimed `o` entry — moderator from the beginning, still active.
    pub fn moderator(pubkey: PublicKey) -> Self {
        Self {
            letter: RoleLetter::Moderator,
            pubkey,
            boundaries: vec![],
        }
    }

    /// Replace the entry's history boundaries — e.g.
    /// `.with_boundaries(vec![0, ended_at])` for a founding member who was
    /// removed, or `vec![start, end, second_start]` for a returning one.
    pub fn with_boundaries(mut self, boundaries: Vec<u64>) -> Self {
        self.boundaries = boundaries
            .into_iter()
            .map(RoleBoundary::Timestamp)
            .collect();
        self
    }

    /// Retain one historical interval without creating a current assignment.
    pub fn with_deferred_interval(mut self, start: u64) -> Self {
        self.boundaries = vec![RoleBoundary::Timestamp(start), RoleBoundary::Defer];
        self
    }

    /// Replace the complete boundary sequence, including an optional final
    /// [`RoleBoundary::Defer`].
    pub fn with_role_boundaries(mut self, boundaries: Vec<RoleBoundary>) -> Self {
        self.boundaries = boundaries;
        self
    }

    fn to_tag(&self) -> Tag {
        let mut values: Vec<String> = vec![self.pubkey.to_string()];
        values.extend(self.boundaries.iter().map(ToString::to_string));
        Tag::custom(self.letter.tag_name(), values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_interval_renders_the_literal_boundary() {
        let pubkey = Keys::generate().public_key();
        assert_eq!(
            RoleEntry::co_maintainer(pubkey)
                .with_deferred_interval(123)
                .to_tag()
                .as_slice(),
            ["m", &pubkey.to_string(), "123", "defer"],
        );
    }
}

/// Knobs for [`Harness::publish_fabricated_announcement`].
#[derive(Clone, Debug)]
pub struct FabricateAnnouncementOpts {
    /// `d` tag value. Mandatory — every consumer matches on it.
    pub identifier: String,
    /// Indexed role tags, emitted in order. May be empty for a deprecated
    /// `maintainers`-tag-only announcement (pre-role-tag shape).
    pub roles: Vec<RoleEntry>,
    /// Deprecated `maintainers` tag values. `None` omits the tag entirely.
    /// NIP-34's graceful-degradation rule says a role-tagged announcement
    /// MAY carry it listing the current `M`/`m` members; a role-tag-free
    /// announcement uses it as the sole membership source.
    pub maintainers_tag: Option<Vec<PublicKey>>,
    /// `relays` tag values. `None` defaults to the harness's `"default"`
    /// vanilla relay — the same place the event is published, so ngit's
    /// member-announcement discovery finds companion events.
    pub relays: Option<Vec<String>>,
    /// `clone` tag values. Empty omits the tag. Tests that drive `ngit
    /// init` against the fabricated announcement should supply an (inert)
    /// URL so init inherits git-server infrastructure instead of demanding
    /// `--grasp-server`.
    pub clone_urls: Vec<String>,
    /// `name` tag value. `None` omits the tag.
    pub name: Option<String>,
    /// Additional tags emitted verbatim after the fixture's typed fields.
    pub extra_tags: Vec<Tag>,
    /// `["r", <oid>, "euc"]` earliest-unique-commit marker. `None` omits.
    pub euc: Option<String>,
    /// Explicit `created_at`. `None` uses `Timestamp::now()`. Tests that
    /// need ngit's subsequent republish to win NIP-01 replacement should
    /// back-date (the established fixture convention is 30s).
    pub created_at: Option<Timestamp>,
    /// Relay URLs to publish to. `None` publishes to the `"default"`
    /// vanilla relay only.
    pub publish_to: Option<Vec<String>>,
}

impl FabricateAnnouncementOpts {
    /// Minimal announcement for `identifier` carrying `roles` and nothing
    /// else beyond the defaults documented on each field.
    pub fn new(identifier: impl Into<String>, roles: Vec<RoleEntry>) -> Self {
        Self {
            identifier: identifier.into(),
            roles,
            maintainers_tag: None,
            relays: None,
            clone_urls: vec![],
            name: None,
            extra_tags: vec![],
            euc: None,
            created_at: None,
            publish_to: None,
        }
    }
}

/// A grasp-hosted repository whose announcement graph exercises every
/// member role: produced by [`Harness::publish_repo_with_role_graph`].
///
/// The wire shape after the fixture completes:
///
/// - **lead** ([`PublishedRepo::maintainer_keys`]): real `ngit init
///   --lead-maintainer <self>` announcement, then a fabricated replacement
///   adding `["m", co]`, `["o", moderator]`, `["o", unacknowledged]` and a
///   degradation `maintainers` tag of `[lead, co]`.
/// - **co-maintainer**: fabricated reciprocal announcement listing themselves
///   (`m`) and the lead (`M`) — an accepted, confirmed member.
/// - **moderator**: fabricated acknowledgement listing themselves (`o`) and the
///   lead (`M`) — a confirmed moderator.
/// - **unacknowledged moderator**: assigned `o` in the lead's announcement, no
///   announcement of their own — an invited moderator.
///
/// All fabricated announcements live on the `"default"` vanilla relay,
/// which the repository's relay set includes.
#[derive(Clone, Debug)]
pub struct RoleGraph {
    /// The underlying published repository. The lead's keys are
    /// [`PublishedRepo::maintainer_keys`].
    pub published: PublishedRepo,
    /// Accepted co-maintainer.
    pub co_maintainer_keys: Keys,
    /// Bech32 nsec for the co-maintainer, ready for `nostr.nsec`.
    pub co_maintainer_nsec: String,
    /// Acknowledged moderator.
    pub moderator_keys: Keys,
    /// Bech32 nsec for the moderator.
    pub moderator_nsec: String,
    /// Assigned-but-unacknowledged moderator. No announcement exists for
    /// this key; its nsec is derivable via `secret_key().to_bech32()` for
    /// tests that drive ngit as this identity.
    pub unacknowledged_moderator_keys: Keys,
    /// The lead's fabricated replacement announcement (the role
    /// assignments).
    pub lead_announcement: Event,
    /// The co-maintainer's reciprocal announcement.
    pub co_maintainer_announcement: Event,
    /// The moderator's acknowledgement announcement.
    pub moderator_announcement: Event,
}

impl Harness {
    /// Sign and publish a kind-30617 announcement carrying exactly the
    /// requested role tags. See [`FabricateAnnouncementOpts`] for the
    /// defaults; the returned event is the signed announcement as stored.
    pub async fn publish_fabricated_announcement(
        &self,
        signer: &Keys,
        opts: FabricateAnnouncementOpts,
    ) -> Result<Event> {
        let default_relay_url = self.relay("default").url().to_string();
        let relays = opts
            .relays
            .unwrap_or_else(|| vec![default_relay_url.clone()]);
        let publish_to = opts
            .publish_to
            .unwrap_or_else(|| vec![default_relay_url.clone()]);

        let mut tags: Vec<Tag> = vec![Tag::identifier(opts.identifier.clone())];
        if let Some(name) = &opts.name {
            tags.push(Tag::custom("name", vec![name.clone()]));
        }
        if let Some(euc) = &opts.euc {
            tags.push(Tag::custom("r", vec![euc.clone(), "euc".to_string()]));
        }
        if !opts.clone_urls.is_empty() {
            tags.push(Tag::custom("clone", opts.clone_urls.clone()));
        }
        tags.push(Tag::custom("relays", relays));
        for role in &opts.roles {
            tags.push(role.to_tag());
        }
        if let Some(maintainers) = &opts.maintainers_tag {
            tags.push(Tag::custom(
                "maintainers",
                maintainers
                    .iter()
                    .map(PublicKey::to_string)
                    .collect::<Vec<String>>(),
            ));
        }
        tags.extend(opts.extra_tags);

        let created_at = opts.created_at.unwrap_or_else(Timestamp::now);
        let event = EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .custom_created_at(created_at)
            .finalize(signer)
            .context("failed to sign fabricated role-tag announcement")?;

        publish_event_to_relays(&publish_to, &event)
            .await
            .context("failed to publish fabricated role-tag announcement")?;
        Ok(event)
    }

    /// Re-sign `base` (an existing kind-30617 by `signer`) with its
    /// `M`/`m`/`o`/`maintainers` tags replaced by `roles` /
    /// `maintainers_tag`, all other tags copied verbatim, and a
    /// `created_at` strictly greater than `base`'s so the replacement wins
    /// NIP-01 selection without sleeping.
    ///
    /// This is how a test grafts moderator assignments (which ngit has no
    /// flow to emit) onto a real `ngit init`-produced announcement while
    /// keeping its clone/relay infrastructure intact.
    pub async fn republish_announcement_with_roles(
        &self,
        signer: &Keys,
        base: &Event,
        roles: Vec<RoleEntry>,
        maintainers_tag: Option<Vec<PublicKey>>,
        publish_to: Vec<String>,
    ) -> Result<Event> {
        if base.pubkey != signer.public_key() {
            bail!(
                "republish_announcement_with_roles: base announcement author {} does not match \
                 signer {} — the replacement would not supersede it",
                base.pubkey,
                signer.public_key(),
            );
        }
        let mut tags: Vec<Tag> = base
            .tags
            .iter()
            .filter(|tag| {
                !matches!(
                    tag.as_slice().first().map(String::as_str),
                    Some("M" | "m" | "o" | "maintainers")
                )
            })
            .cloned()
            .collect();
        for role in &roles {
            tags.push(role.to_tag());
        }
        if let Some(maintainers) = &maintainers_tag {
            tags.push(Tag::custom(
                "maintainers",
                maintainers
                    .iter()
                    .map(PublicKey::to_string)
                    .collect::<Vec<String>>(),
            ));
        }

        // Strictly newer than `base` even when both land in the same
        // wall-clock second; never in the past relative to now.
        let created_at = Timestamp::from_secs(std::cmp::max(
            Timestamp::now().as_secs(),
            base.created_at
                .as_secs()
                .checked_add(1)
                .context("announcement timestamp overflow")?,
        ));
        let event = EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags(tags)
            .custom_created_at(created_at)
            .finalize(signer)
            .context("failed to sign role-amended announcement")?;

        publish_event_to_relays(&publish_to, &event)
            .await
            .context("failed to publish role-amended announcement")?;
        Ok(event)
    }

    /// Publish a NIP-65 relay list (kind 10002) for `keys` naming the
    /// harness's `"default"` vanilla relay as read+write.
    ///
    /// Fabricated identities have never logged in, so without this ngit's
    /// event fan-out for them has no user write relay and only reaches the
    /// repository relays. Publishing one models a realistically set-up
    /// account (`tests/repo_accept.rs` established the pattern).
    pub async fn publish_user_relay_list(&self, keys: &Keys) -> Result<()> {
        let relay_url = self.relay("default").url().to_string();
        let relay_list = RelayList::new([(RelayUrl::parse(&relay_url)?, None)])
            .finalize(keys)
            .context("failed to sign fabricated relay list event")?;
        publish_event_to_relays(&[relay_url], &relay_list)
            .await
            .context("failed to publish fabricated relay list")
    }

    /// [`Harness::clone_published_repo`] and then log the clone in as
    /// `keys` by writing `nostr.nsec` into its local git config.
    ///
    /// Writing the config key directly (rather than `ngit account login`)
    /// skips the login flow's network round-trips; ngit reads the key the
    /// same way either path stores it. Matches `tests/repo_accept.rs`.
    pub async fn clone_published_repo_as(
        &self,
        published: &PublishedRepo,
        keys: &Keys,
    ) -> Result<Repo> {
        let clone = self
            .clone_published_repo(published, CloneLogin::None)
            .await?;
        let nsec = keys
            .secret_key()
            .to_bech32()
            .context("failed to bech32-encode fixture nsec")?;
        clone
            .git_ok(
                ["config", "--local", "nostr.nsec", &nsec],
                "git config nostr.nsec (login clone as fixture identity)",
            )
            .await?;
        Ok(clone)
    }

    /// One-call role-graph scenario: publish a real grasp-hosted repo as a
    /// self-asserted lead, then fabricate the announcements described on
    /// [`RoleGraph`]. Returns the lead's working tree plus the graph.
    ///
    /// Requires `with_relay("default")` and `with_grasp_server("repo")` on
    /// the harness builder. The repository's relay set is `[grasp,
    /// default]` so every fabricated member announcement (published to the
    /// default relay) is discoverable from the repository's coordinate.
    pub async fn publish_repo_with_role_graph(
        &self,
        identifier: &str,
    ) -> Result<(Repo, RoleGraph)> {
        let default_relay_url = self.relay("default").url().to_string();
        let (lead_repo, published) = self
            .publish_repo(PublishRepoOpts {
                display_name: Some(identifier.to_string()),
                identifier: Some(identifier.to_string()),
                extra_repo_relays: vec![default_relay_url.clone()],
                assert_self_as_lead: true,
                ..Default::default()
            })
            .await?;
        let lead_pubkey = published.maintainer_keys.public_key();

        // The init-produced announcement is the base whose infrastructure
        // (clone/relays/euc) the fabricated replacement must keep. Post-push
        // it has graduated out of grasp purgatory, so a REQ sees it.
        let base = self
            .grasp("repo")
            .events(
                Filter::new()
                    .kind(Kind::GitRepoAnnouncement)
                    .author(lead_pubkey)
                    .identifier(identifier.to_string()),
            )
            .await?
            .into_iter()
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            })
            .context("no lead announcement on grasp after publish_repo — did the push graduate?")?;

        let co_maintainer_keys = Keys::generate();
        let moderator_keys = Keys::generate();
        let unacknowledged_moderator_keys = Keys::generate();
        let co_pubkey = co_maintainer_keys.public_key();
        let moderator_pubkey = moderator_keys.public_key();

        // Lead's replacement: assign every role. Published to the default
        // relay and the grasp relay — the grasp accepts announcement
        // updates for repositories whose git data already exists.
        let lead_announcement = self
            .republish_announcement_with_roles(
                &published.maintainer_keys,
                &base,
                vec![
                    RoleEntry::lead(lead_pubkey),
                    RoleEntry::co_maintainer(co_pubkey),
                    RoleEntry::moderator(moderator_pubkey),
                    RoleEntry::moderator(unacknowledged_moderator_keys.public_key()),
                ],
                Some(vec![lead_pubkey, co_pubkey]),
                vec![default_relay_url.clone(), self.grasp("repo").relay_url()],
            )
            .await
            .context("failed to publish the lead's role-assignment announcement")?;

        // Reciprocal announcements: per NIP-34, under a lead the non-lead
        // members list only themselves and the lead.
        let member_relays = vec![self.grasp("repo").relay_url(), default_relay_url.clone()];
        let co_maintainer_announcement = self
            .publish_fabricated_announcement(
                &co_maintainer_keys,
                FabricateAnnouncementOpts {
                    maintainers_tag: Some(vec![co_pubkey, lead_pubkey]),
                    relays: Some(member_relays.clone()),
                    ..FabricateAnnouncementOpts::new(
                        identifier,
                        vec![
                            RoleEntry::lead(lead_pubkey),
                            RoleEntry::co_maintainer(co_pubkey),
                        ],
                    )
                },
            )
            .await
            .context("failed to publish the co-maintainer's acceptance announcement")?;
        let moderator_announcement = self
            .publish_fabricated_announcement(
                &moderator_keys,
                FabricateAnnouncementOpts {
                    relays: Some(member_relays),
                    ..FabricateAnnouncementOpts::new(
                        identifier,
                        vec![
                            RoleEntry::lead(lead_pubkey),
                            RoleEntry::moderator(moderator_pubkey),
                        ],
                    )
                },
            )
            .await
            .context("failed to publish the moderator's acknowledgement announcement")?;

        // Give the fabricated identities a user write relay so ngit
        // commands run as them fan out somewhere queryable.
        self.publish_user_relay_list(&co_maintainer_keys).await?;
        self.publish_user_relay_list(&moderator_keys).await?;

        let co_maintainer_nsec = co_maintainer_keys
            .secret_key()
            .to_bech32()
            .context("failed to bech32-encode co-maintainer nsec")?;
        let moderator_nsec = moderator_keys
            .secret_key()
            .to_bech32()
            .context("failed to bech32-encode moderator nsec")?;

        Ok((
            lead_repo,
            RoleGraph {
                published,
                co_maintainer_keys,
                co_maintainer_nsec,
                moderator_keys,
                moderator_nsec,
                unacknowledged_moderator_keys,
                lead_announcement,
                co_maintainer_announcement,
                moderator_announcement,
            },
        ))
    }
}

/// Publish `event` to every relay in `urls`, bailing when any of them
/// rejects it — a silent partial failure would surface later as a baffling
/// discovery miss in the consuming test.
async fn publish_event_to_relays(urls: &[String], event: &Event) -> Result<()> {
    let client = Client::default();
    for url in urls {
        client
            .add_relay(url)
            .await
            .with_context(|| format!("failed to add relay {url} for fixture publish"))?;
    }
    client.connect().await;
    let output = client
        .send_event(event)
        .to(urls.iter().map(String::as_str))
        .await
        .with_context(|| format!("failed to publish fixture event to {urls:?}"))?;
    client.disconnect().await;
    if !output.failed.is_empty() {
        bail!(
            "relay(s) rejected fixture event id={}: {:?}",
            event.id,
            output.failed,
        );
    }
    Ok(())
}
