use std::{collections::HashSet, env::current_dir};

use anyhow::{Context, Result};
use futures::join;
use git2::Oid;
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{secp256k1::rand, Event, JsonUtil, Kind, ToBech32};
use relay::Relay;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

static NOSTR_REMOTE_NAME: &str = "nostr";
static STATE_KIND: nostr::Kind = Kind::Custom(30618);

fn get_nostr_remote_url() -> Result<String> {
    let repo_event = generate_repo_ref_event();
    let naddr = Coordinate {
        kind: Kind::GitRepoAnnouncement,
        public_key: repo_event.author(),
        identifier: repo_event.identifier().unwrap().to_string(),
        relays: vec![
            "ws://localhost:8055".to_string(),
            "ws://localhost:8056".to_string(),
        ],
    }
    .to_bech32()?;
    Ok(format!("nostr://{naddr}"))
}

fn prep_git_repo() -> Result<GitTestRepo> {
    let test_repo = GitTestRepo::without_repo_in_git_config();
    set_git_nostr_login_config(&test_repo)?;
    test_repo.add_remote(NOSTR_REMOTE_NAME, &get_nostr_remote_url()?)?;
    test_repo.populate()?;
    Ok(test_repo)
}

fn set_git_nostr_login_config(test_repo: &GitTestRepo) -> Result<()> {
    let mut config = test_repo
        .git_repo
        .config()
        .context("cannot open git config")?;
    config.set_str("nostr.nsec", TEST_KEY_2_NSEC)?;
    config.set_str("nostr.npub", TEST_KEY_2_NPUB)?;
    config.set_str("user.name", "test name")?;
    config.set_str("user.email", "test@test.com")?;
    config.set_bool("commit.gpgSign", false)?;
    Ok(())
}

fn clone_git_repo_with_nostr_url() -> Result<GitTestRepo> {
    let path = current_dir()?.join(format!("tmpgit-clone{}", rand::random::<u64>()));
    std::fs::create_dir(path.clone())?;
    CliTester::new_git_with_remote_helper_from_dir(&path, ["clone", &get_nostr_remote_url()?, "."])
        .expect_end_eventually_and_print()?;
    let test_repo = GitTestRepo::open(&path)?;
    set_git_nostr_login_config(&test_repo)?;
    Ok(test_repo)
}

fn prep_git_repo_minus_1_commit() -> Result<GitTestRepo> {
    let test_repo = GitTestRepo::without_repo_in_git_config();
    set_git_nostr_login_config(&test_repo)?;
    test_repo.add_remote(NOSTR_REMOTE_NAME, &get_nostr_remote_url()?)?;
    test_repo.populate_minus_1()?;
    Ok(test_repo)
}

fn cli_tester(git_repo: &GitTestRepo) -> CliTester {
    CliTester::new_remote_helper_from_dir(&git_repo.dir, &get_nostr_remote_url().unwrap())
}

fn cli_tester_after_fetch(git_repo: &GitTestRepo) -> Result<CliTester> {
    let mut p = cli_tester(git_repo);
    cli_expect_nostr_fetch(&mut p)?;
    Ok(p)
}

fn cli_expect_nostr_fetch(p: &mut CliTester) -> Result<()> {
    p.expect("nostr: fetching...\r\n")?;
    p.expect_eventually("updates")?; // some updates
    p.expect_eventually("\r\n")?;
    Ok(())
}

/// git runs `list for-push` before `push`. in `push` we use the git server
/// remote refs downloaded by `list` to assess how to push to git servers.
/// we are therefore running it this way in our tests
fn cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(
    git_repo: &GitTestRepo,
) -> Result<CliTester> {
    let mut p = cli_tester_after_fetch(git_repo)?;

    p.send_line("list for-push")?;
    p.expect_eventually_and_print("\r\n\r\n")?;
    Ok(p)
}

