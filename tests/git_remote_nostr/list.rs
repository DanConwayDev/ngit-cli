use super::*;

mod without_state_announcement {

    use super::*;

    #[tokio::test]
    #[serial]
    async fn lists_head_and_2_branches_and_commit_ids_from_git_server() -> Result<()> {
        let source_git_repo = prep_git_repo()?;
        let _source_path = source_git_repo.dir.to_str().unwrap().to_string();
        std::fs::write(source_git_repo.dir.join("commit.md"), "some content")?;
        let main_commit_id = source_git_repo.stage_and_commit("commit.md")?;

        source_git_repo.create_branch("vnext")?;
        source_git_repo.checkout("vnext")?;
        std::fs::write(source_git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = source_git_repo.stage_and_commit("vnext.md")?;
        source_git_repo.checkout("main")?;

        let git_repo = prep_git_repo()?;
        let events = vec![
            generate_test_key_1_metadata_event("fred"),
            generate_test_key_1_relay_list_event(),
            generate_repo_ref_event_with_git_server(vec![
                source_git_repo.dir.to_str().unwrap().to_string(),
            ]),
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
        r51.events = events.clone();
        r55.events = events;

        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_after_fetch(&git_repo)?;
            p.send_line("list")?;
            p.expect("git servers: listing refs...\r\n")?;
            // println!("{}", p.expect_eventually("\r\n\r\n")?);
            let res = p.expect_eventually("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
            assert_eq!(
                res.split("\r\n")
                    .map(|e| e.to_string())
                    .filter(|s| !s.contains("remote: ")
                        && !s.contains("Receiving objects")
                        && !s.contains("Resolving deltas")
                        && !s.contains("fetching /"))
                    .collect::<HashSet<String>>(),
                HashSet::from([
                    "@refs/heads/main HEAD".to_string(),
                    format!("{main_commit_id} refs/heads/main"),
                    format!("{vnext_commit_id} refs/heads/vnext"),
                ]),
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
mod with_state_announcement {

    use super::*;

    mod when_announcement_matches_git_server {

        use super::*;

        #[tokio::test]
        #[serial]
        async fn lists_head_and_2_branches_and_commit_ids_announcement() -> Result<()> {
            let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
            let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

            let main_commit_id = source_git_repo.get_tip_of_local_branch("main")?;
            let example_commit_id = source_git_repo.get_tip_of_local_branch("example-branch")?;

            let git_repo = prep_git_repo()?;
            let events = vec![
                generate_test_key_1_metadata_event("fred"),
                generate_test_key_1_relay_list_event(),
                generate_repo_ref_event_with_git_server(vec![
                    source_git_repo.dir.to_str().unwrap().to_string(),
                ]),
                state_event,
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
            r51.events = events.clone();
            r55.events = events;

            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_after_fetch(&git_repo)?;
                p.send_line("list")?;
                p.expect("git servers: listing refs...\r\n")?;
                // println!("{}", p.expect_eventually("\r\n\r\n")?);
                let res = p.expect_eventually("\r\n\r\n")?;
                p.exit()?;
                for p in [51, 52, 53, 55, 56, 57] {
                    relay::shutdown_relay(8000 + p)?;
                }
                assert_eq!(
                    res.split("\r\n")
                        .map(|e| e.to_string())
                        .collect::<HashSet<String>>(),
                    HashSet::from([
                        "@refs/heads/main HEAD".to_string(),
                        format!("{main_commit_id} refs/heads/main"),
                        format!("{example_commit_id} refs/heads/example-branch"),
                    ]),
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
    mod when_state_event_references_oids_not_on_git_server {

        use super::*;

        /// Regression test for the bug where a state event published ahead of
        /// the corresponding `git push` caused `git clone` / `git fetch` to
        /// fail with missing-object errors.
        ///
        /// The fix walks per-relay state events newest-first and picks the
        /// first one whose every OID is either present on a git server or
        /// already available locally.  When no such event exists it falls back
        /// to the raw git-server state.
        #[tokio::test]
        #[serial]
        async fn falls_back_to_git_server_state() -> Result<()> {
            // Build a real git repo that acts as the git server.
            let source_git_repo = prep_git_repo()?;
            std::fs::write(source_git_repo.dir.join("initial.md"), "initial")?;
            let main_commit_id = source_git_repo.stage_and_commit("initial.md")?;

            // Craft a state event that claims main points at a commit that has
            // NOT been pushed to the git server yet (a plausible OID that does
            // not exist anywhere).
            let fake_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let root_commit = "9ee507fc4357d7ee16a5d8901bedcd103f23c17d";
            let state_event = nostr::event::EventBuilder::new(STATE_KIND, "")
                .tags([
                    nostr::Tag::identifier(format!("{root_commit}-consider-it-random")),
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
            let events = vec![
                generate_test_key_1_metadata_event("fred"),
                generate_test_key_1_relay_list_event(),
                generate_repo_ref_event_with_git_server(vec![
                    source_git_repo.dir.to_str().unwrap().to_string(),
                ]),
                state_event,
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
            r51.events = events.clone();
            r55.events = events;

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
                // The fake OID must NOT appear – the list must fall back to
                // what the git server actually has.
                assert!(
                    !lines.iter().any(|l| l.contains(fake_oid)),
                    "fake OID from unresolvable state event must not be advertised; got: {lines:?}"
                );
                // The real commit that IS on the git server must be advertised.
                assert!(
                    lines.contains(&format!("{main_commit_id} refs/heads/main")),
                    "real git-server commit must be advertised; got: {lines:?}"
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

    mod when_announcement_doesnt_match_git_server {

        use super::*;

        #[tokio::test]
        #[serial]
        async fn anouncement_state_is_used() -> Result<()> {
            let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
            let _source_path = source_git_repo.dir.to_str().unwrap().to_string();
            let main_original_commit_id = source_git_repo.get_tip_of_local_branch("main")?;

            {
                // add commit to main on git server
                let tmp_repo = GitTestRepo::clone_repo(&source_git_repo)?;
                std::fs::write(tmp_repo.dir.join("commitx.md"), "some content")?;
                tmp_repo.stage_and_commit("commitx.md")?;
                let mut remote = tmp_repo.git_repo.find_remote("origin")?;
                remote.push(&["refs/heads/main:refs/heads/main"], None)?;
            }

            let main_updated_commit_id = source_git_repo.get_tip_of_local_branch("main")?;
            assert_ne!(main_original_commit_id, main_updated_commit_id);
            let example_commit_id = source_git_repo.get_tip_of_local_branch("example-branch")?;

            let git_repo = prep_git_repo()?;
            let events = vec![
                generate_test_key_1_metadata_event("fred"),
                generate_test_key_1_relay_list_event(),
                generate_repo_ref_event_with_git_server(vec![
                    source_git_repo.dir.to_str().unwrap().to_string(),
                ]),
                state_event,
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
            r51.events = events.clone();
            r55.events = events;

            let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
                let mut p = cli_tester_after_fetch(&git_repo)?;
                p.send_line("list")?;
                p.expect("git servers: listing refs...\r\n")?;

                // println!("{}", p.expect_eventually("\r\n\r\n")?);
                let res = p.expect_eventually("\r\n\r\n")?;
                p.exit()?;
                for p in [51, 52, 53, 55, 56, 57] {
                    relay::shutdown_relay(8000 + p)?;
                }
                assert_eq!(
                    res.split("\r\n")
                        .map(|e| e.to_string())
                        .filter(|s| !s.contains("remote: ")
                            && !s.contains("Receiving objects")
                            && !s.contains("Resolving deltas")
                            && !s.contains("fetching /"))
                        .collect::<HashSet<String>>(),
                    HashSet::from([
                        "@refs/heads/main HEAD".to_string(),
                        format!("{main_original_commit_id} refs/heads/main"),
                        format!("{example_commit_id} refs/heads/example-branch"),
                    ]),
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

    mod when_there_are_open_proposals {

        use super::*;

        #[tokio::test]
        #[serial]
        async fn open_proposal_listed_in_prs_namespace() -> Result<()> {
            let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
            let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

            let main_commit_id = source_git_repo.get_tip_of_local_branch("main")?;
            let example_commit_id = source_git_repo.get_tip_of_local_branch("example-branch")?;

            let git_repo = prep_git_repo()?;

            let events = vec![
                generate_test_key_1_metadata_event("fred"),
                generate_test_key_1_relay_list_event(),
                generate_repo_ref_event_with_git_server(vec![
                    source_git_repo.dir.to_str().unwrap().to_string(),
                ]),
                state_event,
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
            r51.events = events.clone();
            r55.events = events;

            let cli_tester_handle = std::thread::spawn(move || -> Result<String> {
                cli_tester_create_proposals()?;

                let mut p = cli_tester_after_fetch(&git_repo)?;
                p.send_line("list")?;
                p.expect("git servers: listing refs...\r\n")?;
                // println!("{}", p.expect_eventually("\r\n\r\n")?);
                let res = p.expect_eventually("\r\n\r\n")?;

                p.exit()?;
                for p in [51, 52, 53, 55, 56, 57] {
                    relay::shutdown_relay(8000 + p)?;
                }
                Ok(res)
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

            let res = cli_tester_handle.join().unwrap()?;

            let proposal_creation_repo = cli_tester_create_proposal_branches_ready_to_send()?;

            let mut pr_refs = vec![];
            for name in [
                FEATURE_BRANCH_NAME_1,
                FEATURE_BRANCH_NAME_2,
                FEATURE_BRANCH_NAME_3,
            ] {
                let tip = proposal_creation_repo.get_tip_of_local_branch(name)?;
                let branch_name = get_proposal_branch_name_from_events(&r55.events, name)?;
                pr_refs.push(format!("{tip} refs/heads/{branch_name}"));
                pr_refs.push(format!("{tip} refs/{branch_name}"));
                let proposal_id = get_proposal_id_from_branch_name(&r55.events, name)?;
                pr_refs.push(format!("{tip} refs/pr/{proposal_id}/head"));
            }

            assert_eq!(
                res.split("\r\n")
                    .map(|e| e.to_string())
                    .collect::<HashSet<String>>(),
                [
                    vec![
                        "@refs/heads/main HEAD".to_string(),
                        format!("{} refs/heads/main", main_commit_id),
                        format!("{} refs/heads/example-branch", example_commit_id),
                    ],
                    pr_refs,
                ]
                .concat()
                .iter()
                .cloned()
                .collect::<HashSet<String>>()
            );

            Ok(())
        }
    }
}
