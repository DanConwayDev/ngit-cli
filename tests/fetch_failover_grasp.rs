//! Lighthouse test: when the first git server in a repo's announcement is
//! unreachable, `git clone <nostr://...>` falls through to the next one.
//!
//! Companion to `tests/clone_grasp.rs` (single-server happy path) and
//! `tests/fetch_grasp.rs` (post-clone fetch). Together they cover the
//! single-server, multi-server-fallback, and incremental-fetch surfaces of
//! the read path.
//!
//! Flow:
//!
//! 1. Harness spins up one vanilla relay (`default`) and two grasp servers
//!    (`primary`, `secondary`).
//! 2. Publisher: `ngit account create`, commit, `ngit init --grasp-server
//!    <primary> --grasp-server <secondary>`, then `git push -u origin main`.
//!    `ngit push` to a nostr remote fans out to every git server listed in the
//!    repo's announcement (`src/lib/push.rs:472`), so both grasps wind up with
//!    the pack data and the announcement.
//! 3. Test calls `Harness::take_grasp("primary")` and drops the returned
//!    server. The subprocess is killed and its loopback port becomes unbindable
//!    until kernel cleanup — connection attempts get `ECONNREFUSED`.
//! 4. Cloner: fresh repo, `git clone <nostr-url>`. `git-remote-nostr` iterates
//!    `repo_ref.git_server` in announcement order (see
//!    `src/bin/git_remote_nostr/fetch.rs:43`); the primary fails, the secondary
//!    succeeds, the clone completes.
//! 5. Assert the clone's working tree contains the publisher's content and
//!    `refs/remotes/origin/main` matches the publisher's tip.
//!
//! ## Why we don't reach the announcement via the dead primary's relay
//!
//! The vanilla `default` relay also receives the kind-30617 fan-out
//! (`Params::default()` lists both grasp-derived relays *and* the user's
//! `relay_default_set`). It's the only relay surface guaranteed alive
//! after the kill, and ngit's relay discovery walks both the URL's
//! embedded relay and `NGIT_RELAY_DEFAULT_SET`. The dead primary's relay
//! drops out of the rotation the same way its git endpoint does.

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::Harness;

