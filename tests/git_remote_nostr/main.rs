use std::{collections::HashSet, env::current_dir};

use anyhow::{Context, Result};
use futures::join;
use git2::Oid;
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{secp256k1::rand, Event, JsonUtil, Kind, ToBech32};
use relay::Relay;
use serial_test::serial;
use test_utils::{git::GitTestRepo, *};

mod fetch;
mod list;
mod push;

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
