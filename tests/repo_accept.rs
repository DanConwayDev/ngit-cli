//! `ngit repo accept` — accepting co-maintainership publishes the accepter's
//! own kind-30617 announcement, and nothing else locally. The accept-then-
//! leave flow of `ngit repo leave` is also driven here, reusing the invited
//! clone arrangement.
//!
//! The coordinate the repo resolves from is the root of trust. If accepting
//! re-rooted resolution on the accepter's own announcement — which always
//! lists them as a maintainer — the accepter could never observe the inviter
//! removing them later. Both accept paths (defaults and explicit
//! `--grasp-server`) must therefore leave `remote.origin.url` byte-for-byte
//! unchanged and `nostr.repo` unwritten. `ngit repo follow-lead` is the
//! explicit command for moving the checkout to its resolved lead coordinate.
//!
//! ## Why the announcement is asserted on the vanilla relay + grasp disk
//!
//! `ngit-grasp` routes brand-new announcements to purgatory rather than its
//! relay DB (see `tests/init_grasp.rs` for the full story), so a REQ against
//! the grasp returns nothing until git data is pushed. The observable proof
//! that the grasp accepted the announcement is the bare repository it creates
//! at `<git_data_path>/<npub>/<identifier>.git`; the vanilla "default" relay
//! (the accepter's fallback write relay) stores the event normally and lets
//! us assert on its content.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, PublishRepoOpts, PublishedRepo, Repo, tag_value, tag_values,
    tag_values_multiple,
};

fn role_entries(event: &Event, letter: &str, subject: PublicKey) -> Vec<Vec<String>> {
    let subject = subject.to_string();
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .filter(|tag| {
            tag.first().map(String::as_str) == Some(letter) && tag.get(1) == Some(&subject)
        })
        .collect()
}

/// Publish a repo with one invited (announcement-less) co-maintainer, clone
/// it, and log the clone in as that co-maintainer.
async fn arrange_invited_clone(
    identifier: &str,
) -> Result<(Harness, PublishedRepo, Repo, PublicKey)> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some(identifier.into()),
            identifier: Some(identifier.into()),
            additional_maintainer_count: 1,
            ..Default::default()
        })
        .await?;

    let co_maintainer_keys = published
        .additional_maintainer_keys
        .first()
        .context("publish_repo did not mint a co-maintainer key")?;
    let co_maintainer_pubkey = co_maintainer_keys.public_key();
    let co_maintainer_nsec = co_maintainer_keys.secret_key().to_bech32()?;

    // The invited key has never published a NIP-65 relay list. Without one
    // the accept fan-out only reaches the grasp's relay, whose purgatory is
    // not queryable (see module doc); a write relay pointing at the vanilla
    // "default" relay gives the announcement a queryable destination and
    // matches how a real logged-in user would be set up.
    let relay_url = harness.relay("default").url().to_string();
    let relay_list = RelayList::new([(RelayUrl::parse(&relay_url)?, None)])
        .finalize(co_maintainer_keys)
        .context("failed to sign co-maintainer relay list event")?;
    let nostr_client = Client::default();
    nostr_client
        .add_relay(&relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url} for relay list publish"))?;
    nostr_client.connect().await;
    let output = nostr_client
        .send_event(&relay_list)
        .to([relay_url.as_str()])
        .await
        .with_context(|| format!("failed to publish co-maintainer relay list to {relay_url}"))?;
    nostr_client.disconnect().await;
    if !output.failed.is_empty() {
        anyhow::bail!(
            "relay at {relay_url} rejected co-maintainer relay list: {:?}",
            output.failed,
        );
    }

    let clone = harness
        .clone_published_repo(&published, CloneLogin::None)
        .await
        .context("clone published repo as invited co-maintainer")?;
    clone
        .git_ok(
            ["config", "--local", "nostr.nsec", &co_maintainer_nsec],
            "git config nostr.nsec (login as invited co-maintainer)",
        )
        .await?;

    Ok((harness, published, clone, co_maintainer_pubkey))
}

async fn publish_to_relay(relay_url: &str, events: &[&Event]) -> Result<()> {
    let client = Client::default();
    client.add_relay(relay_url).await?;
    client.connect().await;
    for event in events {
        let output = client.send_event(event).to([relay_url]).await?;
        anyhow::ensure!(output.failed.is_empty(), "relay rejected event: {output:?}");
    }
    client.disconnect().await;
    Ok(())
}

