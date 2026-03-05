use anyhow::Result;
use futures::join;
use serial_test::serial;
use test_utils::{git::GitTestRepo, relay::Relay, *};

/// Run `ngit pr list --json --offline` in `dir` and return the nevent id for
/// the proposal whose branch-name matches `branch_name_in_event`.
fn get_proposal_id_for_branch(dir: &std::path::Path, branch_name_in_event: &str) -> Result<String> {
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("ngit"))
        .env("NGITTEST", "TRUE")
        .current_dir(dir)
        .args([
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "pr",
            "list",
            "--json",
            "--offline",
        ])
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let proposals: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse pr list json: {e}\nstdout: {stdout}"))?;
    let entry = proposals
        .iter()
        .find(|p| {
            p["branch"]
                .as_str()
                .map(|b| b.starts_with(&format!("pr/{branch_name_in_event}(")))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no proposal found for branch {branch_name_in_event} in: {stdout}"
            )
        })?;
    Ok(entry["id"].as_str().unwrap_or_default().to_string())
}

/// Run `ngit pr checkout --offline <id>` (cache must already be populated).
fn run_pr_checkout(test_repo: &GitTestRepo, branch_name_in_event: &str) -> Result<()> {
    run_pr_checkout_with_args(test_repo, branch_name_in_event, &["--offline"])
}

/// Run `ngit pr checkout --force --offline <id>` (cache must already be populated).
fn run_pr_checkout_force(test_repo: &GitTestRepo, branch_name_in_event: &str) -> Result<()> {
    run_pr_checkout_with_args(test_repo, branch_name_in_event, &["--force", "--offline"])
}

fn run_pr_checkout_with_args(
    test_repo: &GitTestRepo,
    branch_name_in_event: &str,
    extra_args: &[&str],
) -> Result<()> {
    let proposal_id = get_proposal_id_for_branch(&test_repo.dir, branch_name_in_event)?;
    let mut args = vec![
        "--nsec",
        TEST_KEY_1_NSEC,
        "--password",
        TEST_PASSWORD,
        "--disable-cli-spinners",
        "pr",
        "checkout",
    ];
    args.extend_from_slice(extra_args);
    args.push(&proposal_id);
    // Use std::process::Command directly (not CliTester/rexpect) so that a
    // non-zero exit code is reliably detected without PTY timeout issues.
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("ngit"))
        .env("NGITTEST", "TRUE")
        .current_dir(&test_repo.dir)
        .args(&args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("ngit pr checkout exited with {status}")
    }
}

/// Spin up the standard 5-relay set used by all tests in this file.
/// Returns the five relay handles in port order (51,52,53,55,56).
#[allow(clippy::type_complexity)]
fn make_relays() -> (
    Relay<'static>,
    Relay<'static>,
    Relay<'static>,
    Relay<'static>,
    Relay<'static>,
) {
    let mut r51 = Relay::new(8051, None, None);
    let r52 = Relay::new(8052, None, None);
    let r53 = Relay::new(8053, None, None);
    let mut r55 = Relay::new(8055, None, None);
    let r56 = Relay::new(8056, None, None);

    r51.events.push(generate_test_key_1_relay_list_event());
    r51.events.push(generate_test_key_1_metadata_event("fred"));
    r51.events.push(generate_repo_ref_event());

    r55.events.push(generate_repo_ref_event());
    r55.events.push(generate_test_key_1_metadata_event("fred"));
    r55.events.push(generate_test_key_1_relay_list_event());

    (r51, r52, r53, r55, r56)
}

fn shutdown_relays() -> Result<()> {
    for port in [51u64, 52, 53, 55, 56] {
        relay::shutdown_relay(8000 + port)?;
    }
    Ok(())
}

mod when_proposal_branch_doesnt_exist {
    use super::*;

    async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = make_relays();

