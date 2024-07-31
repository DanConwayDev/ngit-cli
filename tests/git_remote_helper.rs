use std::collections::HashSet;

use anyhow::{Context, Result};
use futures::join;
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{Kind, ToBech32};
use relay::Relay;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

static NOSTR_REMOTE_NAME: &str = "nostr";

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
    let mut config = test_repo
        .git_repo
        .config()
        .context("cannot open git config")?;
    config.set_str("nostr.nsec", TEST_KEY_1_NSEC)?;
    config.set_str("nostr.npub", TEST_KEY_1_NPUB)?;
    test_repo.add_remote(NOSTR_REMOTE_NAME, &get_nostr_remote_url()?)?;
    test_repo.populate()?;
    Ok(test_repo)
}

fn cli_tester(git_repo: &GitTestRepo) -> CliTester {
    CliTester::new_remote_helper_from_dir(&git_repo.dir, &get_nostr_remote_url().unwrap())
}

fn cli_tester_after_fetch(git_repo: &GitTestRepo) -> Result<CliTester> {
    let mut p = cli_tester(git_repo);
    p.expect("fetching updates...\r\n")?;
    p.expect_eventually("updates")?; // some updates
    p.expect_eventually("\r\n")?;
    Ok(p)
}

mod initially_runs_fetch {

    use super::*;
    async fn async_run_test() -> Result<()> {
        let source_git_repo = prep_git_repo()?;
        let git_repo = prep_git_repo()?;
        let events = vec![
            generate_test_key_1_metadata_event("fred"),
            generate_test_key_1_relay_list_event(),
            generate_repo_ref_event_with_git_server(source_git_repo.dir.to_str().unwrap()),
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

    #[tokio::test]
    #[serial]
    async fn runs_fetch_and_reports() -> Result<()> {
        async_run_test().await
    }
}

mod list {

    use super::*;

    async fn async_run_test() -> Result<()> {
        let source_git_repo = prep_git_repo()?;
        std::fs::write(source_git_repo.dir.join("commit.md"), "some content")?;
        let main_commit_id = source_git_repo.stage_and_commit("commit.md")?;

        source_git_repo.create_branch("vnext")?;
        source_git_repo.checkout("vnext")?;
        std::fs::write(source_git_repo.dir.join("vnext.md"), "some content")?;
        let vnext_commit_id = source_git_repo.stage_and_commit("vnext.md")?;

        let head_commit_id = source_git_repo.checkout("main")?;

        let git_repo = prep_git_repo()?;
        let events = vec![
            generate_test_key_1_metadata_event("fred"),
            generate_test_key_1_relay_list_event(),
            generate_repo_ref_event_with_git_server(source_git_repo.dir.to_str().unwrap()),
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
            // println!("{}", p.expect_eventually("\r\n\r\n")?);
            assert_eq!(
                p.expect_eventually("\r\n\r\n")?
                    .split("\r\n")
                    .map(|e| e.to_string())
                    .collect::<HashSet<String>>(),
                HashSet::from([
                    format!("{} HEAD", head_commit_id),
                    format!("{} refs/heads/main", main_commit_id),
                    format!("{} refs/heads/vnext", vnext_commit_id),
                ]),
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
    async fn lists_head_and_2_branches_and_commit_ids_from_git_server() -> Result<()> {
        async_run_test().await
    }
}

mod fetch {

    use super::*;

    async fn async_run_test() -> Result<()> {
        let source_git_repo = prep_git_repo()?;
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
            generate_repo_ref_event_with_git_server(source_git_repo.dir.to_str().unwrap()),
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
            p.expect("\r\n")?;

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

    #[tokio::test]
    #[serial]
    async fn fetch_downloads_speficied_branch_ref_commits_from_git_server() -> Result<()> {
        async_run_test().await
    }
}

mod push {

    use super::*;

    mod two_branches_in_batch {

        use super::*;

        mod git_server_updated {

            use super::*;

            async fn async_run_test() -> Result<()> {
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
                    generate_repo_ref_event_with_git_server(source_git_repo.dir.to_str().unwrap()),
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

                    let mut p = cli_tester_after_fetch(&git_repo)?;
                    p.send_line("push refs/heads/main:refs/heads/main")?;
                    p.send_line("push refs/heads/vnext:refs/heads/vnext")?;
                    p.send_line("")?;
                    p.expect("ok refs/heads/main\r\n")?;
                    p.expect("ok refs/heads/vnext\r\n")?;
                    p.expect("\r\n")?;

                    assert_eq!(
                        source_git_repo.get_tip_of_local_branch("main")?,
                        main_commit_id
                    );

                    assert_eq!(
                        source_git_repo.get_tip_of_local_branch("vnext")?,
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
            async fn push_updates_ref_on_git_server() -> Result<()> {
                async_run_test().await
            }
        }
    }
}