/// Assert the accepter's kind-30617 landed on their fallback write relay with
/// both maintainers listed, and that the grasp accepted it (bare repo on
/// disk under the accepter's npub).
async fn assert_announcement_published(
    harness: &Harness,
    published: &PublishedRepo,
    co_maintainer_pubkey: PublicKey,
) -> Result<()> {
    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("co-maintainer kind-30617 was not published on repo accept")?;

    let maintainers = tag_values(announcement, "maintainers");
    assert!(
        maintainers.contains(&co_maintainer_pubkey.to_string()),
        "announcement should list the accepting co-maintainer; got {maintainers:?}",
    );
    assert!(
        maintainers.contains(&published.maintainer_keys.public_key().to_string()),
        "announcement should retain the inviting maintainer; got {maintainers:?}",
    );
    // NIP-34 graceful degradation: active lead and co-maintainer roles carry
    // exactly the same current members as the deprecated compatibility tag.
    let roles = [
        tag_values_multiple(announcement, "m"),
        tag_values_multiple(announcement, "M"),
    ]
    .concat();
    assert_eq!(
        roles, maintainers,
        "active `M`/`m` roles should match the deprecated `maintainers` tag",
    );

    let bare_repo = harness
        .grasp("repo")
        .git_data_path()
        .join(co_maintainer_pubkey.to_bech32()?)
        .join(format!("{}.git", published.identifier));
    assert!(
        bare_repo.is_dir(),
        "expected the grasp to accept the announcement and create {}",
        bare_repo.display(),
    );

    Ok(())
}

