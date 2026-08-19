//! `ngit init --lead-maintainer <someone else>` — the NIP-34 listing
//! collapse and its `--force` gate, end-to-end against fabricated
//! announcements.
//!
//! Designating another pubkey as lead follows NIP-34's SHOULD: the
//! author's announcement then lists only themselves and the lead. When the
//! collapse would drop a pubkey the author's current announcement lists
//! without authoritative cover from the lead's own announcement, that
//! pubkey would lose authorized-maintainer status and init must demand
//! `--force` (`apply_lead_to_maintainers` in
//! `src/bin/ngit/sub_commands/init.rs`; the unit tests there pin the
//! decision table, these tests pin the wire round-trip).
//!
//! The arranged "my announcement" deliberately predates indexed role tags
//! (deprecated `maintainers` tag only), so the forced republish also
//! exercises history materialization: the dropped member must be closed
//! with an end boundary (`["m", <pk>, "0", <end>]`) rather than silently
//! unlisted, while continuing members stay untimed.
//!
//! Same error-message-substring caveat as `tests/init_state_fresh.rs`:
//! asserting on a stable stderr fragment is the tolerated shortcut for
//! tests whose contract *is* "this validation fired".

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{FabricateAnnouncementOpts, Harness, tag_value, tag_values};

/// The fabricated announcement's git server is unreachable, so a
/// *successful* republish still exits non-zero at the post-publish push
/// step. Mirror `tests/init_state_my_announcement.rs`: the invocation
/// counts as a publish success iff the announcement-published-but-push-
/// failed error is reported.
fn expect_announcement_published_but_push_failed(
    invocation: &str,
    out: &std::process::Output,
) -> Result<()> {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        bail!(
            "{invocation} exited zero despite its git servers being unreachable — \
             the push failure should be a real error\nstdout+stderr: {combined}"
        );
    }
    if !combined.contains("was published to nostr but pushing your git data failed") {
        bail!(
            "{invocation} failed for an unexpected reason (wanted the \
             announcement-published-but-push-failed error)\nstdout+stderr: {combined}"
        );
    }
    Ok(())
}

/// The NIP-01 winner among `author`'s kind-30617s for `identifier` on the
/// default relay.
async fn latest_announcement(
    harness: &Harness,
    author: PublicKey,
    identifier: &str,
) -> Result<Event> {
    harness
        .relay("default")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .filter(|event| tag_value(event, "d").as_deref() == Some(identifier))
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        })
        .context("no announcement found on the default relay")
}

/// All role tags of `letter` naming `pubkey`, as raw slices.
fn role_entries(event: &Event, letter: &str, pubkey: &PublicKey) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|t| t.as_slice().to_vec())
        .filter(|s| {
            s.first().map(String::as_str) == Some(letter) && s.get(1) == Some(&pubkey.to_string())
        })
        .collect()
}

/// State-B repo plus a back-dated, deprecated-`maintainers`-only
/// announcement of mine listing `[me, lead, third]`. Returns
/// `(repo, my_keys, identifier, existing_event, lead_keys, third_keys)`.
async fn arrange_three_member_announcement(
    harness: &Harness,
) -> Result<(test_harness::Repo, Keys, String, Event, Keys, Keys)> {
    let (repo, state_b) = harness.arrange_init_state_b_coordinate_only().await?;
    let lead_keys = Keys::generate();
    let third_keys = Keys::generate();

    let existing = harness
        .publish_fabricated_announcement(
            &state_b.keys,
            FabricateAnnouncementOpts {
                maintainers_tag: Some(vec![
                    state_b.keys.public_key(),
                    lead_keys.public_key(),
                    third_keys.public_key(),
                ]),
                // Inert git server so init inherits infrastructure instead
                // of demanding --grasp-server. The post-publish push then
                // fails against it, which
                // expect_announcement_published_but_push_failed treats as
                // the publish-success signal.
                clone_urls: vec!["https://ngit-test-clone.invalid/repo.git".to_string()],
                name: Some("example name".to_string()),
                euc: Some(state_b.root_oid.clone()),
                // Back-date so ngit's republish unambiguously wins NIP-01
                // replacement (established fixture convention).
                created_at: Some(Timestamp::now() - 30u64),
                ..FabricateAnnouncementOpts::new(state_b.coordinate_identifier.clone(), vec![])
            },
        )
        .await?;

    Ok((
        repo,
        state_b.keys,
        state_b.coordinate_identifier,
        existing,
        lead_keys,
        third_keys,
    ))
}

