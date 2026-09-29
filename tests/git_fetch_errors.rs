use anyhow::Result;
use ngit::{fetch::ensure_commit_local, git::Repo, repo_ref::RepoRef};
use nostr::prelude::*;
use test_harness::Harness;

#[tokio::test]
async fn unavailable_commit_retains_each_server_failure_and_handles_no_servers() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .build()
    .await?;
    let directory = harness.fresh_repo()?;
    let repo = Repo::from_path(&directory.dir().to_path_buf())?;
    let announcement = EventBuilder::new(Kind::GitRepoAnnouncement, "")
        .tags([Tag::identifier("fetch-errors")])
        .finalize(&Keys::generate())?;
    let mut reference = RepoRef::try_from((announcement, None))?;
    let missing = "1111111111111111111111111111111111111111";
    let term = console::Term::buffered_stderr();
    let error = ensure_commit_local(missing, &repo, &reference, &[], &term, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no Git servers were available"));
    // Rejected transports provide distinct, deterministic failures without
    // contacting external hosts or invoking a helper subprocess.
    reference.git_server = vec!["nostr://invalid-git-server".into(), "fd::12".into()];
    let error = ensure_commit_local(missing, &repo, &reference, &[], &term, None)
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("nostr://invalid-git-server"), "{message}");
    assert!(message.contains("fd::12"), "{message}");
    assert!(message.contains("recursively invoke"), "{message}");
    assert!(message.contains("file-descriptor transport"), "{message}");
    assert!(
        repo.git_repo
            .find_commit(git2::Oid::from_str(missing)?)
            .is_err()
    );
    assert!(repo.git_repo.is_empty()?);
    Ok(())
}
