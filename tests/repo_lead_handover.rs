//! `ngit repo edit --lead-maintainer <someone else>` — safe lead handover
//! end-to-end against fabricated announcements.
//!
//! Designating another pubkey as lead follows NIP-34's SHOULD: the
//! author's announcement then keeps only themselves and the lead active. If
//! the proposed lead does not cover another current maintainer, that person
//! would lose authorized-maintainer status, so the edit must identify them
//! and refuse the unnamed removal. There is no force override.
//!
//! Same error-message-substring caveat as `tests/init_state_fresh.rs`:
//! asserting on a stable stderr fragment is the tolerated shortcut for
//! tests whose contract *is* "this validation fired".

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{FabricateAnnouncementOpts, Harness, RoleEntry, tag_value, tag_values};

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
async fn handover_to_unprepared_lead_names_and_refuses_uncovered_member() -> Result<()> {
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

    // The lead has no announcement, so their listing covers nobody.
    let out = repo
        .ngit(["repo", "edit", "--lead-maintainer", &lead_npub])
        .output()
        .await
        .context("failed to spawn ngit repo edit --lead-maintainer")?;
    assert!(
        !out.status.success(),
        "handover removing an uncovered maintainer must fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("active graph"),
        "the error should explain the graph removal, got: {stderr}",
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
        "a refused edit must not publish a new announcement",
    );

    Ok(())
}

#[tokio::test]
async fn handover_to_prepared_lead_preserves_the_active_graph() -> Result<()> {
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

    // The proposed lead first declares themselves lead with the complete
    // current roster.
    harness
        .publish_fabricated_announcement(
            &lead_keys,
            FabricateAnnouncementOpts {
                roles: vec![
                    RoleEntry::lead(lead),
                    RoleEntry::co_maintainer(me),
                    RoleEntry::co_maintainer(third),
                ],
                maintainers_tag: Some(vec![lead, me, third]),
                created_at: Some(Timestamp::now() - 30u64),
                ..FabricateAnnouncementOpts::new(identifier.clone(), vec![])
            },
        )
        .await?;

    let out = repo
        .ngit(["repo", "edit", "--lead-maintainer", &lead_npub])
        .output()
        .await
        .context("failed to spawn ngit repo edit --lead-maintainer")?;
    // The prepared lead covers every existing maintainer, so the handover
    // publishes straight away (then the inert git-data push fails).
    expect_announcement_published_but_push_failed(
        "ngit repo edit --lead-maintainer (prepared handover)",
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
        "the active projection should contain me and the lead; got {maintainers:?}",
    );
    assert!(
        !maintainers.contains(&third.to_string()),
        "the covered member should move to deferred history in my announcement; got {maintainers:?}",
    );
    let third_history = role_entries(&announcement, "m", &third);
    assert_eq!(third_history.len(), 1);
    assert_eq!(
        third_history[0].last().map(String::as_str),
        Some("defer"),
        "the old lead should retain third-party history without assigning it",
    );

    Ok(())
}