async fn generate_repo_with_state_event() -> Result<(nostr::Event, GitTestRepo)> {
    let git_repo = prep_git_repo()?;
    git_repo.create_branch("example-branch")?;
    let main_commit_id = git_repo.get_tip_of_local_branch("main")?.to_string();
    // TODO recreate_as_bare isn't creating other branches
    let source_git_repo = GitTestRepo::recreate_as_bare(&git_repo)?;
    let example_commit_id = source_git_repo
        .get_tip_of_local_branch("example-branch")?
        .to_string();
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
        p.send_line("")?;
        // p.expect("ok refs/heads/main\r\n")?;
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
        state_event
            .tags
            .iter()
            .filter(|t| t.kind().to_string().as_str().ne("d"))
            .map(|t| t.as_vec().to_vec())
            .collect::<HashSet<Vec<String>>>(),
        HashSet::from([
            vec!["HEAD".to_string(), "ref: refs/heads/main".to_string()],
            vec!["refs/heads/main".to_string(), main_commit_id.clone(),],
            vec!["refs/heads/example-branch".to_string(), example_commit_id,],
        ]),
    );

    // wait for bigger timestamp
    std::thread::sleep(std::time::Duration::from_millis(1000));

    Ok((state_event.clone(), source_git_repo))
}

async fn prep_source_repo_and_events_including_proposals()
-> Result<(Vec<nostr::Event>, GitTestRepo)> {
    let (state_event, source_git_repo) = generate_repo_with_state_event().await?;
    let source_path = source_git_repo.dir.to_str().unwrap().to_string();

    let events = vec![
        generate_test_key_1_metadata_event("fred"),
        generate_test_key_1_relay_list_event(),
        generate_repo_ref_event_with_git_server(vec![source_path.to_string()]),
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
        cli_tester_create_proposals()?;
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

    Ok((r55.events, source_git_repo))
}

mod initially_runs_fetch {

    use super::*;

    #[tokio::test]
    #[serial]
    async fn runs_fetch_and_reports() -> Result<()> {
        let source_git_repo = prep_git_repo()?;
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

        // // check relay had the right number of events
        let cli_tester_handle = std::thread::spawn(move || -> Result<()> {
            let mut p = cli_tester_after_fetch(&git_repo)?;
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

mod list {

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
                p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
                let example_commit_id =
                    source_git_repo.get_tip_of_local_branch("example-branch")?;

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
                    p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
                let example_commit_id =
                    source_git_repo.get_tip_of_local_branch("example-branch")?;

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
                    p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
                let example_commit_id =
                    source_git_repo.get_tip_of_local_branch("example-branch")?;

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
                    p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
}

mod fetch {

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
            p.expect(format!("fetching from {source_path}...\r\n").as_str())?;
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
                p.expect(format!("fetching from {error_path}...\r\n").as_str())?;
                // not sure why the below isn't appearing
                // p.expect(format!("fetching from {source_path}...\r\n").as_str())?;
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
            p.expect(format!("fetching from {source_path}...\r\n").as_str())?;
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
}

mod push {

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

                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;

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

                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
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

                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;

                p.send_line("push refs/heads/main:refs/heads/main")?;
                p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
                p.send_line("")?;
                p.expect("ok refs/heads/main\r\n")?;
                p.expect("ok refs/heads/vnext\r\n")?;
                p.expect("\r\n")?;
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
                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
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
                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
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

                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
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

                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
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
                let mut p =
                    cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(&git_repo)?;
                p.send_line("push :refs/heads/vnext")?;
                p.send_line("")?;
                // let res = p.expect_eventually("\r\n\r\n")?;
                // println!("{res}");
                p.expect("ok refs/heads/vnext\r\n")?;
                p.expect("\r\n")?;
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
                    p.expect("\r\n")?;
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
                        let mut p = cli_tester_after_nostr_fetch_and_sent_list_for_push_responds(
                            &git_repo,
                        )?;
                        p.send_line("push :refs/heads/example-branch")?;
                        p.send_line("")?;
                        p.expect("ok refs/heads/example-branch\r\n")?;
                        p.expect("\r\n")?;
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
            p.expect("\r\n")?;
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
            p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;

            p.expect("merge commit ")?;
            // shorthand merge commit id appears in this gap
            p.expect_eventually(": create nostr proposal status event\r\n")?;
            p.expect(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
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
            p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
            p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
            format!(" + eb5d678...8a296c8 {branch_name} -> {branch_name} (forced update)\r\n")
                .as_str(),
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
            p.expect(format!("fetching refs list: {}...\r\n\r", source_path).as_str())?;
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
}