/// Run `ngit repo accept <extra_args>` in `clone` and assert that origin and
/// `nostr.repo` still resolve from the inviter's coordinate afterwards.
async fn accept_and_assert_resolution_untouched(clone: &Repo, extra_args: &[&str]) -> Result<()> {
    let origin_before = clone
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing before repo accept")?;
    assert!(
        clone.config("nostr.repo").await?.is_none(),
        "precondition: a fresh clone should have no nostr.repo config",
    );

    let mut args = vec!["repo", "accept"];
    args.extend_from_slice(extra_args);
    let out = clone
        .ngit(&args)
        .output()
        .await
        .context("failed to spawn ngit repo accept")?;
    assert!(
        out.status.success(),
        "ngit repo accept exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let origin_after = clone
        .config("remote.origin.url")
        .await?
        .context("remote.origin.url missing after repo accept")?;
    assert_eq!(
        origin_after, origin_before,
        "repo accept must leave the origin remote untouched so resolution \
         stays rooted on the inviter's coordinate",
    );
    assert_eq!(
        clone.config("nostr.repo").await?,
        None,
        "repo accept must not write nostr.repo; re-rooting resolution on the \
         accepter's own coordinate would hide a later removal",
    );

    let info = clone
        .ngit(["repo", "--json", "--offline"])
        .output()
        .await
        .context("failed to inspect accepted repository as JSON")?;
    assert!(info.status.success(), "ngit repo --json failed");
    let json: serde_json::Value = serde_json::from_slice(&info.stdout)?;
    assert_eq!(json["maintainers"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        json["confirmed_maintainers"].as_array().map(Vec::len),
        Some(2),
        "reciprocal acceptance should confirm both maintainers: {json}",
    );
    assert_eq!(
        json["invited_maintainers"],
        serde_json::json!([]),
        "accepted maintainers must no longer be framed as invited: {json}",
    );
    assert_eq!(json["maintainer_edges"].as_array().map(Vec::len), Some(2));
    assert!(json["selected_maintainer"].is_string());
    assert!(json.get("lead_maintainer").is_some());
    assert_eq!(json["lead_source"], "explicit");
    assert_eq!(json["lead_path"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["pending_actions"], serde_json::json!([]));
    assert_eq!(json["health"]["status"], "ok");
    assert_eq!(json["moderators"], serde_json::json!([]));
    assert_eq!(json["confirmed_moderators"], serde_json::json!([]));
    let members = json["members"]
        .as_array()
        .context("members missing from ngit repo --json")?;
    assert_eq!(members.len(), 2, "one member entry per maintainer: {json}");
    let lead = json["lead_maintainer"]
        .as_str()
        .context("first invitation should establish the inviter as lead")?;
    for member in members {
        assert!(member["pubkey"].is_string());
        assert_eq!(
            member["role"],
            if member["pubkey"] == lead {
                "lead"
            } else {
                "co-maintainer"
            },
            "the first inviter should be the sole lead: {json}",
        );
        assert_eq!(
            member["status"], "confirmed",
            "reciprocal acceptance should confirm both members: {json}",
        );
        assert_eq!(
            member["source"], "role_tag",
            "both announcements were published with indexed role tags: {json}",
        );
    }

    Ok(())
}

#[tokio::test]
async fn accept_with_defaults_publishes_announcement_without_rerooting_resolution() -> Result<()> {
    let (harness, published, clone, co_maintainer_pubkey) =
        arrange_invited_clone("repo-accept-defaults").await?;

    accept_and_assert_resolution_untouched(&clone, &[]).await?;
    assert_announcement_published(&harness, &published, co_maintainer_pubkey).await?;

    Ok(())
}

#[tokio::test]
async fn accept_refuses_preexisting_divergent_state_without_republishing() -> Result<()> {
    let (harness, published, clone, co_maintainer_pubkey) =
        arrange_invited_clone("repo-accept-state-collision").await?;
    let co_keys = published.additional_maintainer_keys[0].clone();
    let lead = published.maintainer_keys.public_key();
    let now = Timestamp::now();
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([
            Tag::identifier(published.identifier.clone()),
            Tag::parse(["M", &lead.to_string(), &now.as_secs().to_string()])?,
            Tag::parse([
                "m",
                &co_maintainer_pubkey.to_string(),
                &now.as_secs().to_string(),
            ])?,
            Tag::parse([
                "maintainers",
                &lead.to_string(),
                &co_maintainer_pubkey.to_string(),
            ])?,
        ])
        .finalize(&co_keys)?;
    let state = EventBuilder::new(Kind::Custom(30618), "")
        .tags([
            Tag::identifier(published.identifier.clone()),
            Tag::parse([
                "refs/heads/experiment",
                "1111111111111111111111111111111111111111",
            ])?,
        ])
        .finalize(&co_keys)?;
    publish_to_relay(harness.relay("default").url(), &[&announcement, &state]).await?;

    let output = clone.ngit(["repo", "accept"]).output().await?;
    assert!(
        !output.status.success(),
        "divergent state must block acceptance"
    );

    let surviving = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement)
                .identifier(published.identifier),
        )
        .await?;
    assert_eq!(
        surviving.iter().map(|event| event.id).collect::<Vec<_>>(),
        vec![announcement.id],
        "a refused acceptance must not replace the existing announcement",
    );
    Ok(())
}

/// After accepting, `ngit repo leave` republishes the leaver's announcement
/// with the self-role ended per NIP-34's role-history grammar: the `m` entry
/// gains an end boundary instead of vanishing from the event, and the
/// deprecated `maintainers` degradation tag drops the leaver. A second leave
/// has no active self-role left to end and must fail cleanly.
#[tokio::test]
async fn leave_after_accept_ends_the_self_role_with_a_boundary() -> Result<()> {
    let (harness, published, clone, co_maintainer_pubkey) =
        arrange_invited_clone("repo-leave-after-accept").await?;

    accept_and_assert_resolution_untouched(&clone, &[]).await?;
    assert_announcement_published(&harness, &published, co_maintainer_pubkey).await?;

    let out = clone
        .ngit(["repo", "leave"])
        .output()
        .await
        .context("failed to spawn ngit repo leave")?;
    assert!(
        out.status.success(),
        "ngit repo leave exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    // the NIP-01 winner in case the relay retained the pre-leave version
    let announcement = announcements
        .iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
        .context("no co-maintainer announcement found after repo leave")?;

    let maintainers = tag_values(announcement, "maintainers");
    assert!(
        !maintainers.contains(&co_maintainer_pubkey.to_string()),
        "the deprecated maintainers tag must drop the leaver; got {maintainers:?}",
    );
    let self_entries: Vec<Vec<String>> = announcement
        .tags
        .iter()
        .map(|t| t.as_slice().to_vec())
        .filter(|s| {
            matches!(s.first().map(String::as_str), Some("M" | "m"))
                && s.get(1) == Some(&co_maintainer_pubkey.to_string())
        })
        .collect();
    assert_eq!(
        self_entries.len(),
        1,
        "leaving must keep exactly one closed self role entry; got {self_entries:?}",
    );
    let entry = &self_entries[0];
    assert!(
        entry.len() >= 4 && entry.len().is_multiple_of(2),
        "the self role entry must be ended (even element count of at least four): {entry:?}",
    );

    let again = clone
        .ngit(["repo", "leave"])
        .output()
        .await
        .context("failed to spawn second ngit repo leave")?;
    assert!(
        !again.status.success(),
        "a second leave must fail: the announcement already records the role as ended",
    );
    // the refused leave must not have published anything: the NIP-01
    // winner on the relay is still the first leave's announcement
    let announcements_after = harness
        .relay("default")
        .events(
            Filter::new()
                .author(co_maintainer_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let winner_after = announcements_after
        .iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
        .context("co-maintainer announcement vanished after refused second leave")?;
    assert_eq!(
        winner_after.id, announcement.id,
        "a refused leave must not publish a new announcement",
    );

    Ok(())
}

/// Under a wire-asserted lead, a non-lead accepter follows NIP-34's
/// SHOULD: their acceptance announcement actively lists only themselves
/// (`m`) and the lead (re-asserted as `M`). Other roster history is retained
/// with `defer`, so the lead can change or remove co-maintainers
/// unilaterally.
#[tokio::test]
async fn accept_under_lead_activates_self_and_lead_and_defers_others() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (_maintainer_repo, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("repo-accept-under-lead".into()),
            identifier: Some("repo-accept-under-lead".into()),
            additional_maintainer_count: 2,
            assert_self_as_lead: true,
            ..Default::default()
        })
        .await?;
    let lead_pubkey = published.maintainer_keys.public_key();
    let other_co_pubkey = published.additional_maintainer_keys[0].public_key();
    let invited_keys = published.additional_maintainer_keys[1].clone();
    let invited_pubkey = invited_keys.public_key();

    // Write relay for the accept fan-out, same as arrange_invited_clone.
    harness.publish_user_relay_list(&invited_keys).await?;
    let clone = harness
        .clone_published_repo_as(&published, &invited_keys)
        .await?;

    let out = clone
        .ngit(["repo", "accept"])
        .output()
        .await
        .context("failed to spawn ngit repo accept")?;
    assert!(
        out.status.success(),
        "ngit repo accept exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(invited_pubkey)
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    let announcement = announcements
        .iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(published.identifier.as_str()))
        .context("no acceptance announcement was published on repo accept")?;

    let lead_entries = role_entries(announcement, "M", lead_pubkey);
    assert_eq!(lead_entries.len(), 1);
    assert_eq!(lead_entries[0].len(), 3, "the lead role must be active");
    assert!(lead_entries[0][2].parse::<u64>().is_ok());

    let self_entries = role_entries(announcement, "m", invited_pubkey);
    assert_eq!(self_entries.len(), 1);
    assert_eq!(self_entries[0].len(), 3, "the self role must be active");
    assert!(self_entries[0][2].parse::<u64>().is_ok());

    let other_entries = role_entries(announcement, "m", other_co_pubkey);
    assert_eq!(other_entries.len(), 1);
    assert_eq!(other_entries[0].last().map(String::as_str), Some("defer"));
    let maintainers = tag_values(announcement, "maintainers");
    assert_eq!(
        {
            let mut sorted = maintainers.clone();
            sorted.sort();
            sorted
        },
        {
            let mut expected = vec![invited_pubkey.to_string(), lead_pubkey.to_string()];
            expected.sort();
            expected
        },
        "the degradation tag carries exactly [me, lead]",
    );
    Ok(())
}

#[tokio::test]
async fn accept_with_grasp_server_flag_publishes_announcement_without_rerooting_resolution()
-> Result<()> {
    let (harness, published, clone, co_maintainer_pubkey) =
        arrange_invited_clone("repo-accept-grasp-flag").await?;

    let grasp_url = harness.grasp("repo").url().to_string();
    accept_and_assert_resolution_untouched(&clone, &["--grasp-server", &grasp_url]).await?;
    assert_announcement_published(&harness, &published, co_maintainer_pubkey).await?;

    Ok(())
}