        let cli_tester_handle =
            std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                let originating_repo = cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                use_ngit_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                shutdown_relays()?;
                Ok((originating_repo, test_repo))
            });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_created_with_correct_name() -> Result<()> {
        let (_, test_repo) = prep_and_run().await?;
        let expected_branch = get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?;
        assert!(
            test_repo.get_local_branch_names()?.contains(&expected_branch),
            "expected branch {expected_branch} to exist"
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_checked_out() -> Result<()> {
        let (_, test_repo) = prep_and_run().await?;
        assert_eq!(
            get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?,
            test_repo.get_checked_out_branch_name()?,
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_tip_is_most_recent_patch() -> Result<()> {
        let (originating_repo, test_repo) = prep_and_run().await?;
        assert_eq!(
            originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            test_repo.get_tip_of_local_branch(
                &get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?
            )?,
        );
        Ok(())
    }
}

mod when_proposal_branch_exists_and_is_up_to_date {
    use super::*;

    async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = make_relays();

        let cli_tester_handle =
            std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                let originating_repo = cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                // first checkout creates the branch
                use_ngit_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;
                test_repo.checkout("main")?;

                // second checkout: branch already exists and is up to date
                run_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                shutdown_relays()?;
                Ok((originating_repo, test_repo))
            });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_checked_out() -> Result<()> {
        let (_, test_repo) = prep_and_run().await?;
        assert_eq!(
            get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?,
            test_repo.get_checked_out_branch_name()?,
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_tip_unchanged() -> Result<()> {
        let (originating_repo, test_repo) = prep_and_run().await?;
        assert_eq!(
            originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            test_repo.get_tip_of_local_branch(
                &get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?
            )?,
        );
        Ok(())
    }
}

mod when_proposal_branch_exists_and_is_behind {
    use super::*;

    async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = make_relays();

        let cli_tester_handle =
            std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                let originating_repo = cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                use_ngit_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                // wind the local branch back one commit so it's behind
                remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main(&test_repo)?;

                // checkout again — should fast-forward to the latest patch
                run_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                shutdown_relays()?;
                Ok((originating_repo, test_repo))
            });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_checked_out() -> Result<()> {
        let (_, test_repo) = prep_and_run().await?;
        assert_eq!(
            get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?,
            test_repo.get_checked_out_branch_name()?,
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_tip_is_most_recent_patch() -> Result<()> {
        let (originating_repo, test_repo) = prep_and_run().await?;
        assert_eq!(
            originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            test_repo.get_tip_of_local_branch(
                &get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?
            )?,
        );
        Ok(())
    }
}

mod when_proposal_branch_has_local_amendments {
    use super::*;

    async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = make_relays();

        let cli_tester_handle =
            std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                let originating_repo = cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                use_ngit_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                // amend: remove the tip and add a different commit in its place
                amend_last_commit(&test_repo, "add ammended-commit.md")?;
                test_repo.checkout("main")?;

                // checkout without --force should bail on diverged branch
                assert!(
                    run_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1).is_err(),
                    "expected checkout to fail without --force on amended branch"
                );

                shutdown_relays()?;
                Ok((originating_repo, test_repo))
            });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn local_unpublished_commits_are_not_overwritten() -> Result<()> {
        let (originating_repo, test_repo) = prep_and_run().await?;
        // the local branch tip must differ from the published tip — local work preserved
        assert_ne!(
            originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            test_repo.get_tip_of_local_branch(
                &get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?
            )?,
        );
        Ok(())
    }
}

mod when_proposal_branch_has_local_commits_on_top {
    use super::*;

    async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = make_relays();

        let cli_tester_handle =
            std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                let originating_repo = cli_tester_create_proposals()?;

                let test_repo = GitTestRepo::default();
                test_repo.populate()?;

                use_ngit_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                // add an extra local commit on top of the proposal branch
                std::fs::write(test_repo.dir.join("local-extra.md"), "local work")?;
                test_repo.stage_and_commit("add local-extra.md")?;
                test_repo.checkout("main")?;