#[tokio::test]
async fn collapse_without_lead_cover_requires_force_and_closes_dropped_member() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, my_keys, identifier, existing, lead_keys, third_keys) =
        arrange_three_member_announcement(&harness).await?;
    let me = my_keys.public_key();
    let lead = lead_keys.public_key();
    let third = third_keys.public_key();
    let lead_npub = lead.to_bech32()?;
    let third_npub = third.to_bech32()?;

    // The lead has no announcement, so their listing covers nobody:
    // dropping `third` needs --force.
    let out = repo
        .ngit(["init", "--lead-maintainer", &lead_npub])
        .output()
        .await
        .context("failed to spawn ngit init --lead-maintainer")?;
    assert!(
        !out.status.success(),
        "collapse dropping an uncovered maintainer must require --force\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("authorized-maintainer status"),
        "the error should explain the lost authorization, got: {stderr}",
    );
    assert!(
        stderr.contains(&third_npub),
        "the error should name the dropped pubkey {third_npub}, got: {stderr}",
    );
    assert!(
        !stderr.contains("was published to nostr"),
        "the gate must fire before anything is published, got: {stderr}",
    );
    let winner = latest_announcement(&harness, me, &identifier).await?;
    assert_eq!(
        winner.id, existing.id,
        "a refused init must not publish a new announcement",
    );

    // --force acknowledges the drop and republishes the collapsed listing
    // (then fails at the git-data push — the arranged server is inert).
    let out = repo
        .ngit(["init", "--lead-maintainer", &lead_npub, "--force"])
        .output()
        .await
        .context("failed to spawn ngit init --lead-maintainer --force")?;
    expect_announcement_published_but_push_failed("ngit init --lead-maintainer --force", &out)?;

    let announcement = latest_announcement(&harness, me, &identifier).await?;
    assert_ne!(announcement.id, existing.id, "a republish must have landed");

    // The lead's role *transitions*: the pre-role-tag listing materializes
    // as an `m` record, which the promotion closes at publish time while
    // the `M` entry opens at it (per NIP-34 a pubkey may appear in one
    // tag per letter to record transitions).
    let lead_m_entries = role_entries(&announcement, "M", &lead);
    assert_eq!(
        lead_m_entries.len(),
        1,
        "the designated lead should carry one M entry; got {lead_m_entries:?}",
    );
    let m_entry = &lead_m_entries[0];
    assert_eq!(
        m_entry.len(),
        3,
        "the promotion opens the M entry at publish time: {m_entry:?}",
    );
    assert!(
        m_entry[2].parse::<u64>().is_ok_and(|start| start > 0),
        "the M start boundary must be a unix timestamp: {m_entry:?}",
    );
    let lead_old_entries = role_entries(&announcement, "m", &lead);
    assert_eq!(
        lead_old_entries.len(),
        1,
        "the lead's materialized m record must be kept, closed; got {lead_old_entries:?}",
    );
    assert_eq!(
        (lead_old_entries[0].len(), lead_old_entries[0][2].as_str()),
        (4, "0"),
        "the closed m record covers the pre-role-tag history: {lead_old_entries:?}",
    );

    // The author continues under the same letter: untimed.
    let my_entries = role_entries(&announcement, "m", &me);
    assert_eq!(
        my_entries,
        vec![vec!["m".to_string(), me.to_string()]],
        "the author should carry one untimed m entry",
    );

    // The dropped member's history is materialized from the pre-role-tag
    // announcement and closed: active for the repository's entire history
    // ("0" start) until the republish ended it.
    let third_entries = role_entries(&announcement, "m", &third);
    assert_eq!(
        third_entries.len(),
        1,
        "the dropped member must keep exactly one closed entry; got {third_entries:?}",
    );
    let entry = &third_entries[0];
    assert_eq!(
        entry.len(),
        4,
        "materialized closure is [m, pk, 0, end]: {entry:?}"
    );
    assert_eq!(
        entry[2], "0",
        "materialized start is the whole history: {entry:?}"
    );
    assert!(
        entry[3].parse::<u64>().is_ok_and(|end| end > 0),
        "the end boundary must be a unix timestamp: {entry:?}",
    );

    // The degradation tag carries only the current members.
    let maintainers = tag_values(&announcement, "maintainers");
    assert_eq!(
        {
            let mut sorted = maintainers.clone();
            sorted.sort();
            sorted
        },
        {
            let mut expected = vec![me.to_string(), lead.to_string()];
            expected.sort();
            expected
        },
        "the deprecated maintainers tag must list exactly [me, lead]",
    );

    Ok(())
}

#[tokio::test]
async fn collapse_with_lead_cover_needs_no_force() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;

    let (repo, my_keys, identifier, existing, lead_keys, third_keys) =
        arrange_three_member_announcement(&harness).await?;
    let me = my_keys.public_key();
    let lead = lead_keys.public_key();
    let third = third_keys.public_key();
    let lead_npub = lead.to_bech32()?;

    // The lead's own announcement keeps `third` listed *and* acknowledges
    // me, so their listing carries authority the moment my collapsed
    // listing publishes: no --force required.
    harness
        .publish_fabricated_announcement(
            &lead_keys,
            FabricateAnnouncementOpts {
                maintainers_tag: Some(vec![lead, me, third]),
                created_at: Some(Timestamp::now() - 30u64),
                ..FabricateAnnouncementOpts::new(identifier.clone(), vec![])
            },
        )
        .await?;

    let out = repo
        .ngit(["init", "--lead-maintainer", &lead_npub])
        .output()
        .await
        .context("failed to spawn ngit init --lead-maintainer")?;
    // No --force gate: the collapse publishes straight away (and then
    // fails at the git-data push against the inert arranged server).
    expect_announcement_published_but_push_failed(
        "ngit init --lead-maintainer (covered drop)",
        &out,
    )?;

    let announcement = latest_announcement(&harness, me, &identifier).await?;
    assert_ne!(announcement.id, existing.id, "a republish must have landed");
    let lead_m_entries = role_entries(&announcement, "M", &lead);
    assert_eq!(
        lead_m_entries.len(),
        1,
        "the designated lead should carry one M entry; got {lead_m_entries:?}",
    );
    assert!(
        lead_m_entries[0].len() < 4 || lead_m_entries[0].len() % 2 == 1,
        "the lead's M entry must be active: {lead_m_entries:?}",
    );
    let maintainers = tag_values(&announcement, "maintainers");
    assert!(
        maintainers.contains(&me.to_string()) && maintainers.contains(&lead.to_string()),
        "the collapsed listing is [me, lead]; got {maintainers:?}",
    );
    assert!(
        !maintainers.contains(&third.to_string()),
        "the covered member is still dropped from *my* listing; got {maintainers:?}",
    );

    Ok(())
}
