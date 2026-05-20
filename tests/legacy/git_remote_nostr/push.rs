use git2::Signature;
use ngit::git_events::KIND_PULL_REQUEST;
use rstest::*;

use super::*;

#[tokio::test]
#[serial]
async fn push_2_commits_to_existing_proposal() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
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
        format!("   2d1b467..9d83ff4  {branch_name} -> {branch_name}\r\n").as_str(),
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
            e.tags
                .iter()
                .find(|t| t.as_slice()[0].eq("branch-name"))
                .is_some_and(|t| t.as_slice()[1].eq(FEATURE_BRANCH_NAME_1))
        })
        .unwrap();

    assert_eq!(
        proposal.id.to_string(),
        first_new_patch
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_slice()[1],
        "first patch sets proposal id as root"
    );

    assert_eq!(
        first_new_patch.id.to_string(),
        second_new_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_slice()[1],
        "second new patch replies to the first new patch"
    );

    let previous_proposal_tip_event = r55
        .events
        .iter()
        .find(|e| {
            e.tags
                .iter()
                .any(|t| t.as_slice()[1].eq(&proposal.id.to_string()))
                && e.content.contains("[PATCH 2/2]")
        })
        .unwrap();

    assert_eq!(
        previous_proposal_tip_event.id.to_string(),
        first_new_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_slice()[1],
        "first patch replies to the previous tip of proposal"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn force_push_creates_proposal_revision() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
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
        format!(" + 2d1b467...ead85e0 {branch_name} -> {branch_name} (forced update)\r\n").as_str(),
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
            e.tags
                .iter()
                .find(|t| t.as_slice()[0].eq("branch-name"))
                .is_some_and(|t| t.as_slice()[1].eq(FEATURE_BRANCH_NAME_1))
        })
        .unwrap();

    let revision_root_patch = new_events
        .iter()
        .find(|e| {
            e.tags
                .iter()
                .any(|t| ["revision-root", "root-revision"].contains(&t.as_slice()[1].as_str()))
        })
        .unwrap();

    assert_eq!(
        proposal.id.to_string(),
        revision_root_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_slice()[1],
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
        revision_root_patch.id.to_string(),
        second_patch
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_slice()[1],
        "second patch sets revision id as root"
    );

    assert_eq!(
        second_patch.id.to_string(),
        third_patch
            .tags
            .iter()
            .find(|t| t.is_reply())
            .unwrap()
            .as_slice()[1],
        "third patch replies to the second new patch"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn push_new_pr_branch_creates_proposal() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
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
        .find(|e| e.tags.iter().any(|t| t.as_slice()[1].eq("root")))
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
            .tags
            .iter()
            .find(|t| t.as_slice()[0].eq("branch-name"))
            .unwrap()
            .as_slice()[1],
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
        proposal.id.to_string(),
        second_patch
            .tags
            .iter()
            .find(|t| t.is_root())
            .unwrap()
            .as_slice()[1],
        "second patch sets proposal id as root"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn push_new_pr_branch_with_title_description_options_creates_pr_with_custom_title_description()
-> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
    let branch_name = "pr/my-pr-with-title";

    let cli_tester_handle = std::thread::spawn(move || -> Result<String> {
        let mut git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.delete_dir_on_drop = false;
        git_repo.create_branch(branch_name)?;
        git_repo.checkout(branch_name)?;

        let large_content = "x".repeat(70 * 1024);
        std::fs::write(git_repo.dir.join("large_file.txt"), large_content)?;
        git_repo.stage_and_commit("large_file.txt")?;

        let mut p = CliTester::new_git_with_remote_helper_from_dir(
            &git_repo.dir,
            [
                "push",
                "--push-option=title=Custom PR Title",
                "--push-option=description=Custom PR description here",
                "-u",
                "origin",
                branch_name,
            ],
        );
        cli_expect_nostr_fetch(&mut p)?;
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok(output)
    });
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
    assert_eq!(new_events.len(), 1, "should create exactly 1 PR event");

    let pr_event = new_events.first().unwrap();

    assert!(
        pr_event.kind.eq(&KIND_PULL_REQUEST),
        "event should be a PR event"
    );

    let title_tag = pr_event.tags.iter().find(|t| t.as_slice()[0].eq("subject"));
    assert!(
        title_tag.is_some(),
        "PR event should have a subject tag for title"
    );
    assert_eq!(
        title_tag.unwrap().as_slice()[1],
        "Custom PR Title",
        "title should match push-option"
    );

    assert_eq!(
        pr_event.content, "Custom PR description here",
        "description should match push-option"
    );

    let branch_name_tag = pr_event
        .tags
        .iter()
        .find(|t| t.as_slice()[0].eq("branch-name"));
    assert!(
        branch_name_tag.is_some(),
        "PR event should have a branch-name tag"
    );
    assert_eq!(
        branch_name_tag.unwrap().as_slice()[1],
        branch_name.replace("pr/", ""),
        "branch-name tag should match"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn push_with_escaped_newlines_in_description_creates_pr_with_multiline_description()
-> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
    let branch_name = "pr/my-pr-multiline";

    let cli_tester_handle = std::thread::spawn(move || -> Result<String> {
        let mut git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.delete_dir_on_drop = false;
        git_repo.create_branch(branch_name)?;
        git_repo.checkout(branch_name)?;

        let large_content = "x".repeat(70 * 1024);
        std::fs::write(git_repo.dir.join("large_file.txt"), large_content)?;
        git_repo.stage_and_commit("large_file.txt")?;

        // Use \\n in the push-option value — the two-character escape sequence
        let mut p = CliTester::new_git_with_remote_helper_from_dir(
            &git_repo.dir,
            [
                "push",
                "--push-option=title=Multiline PR",
                r"--push-option=description=First line\n\nSecond paragraph\nThird line",
                "-u",
                "origin",
                branch_name,
            ],
        );
        cli_expect_nostr_fetch(&mut p)?;
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok(output)
    });
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
    assert_eq!(new_events.len(), 1, "should create exactly 1 PR event");

    let pr_event = new_events.first().unwrap();

    assert!(
        pr_event.kind.eq(&KIND_PULL_REQUEST),
        "event should be a PR event"
    );

    let title_tag = pr_event.tags.iter().find(|t| t.as_slice()[0].eq("subject"));
    assert!(
        title_tag.is_some(),
        "PR event should have a subject tag for title"
    );
    assert_eq!(
        title_tag.unwrap().as_slice()[1],
        "Multiline PR",
        "title should match push-option"
    );

    // The \\n sequences should have been decoded into real newlines
    assert_eq!(
        pr_event.content, "First line\n\nSecond paragraph\nThird line",
        "description should contain real newlines from escaped \\n sequences"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn force_push_to_existing_patch_series_with_title_description_options_creates_patches_with_custom_cover_letter()
-> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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

    let cli_tester_handle = std::thread::spawn(move || -> Result<String> {
        let branch_name = get_proposal_branch_name_from_events(&events, FEATURE_BRANCH_NAME_1)?;

        let git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.checkout_remote_branch(&branch_name)?;

        // Create two new commits to replace the existing patch series
        std::fs::write(git_repo.dir.join("new1.txt"), "content 1")?;
        git_repo.stage_and_commit("add new1")?;

        std::fs::write(git_repo.dir.join("new2.txt"), "content 2")?;
        git_repo.stage_and_commit("add new2")?;

        // Force push with custom title and description
        let mut p = CliTester::new_git_with_remote_helper_from_dir(
            &git_repo.dir,
            [
                "push",
                "--force",
                "--push-option=title=Custom Patch Title",
                "--push-option=description=Custom patch series description",
                "origin",
                &branch_name,
            ],
        );
        cli_expect_nostr_fetch(&mut p)?;
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok(output)
    });
    let _ = join!(
        r51.listen_until_close(),
        r52.listen_until_close(),
        r53.listen_until_close(),
        r55.listen_until_close(),
        r56.listen_until_close(),
        r57.listen_until_close(),
    );

    let output = cli_tester_handle.join().unwrap()?;

    // Verify the output shows the branch was pushed
    assert!(!output.is_empty(), "should have output from push");

    let new_events = r55
        .events
        .iter()
        .cloned()
        .collect::<HashSet<Event>>()
        .difference(&before)
        .cloned()
        .collect::<Vec<Event>>();

    // Should create 5 events: 1 cover letter + 4 patches (2 existing + 2 new)
    assert_eq!(
        new_events.len(),
        5,
        "should create 1 cover letter + 4 patch events"
    );

    // Find the cover letter
    let cover_letter = new_events
        .iter()
        .find(|e| e.kind.eq(&Kind::GitPatch) && e.content.contains("[PATCH 0/4]"))
        .expect("should have a cover letter event");

    // Check that the cover letter contains the custom title and description
    assert!(
        cover_letter.content.contains("Custom Patch Title"),
        "cover letter should contain custom title"
    );

    assert!(
        cover_letter
            .content
            .contains("Custom patch series description"),
        "cover letter content should contain custom description"
    );

    // Verify patches exist
    let patches: Vec<&Event> = new_events
        .iter()
        .filter(|e| {
            e.kind.eq(&Kind::GitPatch) && !e.content.contains("[PATCH 0/4]") // Exclude cover letter
        })
        .collect();

    assert_eq!(patches.len(), 4, "should have 4 patch events");

    Ok(())
}

#[tokio::test]
#[serial]
async fn push_new_pr_branch_with_multiple_commits_sets_merge_base_to_main_tip() -> Result<()> {
    let (events, source_git_repo) = prep_source_repo_and_events_including_proposals().await?;
    let _source_path = source_git_repo.dir.to_str().unwrap().to_string();

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
    let branch_name = "pr/multi-commit-pr";

    let cli_tester_handle = std::thread::spawn(move || -> Result<String> {
        let mut git_repo = clone_git_repo_with_nostr_url()?;
        git_repo.delete_dir_on_drop = false;

        // Record the main tip — this should become the merge-base in the PR event
        let main_tip = git_repo.get_tip_of_local_branch("main")?.to_string();

        git_repo.create_branch(branch_name)?;
        git_repo.checkout(branch_name)?;

        // Add two large commits so the push is forced into PR (not patch) mode
        let large_content = "x".repeat(70 * 1024);
        std::fs::write(git_repo.dir.join("large1.txt"), &large_content)?;
        git_repo.stage_and_commit("add large1")?;

        std::fs::write(git_repo.dir.join("large2.txt"), &large_content)?;
        git_repo.stage_and_commit("add large2")?;

        let mut p = CliTester::new_git_with_remote_helper_from_dir(
            &git_repo.dir,
            ["push", "-u", "origin", branch_name],
        );
        cli_expect_nostr_fetch(&mut p)?;
        p.expect("git servers: listing refs...\r\n")?;
        p.expect_eventually_and_print(format!("To {}\r\n", get_nostr_remote_url()?).as_str())?;
        let output = p.expect_end_eventually()?;

        for p in [51, 52, 53, 55, 56, 57] {
            relay::shutdown_relay(8000 + p)?;
        }

        Ok(format!("{main_tip}\n{output}"))
    });
    let _ = join!(
        r51.listen_until_close(),
        r52.listen_until_close(),
        r53.listen_until_close(),
        r55.listen_until_close(),
        r56.listen_until_close(),
        r57.listen_until_close(),
    );

    let result = cli_tester_handle.join().unwrap()?;
    let (main_tip, _output) = result.split_once('\n').unwrap();

    let new_events = r55
        .events
        .iter()
        .cloned()
        .collect::<HashSet<Event>>()
        .difference(&before)
        .cloned()
        .collect::<Vec<Event>>();
    assert_eq!(new_events.len(), 1, "should create exactly 1 PR event");

    let pr_event = new_events.first().unwrap();
    assert!(
        pr_event.kind.eq(&KIND_PULL_REQUEST),
        "event should be a PR event"
    );

    let merge_base_tag = pr_event
        .tags
        .iter()
        .find(|t| t.as_slice()[0].eq("merge-base"));
    assert!(
        merge_base_tag.is_some(),
        "PR event should have a merge-base tag"
    );
    assert_eq!(
        merge_base_tag.unwrap().as_slice()[1],
        main_tip,
        "merge-base should be the main branch tip at the time of branching, not the parent of the PR tip"
    );

    Ok(())
}

mod push_from_another_maintainer {

    // TODO that has issued announcement
    // - that is listed by trusted maintainer - succeeds (covered by above
    //   tests)
    // - that isn't listed by trusted maintainer - fails
    // TODO that hasn't yet issued announcement
    // - that is listed by trusted maintainer - fails
}
