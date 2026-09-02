//! `ngit init -d` and grasp-server defaulting.
//!
//! `-d` promises the user's preferred grasp servers, falling back to the
//! system defaults. Supplying `--additional-clone` (and/or
//! `--additional-relay`) used to suppress that promise silently: the
//! announcement was published with no grasp server and — because grasp
//! servers are what supply the repository's relays on the default path — no
//! relays at all, leaving the additional clone URL as the repository's only
//! infrastructure. See issue 3ef13932.
//!
//! Hosting a repository without a grasp server is legitimate, so it stays
//! available; it just has to be stated rather than inferred. The statement is
//! an empty value: `--grasp-server ""`.
//!
//! Both tests query the vanilla `default` relay for the announcement rather
//! than the grasp: ngit-grasp routes a new announcement to purgatory until
//! its git data arrives, whereas the default relay always materialises the
//! kind-30617 because publishing fans out to the user's relay list.

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use test_harness::{Harness, tag_value, tag_values};

const DISPLAY_NAME: &str = "grasp defaults";
const IDENTIFIER: &str = "grasp-defaults";

/// The announcement this publisher wrote for [`IDENTIFIER`], read back from
/// the harness's vanilla `default` relay.
async fn announcement(harness: &Harness, author: PublicKey) -> Result<Event> {
    harness
        .relay("default")
        .events(Filter::new().author(author).kind(Kind::GitRepoAnnouncement))
        .await?
        .into_iter()
        .find(|event| tag_value(event, "d").as_deref() == Some(IDENTIFIER))
        .with_context(|| {
            format!(
                "no kind-30617 with `d` = {IDENTIFIER:?} on the default relay after `ngit init`"
            )
        })
}

/// `ngit init -d --additional-clone <url>` supplements the default grasp
/// hosting rather than replacing it: the announcement carries both the
/// grasp-derived clone URL and the additional one, and the grasp-derived
/// relay keeps the announcement's relay list non-empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_clone_supplements_default_grasp_hosting() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    // The only grasp server the harness registers, so it is what
    // `NGIT_GRASP_DEFAULT_SET` offers ngit as the system default.
    .with_grasp_server("repo")
    .with_vanilla_git_server("host")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let grasp = harness.grasp("repo");
    let grasp_http_url = grasp.url().to_string();
    let grasp_relay_url = grasp.relay_url();
    let vanilla_url = harness.vanilla_git_server("host").url().to_string();

    let init = repo
        .ngit([
            "init",
            "--name",
            DISPLAY_NAME,
            "--identifier",
            IDENTIFIER,
            "--additional-clone",
            &vanilla_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init with an additional clone url")?;
    if !init.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init.status,
            String::from_utf8_lossy(&init.stdout),
            String::from_utf8_lossy(&init.stderr),
        );
    }

    let announcement = announcement(&harness, state.keys.public_key()).await?;

    let clone_urls = tag_values(&announcement, "clone");
    assert!(
        clone_urls.iter().any(|url| url == &vanilla_url),
        "the additional clone url {vanilla_url:?} should be announced \
         verbatim; got {clone_urls:?}",
    );
    let grasp_clone_prefix = format!("{grasp_http_url}/");
    assert!(
        clone_urls.iter().any(|url| {
            url.starts_with(&grasp_clone_prefix)
                && url.contains(&state.npub)
                && url.ends_with(&format!("/{IDENTIFIER}.git"))
        }),
        "`-d` should still host the repository on the default grasp server \
         ({grasp_http_url}) alongside the additional clone url; got \
         {clone_urls:?}",
    );

    let relays = tag_values(&announcement, "relays");
    assert!(
        relays.contains(&grasp_relay_url),
        "the default grasp server's relay ({grasp_relay_url}) should be \
         announced; got {relays:?}",
    );

    Ok(())
}

/// Opting out without supplying hosting of your own is refused rather than
/// quietly falling back to the defaults the opt-out just declined. `-d` does
/// not fill this gap: there is no default that could.
#[tokio::test]
async fn opting_out_without_own_hosting_is_refused() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;

    let init = repo
        .ngit([
            "init",
            "--name",
            DISPLAY_NAME,
            "--identifier",
            IDENTIFIER,
            "--grasp-server",
            "",
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init with an unusable opt-out")?;
    assert!(
        !init.status.success(),
        "opting out of grasp hosting without an additional relay and clone \
         url must fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(
        stderr.contains("--additional-relay") && stderr.contains("--additional-clone"),
        "the refusal should name the flags that supply the missing hosting: {stderr}",
    );

    let announcements = harness
        .relay("default")
        .events(
            Filter::new()
                .author(state.keys.public_key())
                .kind(Kind::GitRepoAnnouncement),
        )
        .await?;
    assert!(
        announcements.is_empty(),
        "the refused init must not publish an announcement; got {announcements:?}",
    );

    Ok(())
}

/// `--grasp-server ""` is the explicit opt-out: no grasp server is selected,
/// no grasp-derived clone URL or relay is synthesised, and the announcement
/// carries exactly the additional infrastructure that was supplied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_grasp_server_value_opts_out_of_grasp_hosting() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    // Registered so the system default is non-empty: the opt-out has to beat
    // an available default, not merely the absence of one.
    .with_grasp_server("repo")
    .with_vanilla_git_server("host")
    .build()
    .await?;

    let (repo, state) = harness.arrange_init_state_a_fresh().await?;
    let grasp = harness.grasp("repo");
    let grasp_http_url = grasp.url().to_string();
    let grasp_relay_url = grasp.relay_url();
    let vanilla_url = harness.vanilla_git_server("host").url().to_string();
    let default_relay_url = harness.relay("default").url().to_string();

    let init = repo
        .ngit([
            "init",
            "--name",
            DISPLAY_NAME,
            "--identifier",
            IDENTIFIER,
            "--grasp-server",
            "",
            "--additional-clone",
            &vanilla_url,
            "--additional-relay",
            &default_relay_url,
            "-d",
        ])
        .output()
        .await
        .context("failed to spawn ngit init with an empty --grasp-server value")?;
    if !init.status.success() {
        bail!(
            "ngit init exited non-zero ({:?})\nstdout: {}\nstderr: {}",
            init.status,
            String::from_utf8_lossy(&init.stdout),
            String::from_utf8_lossy(&init.stderr),
        );
    }

    let announcement = announcement(&harness, state.keys.public_key()).await?;

    let clone_urls = tag_values(&announcement, "clone");
    assert_eq!(
        clone_urls,
        vec![vanilla_url.clone()],
        "the opt-out should leave the additional clone url as the only git \
         server (no {grasp_http_url} entry)",
    );

    let relays = tag_values(&announcement, "relays");
    assert!(
        !relays.contains(&grasp_relay_url),
        "the opt-out should not announce the grasp server's relay \
         ({grasp_relay_url}); got {relays:?}",
    );
    assert!(
        relays
            .iter()
            .any(|relay| relay.trim_end_matches('/') == default_relay_url.trim_end_matches('/')),
        "the announcement should carry the supplied additional relay \
         ({default_relay_url}); got {relays:?}",
    );

    Ok(())
}