#[tokio::test]
async fn clone_falls_through_when_first_grasp_is_unreachable() -> Result<()> {
    let mut harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("primary")
    .with_grasp_server("secondary")
    .build()
    .await?;

    let publisher = harness.fresh_repo()?;
    let display_name = "lighthouse fetch failover";
    let identifier = "lighthouse-fetch-failover";

    // --- step 1: account create ---------------------------------------------
    let create_output = publisher
        .ngit(["account", "create", "--local", "--name", display_name])
        .output()
        .await
        .context("failed to spawn ngit account create")?;
    assert!(
        create_output.status.success(),
        "ngit account create exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        create_output.status,
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr),
    );

    let nsec = publisher
        .config("nostr.nsec")
        .await?
        .context("nostr.nsec missing from local git config after account create")?;
    let keys = Keys::parse(&nsec).context("nostr.nsec from local config is not a valid key")?;
    let pubkey = keys.public_key();

    // --- step 2: commit + init with TWO grasp servers -----------------------
    std::fs::write(publisher.dir().join("README.md"), "failover test\n")
        .context("failed to write README.md")?;
    run_git_ok(&publisher, ["add", "README.md"], "git add").await?;
    run_git_ok(
        &publisher,
        ["commit", "-m", "initial", "--no-gpg-sign"],
        "git commit",
    )
    .await?;

    let main_oid = publisher
        .snapshot()?
        .refs
        .get("refs/heads/main")
        .context("refs/heads/main missing after initial commit")?
        .clone();

    let primary_url = harness.grasp("primary").url().to_string();
    let secondary_url = harness.grasp("secondary").url().to_string();
    // `--grasp-server` is `clap`-declared as `num_args = 1..` (see
    // src/bin/ngit/sub_commands/init.rs:412), so multiple values go on one
    // flag instance. The declaration order here is also the order that
    // ends up in `repo_ref.git_server` and therefore the order
    // git-remote-nostr tries on fetch — primary first, secondary as
    // fallback.
    let init_output = publisher
        .ngit([
            "init",
            "--name",
            display_name,
            "--identifier",
            identifier,
            "--grasp-server",
            &primary_url,
            &secondary_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init")?;
    assert!(
        init_output.status.success(),
        "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
        init_output.status,
        String::from_utf8_lossy(&init_output.stdout),
        String::from_utf8_lossy(&init_output.stderr),
    );

    let init_stdout = String::from_utf8_lossy(&init_output.stdout);
    let printed_clone_url = extract_clone_url(&init_stdout).with_context(|| {
        format!("no `clone url:` line in ngit init stdout. full stdout was:\n{init_stdout}")
    })?;
    // The URL printed by ngit init embeds `coordinate.relays.first()` —
    // currently the **primary** grasp's relay (see
    // `src/lib/repo_ref.rs::coordinate_with_hint` + `Display for
    // NostrUrlDecoded`). After we kill the primary that relay disappears
    // along with the git endpoint, so the cloner would not even be able to
    // fetch the announcement.
    //
    // The legacy `when_first_git_server_fails_` scenario sidestepped this
    // by keeping relays and git servers as separate entities. The harness
    // collapses them into one process per grasp, so to recover the same
    // "dead git endpoint, live relay" property we drop the relay hint from
    // the URL and let the cloner consult `NGIT_RELAY_DEFAULT_SET` (the
    // vanilla relay, where init's fan-out also placed a copy of the
    // announcement). The announcement's own `relays` tag — listing both
    // grasp relays plus the user's relay-list — and its `clone` tags
    // (primary then secondary) are unaffected, so once it's read the
    // server-fallback logic exercises exactly the legacy path.
    let npub = pubkey
        .to_bech32()
        .context("failed to bech32-encode publisher pubkey")?;
    let clone_url = format!("nostr://{npub}/{identifier}");
    assert!(
        printed_clone_url.contains(&npub),
        "sanity: printed clone URL {printed_clone_url} should reference publisher npub",
    );

    // --- step 3: push, fanning out to both grasps ----------------------------
    run_git_ok(
        &publisher,
        ["push", "-u", "origin", "main"],
        "git push origin main",
    )
    .await?;

    // Sanity: both grasps received the announcement *and* the state event.
    // Without this, the failover assertion is meaningless — we'd really
    // be testing "single-server clone via secondary" rather than
    // "fallback past dead primary".
    for role in ["primary", "secondary"] {
        let grasp = harness.grasp(role);
        let announcements = grasp
            .events(Filter::new().author(pubkey).kind(Kind::GitRepoAnnouncement))
            .await?;
        assert_eq!(
            announcements.len(),
            1,
            "grasp {role:?} should have exactly one announcement after push; got {}",
            announcements.len(),
        );
        let state_events = grasp
            .events(Filter::new().author(pubkey).kind(Kind::Custom(30618)))
            .await?;
        assert!(
            !state_events.is_empty(),
            "grasp {role:?} should have at least one state event after push",
        );
    }

    // The vanilla `default` relay also carries the announcement — it's the
    // discovery surface that survives killing the primary grasp. If this
    // assertion ever fails, the test below will report "no announcement
    // event found" and the failure mode is "relay fan-out did not include
    // the user's default-set" rather than "fallback git logic broken".
    let default_anns = harness
        .relay("default")
        .events(Filter::new().author(pubkey).kind(Kind::GitRepoAnnouncement))
        .await?;
    assert_eq!(
        default_anns.len(),
        1,
        "vanilla default relay should carry the announcement; got {}",
        default_anns.len(),
    );

    // --- step 4: take + drop the primary grasp ------------------------------
    //
    // `take_grasp` returns the owned `GraspServer`; dropping it signals
    // the subprocess and waits for it to exit (`grasp.rs::Drop`). After
    // this, `127.0.0.1:<primary_port>` no longer accepts connections.
    let primary = harness
        .take_grasp("primary")
        .context("primary grasp was already taken or never registered")?;
    let dead_url = primary.url().to_string();
    drop(primary);

    // Sanity check that the primary really is down — otherwise a passing
    // test could just be hiding a no-op.
    let probe = tokio::net::TcpStream::connect(dead_url.trim_start_matches("http://")).await;
    assert!(
        probe.is_err(),
        "primary grasp should be down after drop, but TCP connect to {dead_url} succeeded",
    );

    // --- step 5: cloner clones — must succeed via secondary -----------------
    //
    // `fresh_repo` runs *after* the take_grasp call, so the cloner's env
    // (`NGIT_GRASP_DEFAULT_SET`) only references the secondary. That's
    // immaterial for clone resolution — git-remote-nostr reads the git
    // server list from the announcement, not from env vars — but it
    // keeps the harness state consistent for any future operation.
    let cloner = harness.fresh_repo()?;
    let clone_dir_name = "cloned";
    let clone_target = cloner.dir().join(clone_dir_name);

    run_git_ok(
        &cloner,
        ["clone", &clone_url, clone_dir_name],
        "git clone over nostr:// with dead primary",
    )
    .await?;

    assert!(
        clone_target.join(".git").is_dir(),
        "git clone reported success but .git is missing at {}",
        clone_target.display(),
    );

    let cloned_readme = clone_target.join("README.md");
    let cloned_contents = std::fs::read_to_string(&cloned_readme).with_context(|| {
        format!(
            "README.md missing from clone at {} — clone returned success but checkout produced no working tree",
            cloned_readme.display()
        )
    })?;
    assert_eq!(
        cloned_contents, "failover test\n",
        "cloned README.md content does not match publisher's",
    );

    let cloned_oid = {
        let repo = git2::Repository::open(&clone_target)
            .with_context(|| format!("open clone at {}", clone_target.display()))?;
        let reference = repo
            .find_reference("refs/remotes/origin/main")
            .context("find_reference refs/remotes/origin/main")?;
        reference
            .target()
            .context("refs/remotes/origin/main has no direct target")?
            .to_string()
    };
    assert_eq!(
        cloned_oid, main_oid,
        "clone's origin/main ({cloned_oid}) does not match publisher's ({main_oid})",
    );

    Ok(())
}

/// Run a `git` subprocess inside `repo`, asserting it exited successfully.
async fn run_git_ok<I, S>(repo: &test_harness::Repo, args: I, label: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = repo
        .git(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn {label}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{label} exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(())
}

/// Pull the first `nostr://...` URL printed after a `clone url:` /
/// `your clone URL:` label in `ngit init` stdout. Same shape as
/// `tests/clone_grasp.rs::extract_clone_url`.
fn extract_clone_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("clone url:") {
            let rest = line[idx + "clone url:".len()..].trim();
            if rest.starts_with("nostr://") {
                return Some(rest.to_string());
            }
        }
    }
    None
}