                // checkout again — should not discard the extra local commit
                run_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1)?;

                shutdown_relays()?;
                Ok((originating_repo, test_repo))
            });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_checked_out() -> Result<()> {
        let (_, test_repo) = prep_and_run().await?;
        assert_eq!(
            get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?,
            test_repo.get_checked_out_branch_name()?,
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn local_commits_are_not_discarded() -> Result<()> {
        let (originating_repo, test_repo) = prep_and_run().await?;
        let branch = get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?;
        let local_tip = test_repo.get_tip_of_local_branch(&branch)?;
        let published_tip = originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?;
        // local tip must be ahead of (not equal to) the published tip
        assert_ne!(local_tip, published_tip, "local commits were discarded");
        // and the published tip must be an ancestor of the local tip
        assert!(
            test_repo
                .git_repo
                .graph_descendant_of(local_tip, published_tip)?,
            "local branch is not descended from published tip"
        );
        Ok(())
    }
}

mod when_newer_revision_rebases_proposal {
    use super::*;

    async fn prep_and_run() -> Result<(GitTestRepo, GitTestRepo)> {
        let (mut r51, mut r52, mut r53, mut r55, mut r56) = make_relays();

        let cli_tester_handle =
            std::thread::spawn(move || -> Result<(GitTestRepo, GitTestRepo)> {
                let (new_originating_repo, test_repo) =
                    create_proposals_with_rebased_first_proposal()?;

                // refresh test_repo cache so it sees the new rebased revision
                let mut p = CliTester::new_from_dir(
                    &test_repo.dir,
                    [
                        "--nsec",
                        TEST_KEY_1_NSEC,
                        "--password",
                        TEST_PASSWORD,
                        "--disable-cli-spinners",
                        "pr",
                        "list",
                    ],
                );
                p.expect_end_eventually()?;

                // checkout without --force should bail on diverged branch
                assert!(
                    run_pr_checkout(&test_repo, FEATURE_BRANCH_NAME_1).is_err(),
                    "expected checkout to fail without --force on diverged branch"
                );
                // checkout with --force should update to the new rebased revision
                // (relays still needed for the fetch inside checkout)
                run_pr_checkout_force(&test_repo, FEATURE_BRANCH_NAME_1)?;

                shutdown_relays()?;
                Ok((new_originating_repo, test_repo))
            });

        let _ = join!(
            r51.listen_until_close(),
            r52.listen_until_close(),
            r53.listen_until_close(),
            r55.listen_until_close(),
            r56.listen_until_close(),
        );
        cli_tester_handle.join().unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_checked_out() -> Result<()> {
        let (_, test_repo) = prep_and_run().await?;
        assert_eq!(
            get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?,
            test_repo.get_checked_out_branch_name()?,
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn proposal_branch_tip_is_most_recent_revision_tip() -> Result<()> {
        let (new_originating_repo, test_repo) = prep_and_run().await?;
        assert_eq!(
            new_originating_repo.get_tip_of_local_branch(FEATURE_BRANCH_NAME_1)?,
            test_repo.get_tip_of_local_branch(
                &get_proposal_branch_name(&test_repo, FEATURE_BRANCH_NAME_1)?
            )?,
        );
        Ok(())
    }
}

/// Creates 3 proposals, checks out proposal 1 in a test repo, then publishes
/// a rebased revision of proposal 1 from a second originating repo. Returns
/// (new_originating_repo, test_repo) with the test repo still on the old branch.
fn create_proposals_with_rebased_first_proposal(
) -> Result<(GitTestRepo, GitTestRepo)> {
    // create the initial 3 proposals and check out proposal 1 in a test repo
    let (_, test_repo) =
        create_proposals_and_repo_with_proposal_branch_checked_out(FEATURE_BRANCH_NAME_1)?;

    // get the original proposal id to use as in_reply_to for the rebased revision
    let original_proposal_id =
        get_proposal_id_for_branch(&test_repo.dir, FEATURE_BRANCH_NAME_1)?;

    // publish a rebased revision of proposal 1 from a second originating repo
    let second_originating_repo = GitTestRepo::default();
    second_originating_repo.populate()?;
    std::fs::write(
        second_originating_repo.dir.join("amazing.md"),
        "some content",
    )?;
    second_originating_repo.stage_and_commit("commit for rebasing on top of")?;
    cli_tester_create_proposal(
        &second_originating_repo,
        FEATURE_BRANCH_NAME_1,
        "a",
        Some((PROPOSAL_TITLE_1, "proposal a description")),
        Some(original_proposal_id),
    )?;

    // simulate the test repo having pulled the updated main branch
    let branch_name = test_repo.get_checked_out_branch_name()?;
    test_repo.checkout("main")?;
    std::fs::write(test_repo.dir.join("amazing.md"), "some content")?;
    test_repo.stage_and_commit("commit for rebasing on top of")?;
    test_repo.checkout(&branch_name)?;

    Ok((second_originating_repo, test_repo))
}
