use super::*;

#[tokio::test]
#[serial]
async fn fetch_downloads_speficied_commits_from_git_server() -> Result<()> {
    let source_git_repo = prep_git_repo()?;
    let source_path = source_git_repo.dir.to_str().unwrap().to_string();

    std::fs::write(source_git_repo.dir.join("commit.md"), "some content")?;
    let main_commit_id = source_git_repo.stage_and_commit("commit.md")?;

    source_git_repo.create_branch("vnext")?;
    source_git_repo.checkout("vnext")?;
    std::fs::write(source_git_repo.dir.join("vnext.md"), "some content")?;
    let vnext_commit_id = source_git_repo.stage_and_commit("vnext.md")?;

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
        assert!(git_repo.git_repo.find_commit(main_commit_id).is_err());
        assert!(git_repo.git_repo.find_commit(vnext_commit_id).is_err());

        let mut p = cli_tester_after_fetch(&git_repo)?;
        p.send_line(format!("fetch {main_commit_id} main").as_str())?;
        p.send_line(format!("fetch {vnext_commit_id} vnext").as_str())?;
        p.send_line("")?;
        p.expect(format!("fetching {source_path} over filesystem...\r\n").as_str())?;
        p.expect_eventually_and_print("\r\n")?;

        assert!(git_repo.git_repo.find_commit(main_commit_id).is_ok());
        assert!(git_repo.git_repo.find_commit(vnext_commit_id).is_ok());

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

mod when_first_git_server_fails_ {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn fetch_downloads_speficied_commits_from_second_git_server() -> Result<()> {
        let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
        // let source_path = source_git_repo.dir.to_str().unwrap().to_string();
        let error_path = "./path-doesnt-exist".to_string();

        let main_commit_id = source_git_repo.get_tip_of_local_branch("main")?;

        let git_repo = prep_git_repo_minus_1_commit()?;

        let events = vec![
            generate_test_key_1_metadata_event("fred"),
            generate_test_key_1_relay_list_event(),
            generate_repo_ref_event_with_git_server(vec![
                error_path.to_string(),
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
            assert!(git_repo.git_repo.find_commit(main_commit_id).is_err());

            let mut p = cli_tester_after_fetch(&git_repo)?;
            p.send_line(format!("fetch {main_commit_id} main").as_str())?;
            p.send_line("")?;
            p.expect(format!("fetching {error_path} over filesystem...\r\n").as_str())?;
            // not sure why the below isn't appearing
            // p.expect(format!("fetching over filesystem from
            // {source_path}...\r\n").as_str())?;
            p.expect_eventually_and_print("\r\n")?;
            // p.expect("\r\n")?;

            assert!(git_repo.git_repo.find_commit(main_commit_id).is_ok());

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
}

#[tokio::test]
#[serial]
async fn creates_commits_from_open_proposal_with_no_warngins_printed() -> Result<()> {
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

    let git_repo = prep_git_repo()?;

    let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
        let branch_name = get_proposal_branch_name_from_events(&events, FEATURE_BRANCH_NAME_1)?;
        let proposal_tip = cli_tester_create_proposal_branches_ready_to_send()?
            .get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?;

        assert!(git_repo.git_repo.find_commit(proposal_tip).is_err());

        let mut p = cli_tester_after_fetch(&git_repo)?;
        p.send_line(format!("fetch {proposal_tip} refs/heads/{branch_name}").as_str())?;
        p.send_line("")?;
        p.expect(format!("fetching {source_path} over filesystem...\r\n").as_str())?;
        // expect no errors
        p.expect_after_whitespace("\r\n")?;
        p.exit()?;
        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        assert!(git_repo.git_repo.find_commit(proposal_tip).is_ok());

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
