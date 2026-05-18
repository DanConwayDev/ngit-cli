use super::*;

// NOTE: The other two scenarios that used to live in this file
// (fetch_downloads_speficied_commits_from_git_server and
// when_first_git_server_fails_::fetch_downloads_speficied_commits_from_second_git_server)
// have been ported to the new test harness and now live in
// tests/fetch_grasp.rs and tests/fetch_failover_grasp.rs respectively. They
// were removed from this legacy file in commit 90679fb.
//
// The proposal-fetch scenario below is retained because porting it requires a
// proposal-publishing scenario builder that does not yet exist in
// test_harness (legacy uses the heavyweight cli_tester_create_proposals
// helper). It must stay here until a harness-native replacement lands.

#[tokio::test]
#[serial]
async fn creates_commits_from_open_proposal_with_no_warnings_printed() -> Result<()> {
    let (events, _) = prep_source_repo_and_events_including_proposals().await?;

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
