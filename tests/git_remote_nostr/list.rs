use super::*;

mod without_state_announcement {

    use super::*;

    #[tokio::test]
    #[serial]
    async fn lists_head_and_2_branches_and_commit_ids_from_git_server() -> Result<()> {
        let source_git_repo = prep_git_repo()?;
        let source_path = source_git_repo.dir.to_str().unwrap().to_string();
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
            p.expect(format!("fetching {} ref list over filesystem...\r\n", source_path).as_str())?;
            p.expect("list: connecting...\r\n\r\r\r")?;
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
                    format!("{} refs/heads/main", main_commit_id),
                    format!("{} refs/heads/vnext", vnext_commit_id),
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
            let source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
                p.expect(
                    format!("fetching {} ref list over filesystem...\r\n", source_path).as_str(),
                )?;
                p.expect("list: connecting...\r\n\r\r\r")?;
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
                        format!("{} refs/heads/main", main_commit_id),
                        format!("{} refs/heads/example-branch", example_commit_id),
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
    mod when_announcement_doesnt_match_git_server {

        use super::*;

        #[tokio::test]
        #[serial]
        async fn anouncement_state_is_used() -> Result<()> {
            let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
            let source_path = source_git_repo.dir.to_str().unwrap().to_string();
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
                p.expect(
                    format!("fetching {} ref list over filesystem...\r\n", source_path).as_str(),
                )?;
                p.expect("list: connecting...\r\n\r\r\r")?;
                p.expect(
                    format!(
                        "WARNING: {} refs/heads/main is out of sync with nostr \r\n",
                        source_path
                    )
                    .as_str(),
                )?;

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
                        format!("{} refs/heads/main", main_original_commit_id),
                        format!("{} refs/heads/example-branch", example_commit_id),
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
            let source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
                p.expect(
                    format!("fetching {} ref list over filesystem...\r\n", source_path).as_str(),
                )?;
                p.expect("list: connecting...\r\n\r\r\r")?;
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
                pr_refs.push(format!(
                    "{} refs/heads/{}",
                    proposal_creation_repo.get_tip_of_local_branch(name)?,
                    get_proposal_branch_name_from_events(&r55.events, name)?,
                ));
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
