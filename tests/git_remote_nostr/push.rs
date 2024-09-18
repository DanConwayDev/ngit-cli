use super::*;

#[tokio::test]
#[serial]
async fn new_branch_when_no_state_event_exists() -> Result<()> {
    generate_repo_with_state_event().await?;
    Ok(())
}
mod two_branches_in_batch_one_added_one_updated {

    use super::*;

    #[tokio::test]
    #[serial]
    async fn updates_branch_on_git_server() -> Result<()> {
        let git_repo = prep_git_repo()?;
        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

        std::fs::write(git_repo.dir.join("commit.md"), "some content")?;
        let main_commit_id = git_repo.stage_and_commit("commit.md")?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("vnext.md")?;

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
            assert_ne!(
                source_git_repo.get_tip_of_local_branch("main")?,
                main_commit_id
            );

            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;

            p.send_line("push refs/heads/main:refs/heads/main")?;
            p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
            p.send_line("")?;
            p.expect_eventually("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }

            assert_eq!(
                source_git_repo.get_tip_of_local_branch("main")?,
                main_commit_id
            );

            assert_eq!(
                source_git_repo.get_tip_of_local_branch("vnext")?,
                vnext_commit_id
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

    #[tokio::test]
    #[serial]
    async fn remote_refs_updated_in_local_git() -> Result<()> {
        let git_repo = prep_git_repo()?;
        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

        std::fs::write(git_repo.dir.join("commit.md"), "some content")?;
        let main_commit_id = git_repo.stage_and_commit("commit.md")?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("vnext.md")?;

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
            assert_ne!(
                source_git_repo.get_tip_of_local_branch("main")?,
                main_commit_id
            );

            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
            p.send_line("push refs/heads/main:refs/heads/main")?;
            p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
            p.send_line("")?;
            p.expect_eventually("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }

            assert_eq!(
                git_repo
                    .git_repo
                    .find_reference("refs/remotes/nostr/main")?
                    .peel_to_commit()?
                    .id(),
                main_commit_id,
            );

            assert_eq!(
                git_repo
                    .git_repo
                    .find_reference("refs/remotes/nostr/vnext")?
                    .peel_to_commit()?
                    .id(),
                vnext_commit_id
            );

            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
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

    #[tokio::test]
    #[serial]
    async fn prints_git_helper_ok_respose() -> Result<()> {
        let git_repo = prep_git_repo()?;
        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

        std::fs::write(git_repo.dir.join("commit.md"), "some content")?;
        let main_commit_id = git_repo.stage_and_commit("commit.md")?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        git_repo.stage_and_commit("vnext.md")?;

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
            assert_ne!(
                source_git_repo.get_tip_of_local_branch("main")?,
                main_commit_id
            );

            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;

            p.send_line("push refs/heads/main:refs/heads/main")?;
            p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
            p.send_line("")?;
            p.expect("ok refs/heads/main\r\n")?;
            p.expect("ok refs/heads/vnext\r\n")?;
            p.expect_eventually("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
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

    #[tokio::test]
    #[serial]
    async fn when_no_existing_state_event_state_on_git_server_published_in_nostr_state_event()
    -> Result<()> {
        let git_repo = prep_git_repo()?;
        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

        std::fs::write(git_repo.dir.join("commit.md"), "some content")?;
        let main_commit_id = git_repo.stage_and_commit("commit.md")?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("vnext.md")?;

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
            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
            p.send_line("push refs/heads/main:refs/heads/main")?;
            p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
            p.send_line("")?;
            p.expect_eventually_and_print("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
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

        let state_event = r56
            .events
            .iter()
            .find(|e| e.kind().eq(&STATE_KIND))
            .context("state event not created")?;

        assert_eq!(
            state_event.identifier(),
            generate_repo_ref_event().identifier(),
        );
        // println!("{:#?}", state_event);
        assert_eq!(
            state_event
                .tags
                .iter()
                .filter(|t| t.kind().to_string().as_str().ne("d"))
                .map(|t| t.as_vec().to_vec())
                .collect::<HashSet<Vec<String>>>(),
            HashSet::from([
                vec!["HEAD".to_string(), "ref: refs/heads/main".to_string()],
                vec!["refs/heads/main".to_string(), main_commit_id.to_string()],
                vec!["refs/heads/vnext".to_string(), vnext_commit_id.to_string()],
            ]),
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn existing_state_event_published_in_nostr_state_event() -> Result<()> {
        let (state_event, source_git_repo) = generate_repo_with_state_event().await?;

        let git_repo = prep_git_repo()?;
        let example_branch_commit_id = git_repo.get_tip_of_local_branch("main")?.to_string(); // same as example

        std::fs::write(git_repo.dir.join("new.md"), "some content")?;
        let main_commit_id = git_repo.stage_and_commit("new.md")?;
        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("more.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("more.md")?;

        let events = vec![
            generate_test_key_1_metadata_event("fred"),
            generate_test_key_1_relay_list_event(),
            generate_repo_ref_event_with_git_server(vec![
                source_git_repo.dir.to_str().unwrap().to_string(),
            ]),
            state_event.clone(),
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
            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
            p.send_line("push refs/heads/main:refs/heads/main")?;
            p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
            p.send_line("")?;
            p.expect_eventually_and_print("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
            // local refs updated
            assert_eq!(
                git_repo
                    .git_repo
                    .find_reference("refs/remotes/nostr/main")?
                    .peel_to_commit()?
                    .id(),
                main_commit_id,
            );

            assert_eq!(
                git_repo
                    .git_repo
                    .find_reference("refs/remotes/nostr/vnext")?
                    .peel_to_commit()?
                    .id(),
                vnext_commit_id
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

        // git_server updated
        assert_eq!(
            source_git_repo.get_tip_of_local_branch("main")?,
            main_commit_id
        );

        assert_eq!(
            source_git_repo.get_tip_of_local_branch("vnext")?,
            vnext_commit_id
        );

        // state annoucement updated
        let state_event = r56
            .events
            .iter()
            .find(|e| e.kind().eq(&STATE_KIND))
            .context("state event not created")?;

        // println!("{:#?}", state_event);
        assert_eq!(
            state_event
                .tags
                .iter()
                .filter(|t| t.kind().to_string().as_str().ne("d"))
                .map(|t| t.as_vec().to_vec())
                .collect::<HashSet<Vec<String>>>(),
            HashSet::from([
                vec!["HEAD".to_string(), "ref: refs/heads/main".to_string()],
                vec!["refs/heads/main".to_string(), main_commit_id.to_string()],
                vec![
                    "refs/heads/example-branch".to_string(),
                    example_branch_commit_id.to_string()
                ],
                vec!["refs/heads/vnext".to_string(), vnext_commit_id.to_string()],
            ]),
        );
        Ok(())
    }
}
mod delete_one_branch {

    use super::*;

    #[tokio::test]
    #[serial]
    async fn deletes_branch_on_git_server() -> Result<()> {
        let git_repo = prep_git_repo()?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("vnext.md")?;

        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

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
            assert_eq!(
                source_git_repo
                    .git_repo
                    .find_reference("refs/heads/vnext")?
                    .peel_to_commit()?
                    .id(),
                vnext_commit_id
            );

            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
            p.send_line("push :refs/heads/vnext")?;
            p.send_line("")?;
            p.expect_eventually_and_print("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }

            assert!(
                source_git_repo
                    .git_repo
                    .find_reference("refs/heads/vnext")
                    .is_err()
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

    #[tokio::test]
    #[serial]
    async fn remote_refs_updated_in_local_git() -> Result<()> {
        let git_repo = prep_git_repo()?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("vnext.md")?;

        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

        git_repo
            .git_repo
            .reference("refs/remotes/nostr/vnext", vnext_commit_id, true, "")?;

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
            assert_eq!(
                git_repo
                    .git_repo
                    .find_reference("refs/remotes/nostr/vnext")?
                    .peel_to_commit()?
                    .id(),
                vnext_commit_id
            );

            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
            p.send_line("push :refs/heads/vnext")?;
            p.send_line("")?;
            p.expect_eventually("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
            assert!(
                git_repo
                    .git_repo
                    .find_reference("refs/remotes/nostr/vnext")
                    .is_err()
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

    #[tokio::test]
    #[serial]
    async fn prints_git_helper_ok_respose() -> Result<()> {
        let git_repo = prep_git_repo()?;

        git_repo.create_branch("vnext")?;
        git_repo.checkout("vnext")?;
        std::fs::write(git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = git_repo.stage_and_commit("vnext.md")?;

        let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;

        git_repo
            .git_repo
            .reference("refs/remotes/nostr/vnext", vnext_commit_id, true, "")?;

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
            let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
            p.send_line("push :refs/heads/vnext")?;
            p.send_line("")?;
            p.expect("ok refs/heads/vnext\r\n")?;
            p.expect_eventually("\r\n\r\n")?;
            p.exit()?;
            for p in [51, 52, 53, 55, 56, 57] {
                relay::shutdown_relay(8000 + p)?;
            }
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

    mod when_existing_state_event {
        use super::*;

        #[tokio::test]
        #[serial]
        async fn state_event_updated_and_branch_deleted_and_ok_printed() -> Result<()> {
            let (state_event, source_git_repo) = generate_repo_with_state_event().await?;

            let git_repo = prep_git_repo()?;
            let main_commit_id = git_repo.get_tip_of_local_branch("main")?.to_string(); // same as example

            let events = vec![
                generate_test_key_1_metadata_event("fred"),
                generate_test_key_1_relay_list_event(),
                generate_repo_ref_event_with_git_server(vec![
                    source_git_repo.dir.to_str().unwrap().to_string(),
                ]),
                state_event.clone(),
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
                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
                p.send_line("push :refs/heads/example-branch")?;
                p.send_line("")?;
                p.expect("ok refs/heads/example-branch\r\n")?;
                p.expect_eventually("\r\n\r\n")?;
                p.exit()?;
                for p in [51, 52, 53, 55, 56, 57] {
                    relay::shutdown_relay(8000 + p)?;
                }
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

            let state_event = r56
                .events
                .iter()
                .find(|e| e.kind().eq(&STATE_KIND))
                .context("state event not created")?;

            // println!("{:#?}", state_event);
            assert_eq!(
                state_event
                    .tags
                    .iter()
                    .filter(|t| t.kind().to_string().as_str().ne("d"))
                    .map(|t| t.as_vec().to_vec())
                    .collect::<HashSet<Vec<String>>>(),
                HashSet::from([
                    vec!["HEAD".to_string(), "ref: refs/heads/main".to_string()],
                    vec!["refs/heads/main".to_string(), main_commit_id.to_string()],
                ]),
            );
            Ok(())
        }

        mod already_deleted_on_git_server {
            use super::*;

            #[tokio::test]
            #[serial]
            async fn existing_state_event_updated_and_ok_printed() -> Result<()> {
                let (state_event, source_git_repo) = generate_repo_with_state_event().await?;

                {
                    // delete branch on git server
                    let tmp_repo = GitTestRepo::clone_repo(&source_git_repo)?;
                    let mut remote = tmp_repo.git_repo.find_remote("origin")?;
                    remote.push(&[":refs/heads/example-branch"], None)?;
                }

                let git_repo = prep_git_repo()?;
                let main_commit_id = git_repo.get_tip_of_local_branch("main")?.to_string(); // same as example

                let events = vec![
                    generate_test_key_1_metadata_event("fred"),
                    generate_test_key_1_relay_list_event(),
                    generate_repo_ref_event_with_git_server(vec![
                        source_git_repo.dir.to_str().unwrap().to_string(),
                    ]),
                    state_event.clone(),
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
                    let mut p =
                        cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
                    p.send_line("push :refs/heads/example-branch")?;
                    p.send_line("")?;
                    p.expect("ok refs/heads/example-branch\r\n")?;
                    p.expect_eventually("\r\n")?;
                    p.exit()?;
                    for p in [51, 52, 53, 55, 56, 57] {
                        relay::shutdown_relay(8000 + p)?;
                    }
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

                let state_event = r56
                    .events
                    .iter()
                    .find(|e| e.kind().eq(&STATE_KIND))
                    .context("state event not created")?;

                // println!("{:#?}", state_event);
                assert_eq!(
                    state_event
                        .tags
                        .iter()
                        .filter(|t| t.kind().to_string().as_str().ne("d"))
                        .map(|t| t.as_vec().to_vec())
                        .collect::<HashSet<Vec<String>>>(),
                    HashSet::from([
                        vec!["HEAD".to_string(), "ref: refs/heads/main".to_string()],
                        vec!["refs/heads/main".to_string(), main_commit_id.to_string()],
                    ]),
                );
                Ok(())
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn pushes_to_all_git_servers_listed_and_ok_printed() -> Result<()> {
    let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
    let second_source_git_repo = GitTestRepo::duplicate(&source_git_repo)?;

    // uppdate announcement with extra git server

    let git_repo = prep_git_repo()?;

    std::fs::write(git_repo.dir.join("new.md"), "some content")?;
    let main_commit_id = git_repo.stage_and_commit("new.md")?;

    let events = vec![
        generate_test_key_1_metadata_event("fred"),
        generate_test_key_1_relay_list_event(),
        generate_repo_ref_event_with_git_server(vec![
            source_git_repo.dir.to_str().unwrap().to_string(),
            second_source_git_repo.dir.to_str().unwrap().to_string(),
        ]),
        state_event.clone(),
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
        let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
        p.send_line("push refs/heads/main:refs/heads/main")?;
        p.send_line("")?;
        p.expect("ok refs/heads/main\r\n")?;
        p.expect_eventually("\r\n\r\n")?;
        p.exit()?;
        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }
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

    // git_server updated
    assert_eq!(
        source_git_repo.get_tip_of_local_branch("main")?,
        main_commit_id
    );
    assert_eq!(
        second_source_git_repo.get_tip_of_local_branch("main")?,
        main_commit_id
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn proposal_merge_commit_pushed_to_main_leads_to_status_event_issued() -> Result<()> {
    //
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let source_path = source_git_repo.dir.to_str().unwrap().to_string();

    let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
        Relay::new(8051, None, None),
        Relay::new(8052, None, None),
        Relay::new(8053, None, None),
        Relay::new(8055, None, None),
        Relay::new(8056, None, None),
        Relay::new(8057, None, None),
    );
    r51.events = events.clone();
    r55.events = events.clone();

    #[allow(clippy::mutable_key_type)]
    let before = r55.events.iter().cloned().collect::<HashSet<Event>>();

    let cli_tester_handle = std::thread::spawn(move || -> Result<(String, Oid)> {
        let branch_name = get_proposal_branch_name_from_events(&events, FEATURE_BRANCH_NAME_1)?;

        let git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.checkout_remote_branch(&branch_name)?;
        git_repo.checkout("refs/heads/main")?;

        std::fs::write(git_repo.dir.join("new.md"), "some content")?;
        git_repo.stage_and_commit("new.md")?;

        CliTester::new_git_with_remote_helper_from_dir(
            &git_repo.dir,
            ["merge", &branch_name, "-m", "proposal merge commit message"],
        )
        .expect_end_eventually_and_print()?;

        let oid = git_repo.get_tip_of_local_branch("main")?;

        let mut p = CliTester::new_git_with_remote_helper_from_dir(&git_repo.dir, ["push"]);
        cli_expect_nostr_fetch(&mut p)?;
        p.expect(format!("fetching {} ref list over filesystem...\r\n", source_path).as_str())?;
        p.expect("list: connecting...\r\n")?;
        p.expect_after_whitespace("merge commit ")?;
        // shorthand merge commit id appears in this gap
        p.expect_eventually(": create nostr proposal status event\r\n")?;
        // status updates printed here
        p.expect_eventually(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok((output, oid))
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

    let (output, oid) = cli_tester_handle.join().unwrap()?;

    assert_eq!(
        output,
        format!("   431b84e..{}  main -> main\r\n", &oid.to_string()[..7])
    );

    let new_events = r55
        .events
        .iter()
        .cloned()
        .collect::<HashSet<Event>>()
        .difference(&before)
        .cloned()
        .collect::<Vec<Event>>();

    assert_eq!(new_events.len(), 2, "{new_events:?}");

    let proposal = r55
        .events
        .iter()
        .find(|e| {
            e.tags()
                .iter()
                .find(|t| t.as_vec()[0].eq("branch-name"))
                .is_some_and(|t| t.as_vec()[1].eq(FEATURE_BRANCH_NAME_1))
        })
        .unwrap();

    let merge_status = new_events
        .iter()
        .find(|e| e.kind().eq(&Kind::GitStatusApplied))
        .unwrap();

    assert_eq!(
        oid.to_string(),
        merge_status
            .tags
            .iter()
            .find(|t| t.as_vec()[0].eq("merge-commit-id"))
            .unwrap()
            .as_vec()[1],
        "status sets correct merge-commit-id tag"
    );

    let proposal_tip = r55
        .events
        .iter()
        .filter(|e| {
            e.tags()
                .iter()
                .any(|t| t.as_vec()[1].eq(&proposal.id().to_string()))
                && e.kind().eq(&Kind::GitPatch)
        })
        .last()
        .unwrap();

    assert_eq!(
        proposal_tip.id().to_string(),
        merge_status
            .tags
            .iter()
            .find(|t| t.as_vec().len().eq(&4) && t.as_vec()[3].eq("mention"))
            .unwrap()
            .as_vec()[1],
        "status mentions proposal tip event \r\nmerge status:\r\n{}\r\nproposal tip:\r\n{}",
        merge_status.as_json(),
        proposal_tip.as_json(),
    );

    assert_eq!(
        proposal.id().to_string(),
        merge_status
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_vec()[1],
        "status tags proposal id as root \r\nmerge status:\r\n{}\r\nproposal:\r\n{}",
        merge_status.as_json(),
        proposal.as_json(),
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn push_2_commits_to_existing_proposal() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let source_path = source_git_repo.dir.to_str().unwrap().to_string();

    let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
        Relay::new(8051, None, None),
        Relay::new(8052, None, None),
        Relay::new(8053, None, None),
        Relay::new(8055, None, None),
        Relay::new(8056, None, None),
        Relay::new(8057, None, None),
    );
    r51.events = events.clone();
    r55.events = events.clone();

    #[allow(clippy::mutable_key_type)]
    let before = r55.events.iter().cloned().collect::<HashSet<Event>>();

    let cli_tester_handle = std::thread::spawn(move || -> Result<(String, String)> {
        let branch_name = get_proposal_branch_name_from_events(&events, FEATURE_BRANCH_NAME_1)?;

        let git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.checkout_remote_branch(&branch_name)?;

        std::fs::write(git_repo.dir.join("new.md"), "some content")?;
        git_repo.stage_and_commit("new.md")?;

        std::fs::write(git_repo.dir.join("new2.md"), "some content")?;
        git_repo.stage_and_commit("new2.md")?;

        let mut p = CliTester::new_git_with_remote_helper_from_dir(&git_repo.dir, ["push"]);
        cli_expect_nostr_fetch(&mut p)?;
        p.expect(format!("fetching {} ref list over filesystem...\r\n", source_path).as_str())?;
        p.expect("list: connecting...\r\n\r\r\r")?;
        p.expect(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok((output, branch_name))
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

    let (output, branch_name) = cli_tester_handle.join().unwrap()?;

    assert_eq!(
        output,
        format!("   eb5d678..7de5e41  {branch_name} -> {branch_name}\r\n").as_str(),
    );

    let new_events = r55
        .events
        .iter()
        .cloned()
        .collect::<HashSet<Event>>()
        .difference(&before)
        .cloned()
        .collect::<Vec<Event>>();
    assert_eq!(new_events.len(), 2);
    let first_new_patch = new_events
        .iter()
        .find(|e| e.content.contains("new.md"))
        .unwrap();
    let second_new_patch = new_events
        .iter()
        .find(|e| e.content.contains("new2.md"))
        .unwrap();
    assert!(
        first_new_patch.content.contains("[PATCH 3/4]"),
        "first patch labeled with         [PATCH 3/4]"
    );
    assert!(
        second_new_patch.content.contains("[PATCH 4/4]"),
        "second patch labeled with         [PATCH 4/4]"
    );

    let proposal = r55
        .events
        .iter()
        .find(|e| {
            e.tags()
                .iter()
                .find(|t| t.as_vec()[0].eq("branch-name"))
                .is_some_and(|t| t.as_vec()[1].eq(FEATURE_BRANCH_NAME_1))
        })
        .unwrap();

    assert_eq!(
        proposal.id().to_string(),
        first_new_patch
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_vec()[1],
        "first patch sets proposal id as root"
    );

    assert_eq!(
        first_new_patch.id().to_string(),
        second_new_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_vec()[1],
        "second new patch replies to the first new patch"
    );

    let previous_proposal_tip_event = r55
        .events
        .iter()
        .find(|e| {
            e.tags()
                .iter()
                .any(|t| t.as_vec()[1].eq(&proposal.id().to_string()))
                && e.content.contains("[PATCH 2/2]")
        })
        .unwrap();

    assert_eq!(
        previous_proposal_tip_event.id().to_string(),
        first_new_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_vec()[1],
        "first patch replies to the previous tip of proposal"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn force_push_creates_proposal_revision() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let source_path = source_git_repo.dir.to_str().unwrap().to_string();

    let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
        Relay::new(8051, None, None),
        Relay::new(8052, None, None),
        Relay::new(8053, None, None),
        Relay::new(8055, None, None),
        Relay::new(8056, None, None),
        Relay::new(8057, None, None),
    );
    r51.events = events.clone();
    r55.events = events.clone();

    #[allow(clippy::mutable_key_type)]
    let before = r55.events.iter().cloned().collect::<HashSet<Event>>();

    let cli_tester_handle = std::thread::spawn(move || -> Result<(String, String)> {
        let branch_name = get_proposal_branch_name_from_events(&events, FEATURE_BRANCH_NAME_1)?;

        let git_repo = clone_git_repo_with_nostr_url()?;
        let oid = git_repo.checkout_remote_branch(&branch_name)?;
        // remove last commit
        git_repo.checkout("main")?;
        git_repo.git_repo.branch(
            &branch_name,
            &git_repo.git_repo.find_commit(oid)?.parent(0)?,
            true,
        )?;
        git_repo.checkout(&branch_name)?;

        std::fs::write(git_repo.dir.join("new.md"), "some content")?;
        git_repo.stage_and_commit("new.md")?;

        std::fs::write(git_repo.dir.join("new2.md"), "some content")?;
        git_repo.stage_and_commit("new2.md")?;

        let mut p =
            CliTester::new_git_with_remote_helper_from_dir(&git_repo.dir, ["push", "--force"]);
        cli_expect_nostr_fetch(&mut p)?;
        p.expect(format!("fetching {} ref list over filesystem...\r\n", source_path).as_str())?;
        p.expect("list: connecting...\r\n")?;
        p.expect_after_whitespace(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok((output, branch_name))
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

    let (output, branch_name) = cli_tester_handle.join().unwrap()?;

    assert_eq!(
        output,
        format!(" + eb5d678...8a296c8 {branch_name} -> {branch_name} (forced update)\r\n").as_str(),
    );

    let new_events = r55
        .events
        .iter()
        .cloned()
        .collect::<HashSet<Event>>()
        .difference(&before)
        .cloned()
        .collect::<Vec<Event>>();
    assert_eq!(new_events.len(), 3);

    let proposal = r55
        .events
        .iter()
        .find(|e| {
            e.tags()
                .iter()
                .find(|t| t.as_vec()[0].eq("branch-name"))
                .is_some_and(|t| t.as_vec()[1].eq(FEATURE_BRANCH_NAME_1))
        })
        .unwrap();

    let revision_root_patch = new_events
        .iter()
        .find(|e| e.tags().iter().any(|t| t.as_vec()[1].eq("revision-root")))
        .unwrap();

    assert_eq!(
        proposal.id().to_string(),
        revision_root_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_vec()[1],
        "revision root patch replies to original proposal"
    );

    assert!(
        revision_root_patch.content.contains("[PATCH 1/3]"),
        "revision root labeled with    [PATCH 1/3] event: {revision_root_patch:?}",
    );

    let second_patch = new_events
        .iter()
        .find(|e| e.content.contains("new.md"))
        .unwrap();
    let third_patch = new_events
        .iter()
        .find(|e| e.content.contains("new2.md"))
        .unwrap();
    assert!(
        second_patch.content.contains("[PATCH 2/3]"),
        "second patch labeled with     [PATCH 2/3]"
    );
    assert!(
        third_patch.content.contains("[PATCH 3/3]"),
        "third patch labeled with     [PATCH 3/3]"
    );

    assert_eq!(
        revision_root_patch.id().to_string(),
        second_patch
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_vec()[1],
        "second patch sets revision id as root"
    );

    assert_eq!(
        second_patch.id().to_string(),
        third_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_vec()[1],
        "third patch replies to the second new patch"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn push_new_pr_branch_creates_proposal() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let source_path = source_git_repo.dir.to_str().unwrap().to_string();

    let (mut r51, mut r52, mut r53, mut r55, mut r56, mut r57) = (
        Relay::new(8051, None, None),
        Relay::new(8052, None, None),
        Relay::new(8053, None, None),
        Relay::new(8055, None, None),
        Relay::new(8056, None, None),
        Relay::new(8057, None, None),
    );
    r51.events = events.clone();
    r55.events = events.clone();

    #[allow(clippy::mutable_key_type)]
    let before = r55.events.iter().cloned().collect::<HashSet<Event>>();
    let branch_name = "pr/my-new-proposal";

    let cli_tester_handle = std::thread::spawn(move || -> Result<String> {
        let mut git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.delete_dir_on_drop = false;
        git_repo.create_branch(branch_name)?;
        git_repo.checkout(branch_name)?;

        std::fs::write(git_repo.dir.join("new.md"), "some content")?;
        git_repo.stage_and_commit("new.md")?;

        std::fs::write(git_repo.dir.join("new2.md"), "some content")?;
        git_repo.stage_and_commit("new2.md")?;

        let mut p = CliTester::new_git_with_remote_helper_from_dir(
            &git_repo.dir,
            ["push", "-u", "origin", branch_name],
        );
        cli_expect_nostr_fetch(&mut p)?;
        p.expect(format!("fetching {} ref list over filesystem...\r\n", source_path).as_str())?;
        p.expect("list: connecting...\r\n\r\r\r")?;
        p.expect(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok(output)
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

    let output = cli_tester_handle.join().unwrap()?;

    assert_eq!(
            output,
            format!(" * [new branch]      {branch_name} -> {branch_name}\r\nbranch '{branch_name}' set up to track 'origin/{branch_name}'.\r\n").as_str(),
        );

    let new_events = r55
        .events
        .iter()
        .cloned()
        .collect::<HashSet<Event>>()
        .difference(&before)
        .cloned()
        .collect::<Vec<Event>>();
    assert_eq!(new_events.len(), 2);

    let proposal = new_events
        .iter()
        .find(|e| e.tags().iter().any(|t| t.as_vec()[1].eq("root")))
        .unwrap();

    assert!(
        proposal.content.contains("new.md"),
        "first patch is proposal root"
    );

    assert!(
        proposal.content.contains("[PATCH 1/2]"),
        "proposal root labeled with[PATCH 1/2] event: {proposal:?}",
    );

    assert_eq!(
        proposal
            .tags()
            .iter()
            .find(|t| t.as_vec()[0].eq("branch-name"))
            .unwrap()
            .as_vec()[1],
        branch_name.replace("pr/", ""),
    );

    let second_patch = new_events
        .iter()
        .find(|e| e.content.contains("new2.md"))
        .unwrap();

    assert!(
        second_patch.content.contains("[PATCH 2/2]"),
        "second patch labeled with     [PATCH 2/2]"
    );

    assert_eq!(
        proposal.id().to_string(),
        second_patch
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_vec()[1],
        "second patch sets proposal id as root"
    );

    Ok(())
}
