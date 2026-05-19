//! Remaining legacy regression: only the multi-grasp "newer state event
//! unresolvable but older state event resolvable" case. Migrates out in
//! PR 5a alongside `publish_repo_with_two_grasp_servers`, which is the
//! scenario builder this test needs (and which intentionally doesn't
//! exist yet).
//!
//! All five sibling cases from this file were migrated to
//! `tests/list_state.rs`, `tests/list_pr.rs`, and `tests/list_patch.rs`
//! in the same commit that shrank this file.

use super::*;

mod with_state_announcement {

    use super::*;

    mod when_newer_relay_state_has_missing_oid_but_older_relay_state_is_resolvable {

        use super::*;

        /// Two relays serve different state events; two git servers each have
        /// different OIDs.
        ///
        /// - Relay 55 (repo relay A): **newer** state event → main = fake_oid
        ///   (not on any git server)
        /// - Relay 56 (repo relay B): **older** state event → main = commit_a
        ///   (present on git_server_1)
        /// - git_server_1: main = commit_a
        /// - git_server_2: main = commit_b (a different real commit)
        ///
        /// Expected: `list` skips the newer unresolvable event and advertises
        /// `commit_a` from the older-but-resolvable state event.
        #[tokio::test]
        #[serial]
        async fn uses_older_resolvable_state_event() -> Result<()> {
            // --- git_server_1: has commit_a on main ---
            let git_server_1 = prep_git_repo()?;
            std::fs::write(git_server_1.dir.join("server1.md"), "server1")?;
            let commit_a = git_server_1.stage_and_commit("server1.md")?;
            let bare_server_1 = GitTestRepo::recreate_as_bare(&git_server_1)?;

            // --- git_server_2: has commit_b on main (different commit) ---
            let git_server_2 = prep_git_repo()?;
            std::fs::write(git_server_2.dir.join("server2.md"), "server2")?;
            let commit_b = git_server_2.stage_and_commit("server2.md")?;
            let bare_server_2 = GitTestRepo::recreate_as_bare(&git_server_2)?;

            assert_ne!(commit_a, commit_b);

            let fake_oid = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
            let root_commit = "9ee507fc4357d7ee16a5d8901bedcd103f23c17d";
            let identifier = format!("{root_commit}-consider-it-random");

            // Older state event: main = commit_a (resolvable via git_server_1)
            let older_state_event = make_event_old_or_change_user(
                nostr::event::EventBuilder::new(STATE_KIND, "")
                    .tags([
                        nostr::Tag::identifier(identifier.clone()),
                        nostr::Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("HEAD")),
                            vec!["ref: refs/heads/main".to_string()],
                        ),
                        nostr::Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("refs/heads/main")),
                            vec![commit_a.to_string()],
                        ),
                    ])
                    .sign_with_keys(&TEST_KEY_1_KEYS)
                    .unwrap(),
                &TEST_KEY_1_KEYS,
                60, // 60 seconds old
            );

            // Newer state event: main = fake_oid (NOT on any git server)
            let newer_state_event = nostr::event::EventBuilder::new(STATE_KIND, "")
                .tags([
                    nostr::Tag::identifier(identifier.clone()),
                    nostr::Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("HEAD")),
                        vec!["ref: refs/heads/main".to_string()],
                    ),
                    nostr::Tag::custom(
                        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("refs/heads/main")),
                        vec![fake_oid.to_string()],
                    ),
                ])
                .sign_with_keys(&TEST_KEY_1_KEYS)
                .unwrap();

            let git_repo = prep_git_repo()?;

            // Base events (metadata + relay list + repo ref) go on both relays.
            let repo_ref_event = generate_repo_ref_event_with_git_server(vec![
                bare_server_1.dir.to_str().unwrap().to_string(),
                bare_server_2.dir.to_str().unwrap().to_string(),
            ]);
            let base_events = vec![
                generate_test_key_1_metadata_event("fred"),
                generate_test_key_1_relay_list_event(),
                repo_ref_event,
            ];

            // fallback (51,52) user write (53, 55) repo (55, 56) blaster (57)
            let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
                Relay::new(8051, None, None),
                Relay::new(8052, None, None),
                Relay::new(8053, None, None),
                Relay::new(8055, None, None),
                Relay::new(8056, None, None),
                Relay::new(8057, None, None),
            );
            r51.events = base_events.clone();
            // r55 (repo relay A) serves the newer state event with the fake OID
            r55.events = [base_events.clone(), vec![newer_state_event]].concat();
            // r56 (repo relay B) serves the older state event with commit_a
            r56.events = [base_events, vec![older_state_event]].concat();

            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_after_fetch(&git_repo)?;
                p.send_line("list")?;
                p.expect("git servers: listing refs...\r\n")?;
                let res = p.expect_eventually("\r\n\r\n")?;
                p.exit()?;
                for p in [51, 52, 53, 55, 56, 57] {
                    relay::shutdown_relay(8000 + p)?;
                }
                let lines: HashSet<String> = res
                    .split("\r\n")
                    .map(|e| e.to_string())
                    .filter(|s| {
                        !s.contains("remote: ")
                            && !s.contains("Receiving objects")
                            && !s.contains("Resolving deltas")
                            && !s.contains("fetching /")
                    })
                    .collect();
                // The fake OID from the newer-but-unresolvable state event must
                // NOT appear.
                assert!(
                    !lines.iter().any(|l| l.contains(fake_oid)),
                    "fake OID from newer unresolvable state event must not be advertised; got: {lines:?}"
                );
                // commit_a from the older-but-resolvable state event must be
                // advertised for main.
                assert!(
                    lines.contains(&format!("{commit_a} refs/heads/main")),
                    "commit_a from older resolvable state event must be advertised; got: {lines:?}"
                );
                // commit_b (only on git_server_2, not referenced by any state
                // event) must NOT appear for main.
                assert!(
                    !lines.contains(&format!("{commit_b} refs/heads/main")),
                    "commit_b from git_server_2 must not override the chosen state event; got: {lines:?}"
                );
                Ok(())
            });
            // launch relays
            let _ = join!(
                r51.listen_until_close(),
                r52.listen_until_close(),
                r53.listen_until_close(),
                r55.listen_until_close(),
                r56.listen_until_close(),
                r57.listen_until_close(),
            );
            cli_tester_handle.join().unwrap()?;
            Ok(())
        }
    }
}
