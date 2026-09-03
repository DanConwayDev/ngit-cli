//! End-to-end coverage for NIP-5A nsite publication.
//!
//! Every scenario drives the real `ngit nsite publish` subprocess against the
//! shared deterministic Blossom fixture and a harness relay. Assertions target
//! observable side effects — the fixture's captured request log, the manifest
//! event on the relay, the process exit status, and the structured `--json`
//! document — never ngit's human-readable output.

use std::{fs, path::Path, process::Stdio};

use anyhow::{Context, Result, bail, ensure};
use bitcoin_hashes::sha256;
use ngit::nsite::NSITE_ROOT_KIND;
use nostr_sdk::prelude::{Event, Filter, PublicKey};
use serde_json::Value;
use test_harness::{
    BlossomGate, BlossomRequest, BlossomRule, BlossomServer, Harness, PublishRepoOpts,
    PublishedRepo, Repo, presence_requests, upload_requests,
};

const INDEX_HTML: &[u8] = b"<h1>ngit nsite</h1>\n";
const APP_JS: &[u8] = b"console.log('ngit nsite');\n";
const SHARED_HTML: &[u8] = b"<p>shared partial</p>\n";

/// A directory whose `/a/page.html` and `/b/page.html` hold identical bytes is
/// uploaded once for that hash, and both paths map to it in the manifest.
#[tokio::test]
async fn identical_paths_share_one_uploaded_blob() -> Result<()> {
    let (harness, publisher, published) = setup().await?;
    let site = publisher.dir().join("dist");
    write_site(
        &site,
        &[
            ("index.html", INDEX_HTML),
            ("assets/app.js", APP_JS),
            ("a/page.html", SHARED_HTML),
            ("b/page.html", SHARED_HTML),
        ],
    )?;
    let blossom = BlossomServer::start().await?;

    let output = run_json(
        &publisher,
        &[
            "nsite",
            "publish",
            "dist",
            "--blossom-server",
            blossom.base_url(),
            "--json",
        ],
    )
    .await?;
    let requests = blossom.finish().await?;

    ensure!(output["result"]["file_count"] == 4);
    ensure!(output["result"]["unique_blob_count"] == 3);
    ensure!(output["result"]["changed"] == true);
    ensure!(output["result"]["kind"] == u64::from(NSITE_ROOT_KIND.as_u16()));

    // One `PUT /upload` per unique hash, never per path.
    let uploads = upload_requests(&requests);
    ensure!(
        uploads.len() == 3,
        "expected one upload per unique hash, found {}",
        uploads.len()
    );
    let shared_hash = hex_hash(SHARED_HTML);
    ensure!(
        uploads
            .iter()
            .filter(|request| request.hash() == Some(shared_hash.as_str()))
            .count()
            == 1,
        "the shared blob was uploaded more than once"
    );
    assert_presence_precedes_every_upload(&requests)?;
    assert_uploads_are_nostr_authorized(&requests)?;

    let event = single_manifest(&harness, published.maintainer_keys.public_key()).await?;
    let paths = path_tags(&event);
    ensure!(
        paths
            == [
                ("/a/page.html".to_owned(), shared_hash.clone()),
                ("/assets/app.js".to_owned(), hex_hash(APP_JS)),
                ("/b/page.html".to_owned(), shared_hash),
                ("/index.html".to_owned(), hex_hash(INDEX_HTML)),
            ],
        "unexpected manifest path tags: {paths:?}"
    );
    Ok(())
}

/// One Blossom server fails every presence check while the other confirms
/// every blob: publication succeeds, and the per-server JSON records the
/// confirmed and the presence-only failed copies.
#[tokio::test]
async fn publication_succeeds_when_one_of_two_servers_never_answers() -> Result<()> {
    let (harness, publisher, published) = setup().await?;
    let site = publisher.dir().join("dist");
    write_site(
        &site,
        &[
            ("index.html", INDEX_HTML),
            ("assets/app.js", APP_JS),
            ("about.html", b"<h1>about</h1>\n"),
        ],
    )?;
    let healthy = BlossomServer::start().await?;
    let failing = BlossomServer::start().await?;
    failing.add_rule(BlossomRule::any().respond_status(
        500,
        "Internal Server Error",
        "presence check failed",
    ));

    let output = run_json(
        &publisher,
        &[
            "nsite",
            "publish",
            "dist",
            "--blossom-server",
            healthy.base_url(),
            "--blossom-server",
            failing.base_url(),
            "--json",
        ],
    )
    .await?;
    let healthy_root = healthy.base_url_with_slash();
    let failing_root = failing.base_url_with_slash();
    let healthy_requests = healthy.finish().await?;
    let failing_requests = failing.finish().await?;

    ensure!(output["result"]["unique_blob_count"] == 3);
    ensure!(upload_requests(&healthy_requests).len() == 3);
    // A server whose preflight never succeeded is never handed a signed upload
    // authorization.
    ensure!(
        upload_requests(&failing_requests).is_empty(),
        "a server which failed every presence check received an upload"
    );
    ensure!(!presence_requests(&failing_requests).is_empty());

    let blobs = output["result"]["blossom"]["blobs"]
        .as_array()
        .context("per-blob Blossom results were not an array")?;
    ensure!(blobs.len() == 3);
    for blob in blobs {
        let servers = blob["servers"]
            .as_array()
            .context("per-server Blossom results were not an array")?;
        ensure!(servers.len() == 2);
        ensure!(servers[0]["server"] == healthy_root.as_str());
        ensure!(servers[0]["status"] == "stored");
        ensure!(servers[0]["presence_check_only"].is_null());
        ensure!(servers[1]["server"] == failing_root.as_str());
        // The circuit breaker turns later queued checks into `not_attempted`
        // once three consecutive transient failures have opened it, so both
        // shapes are legitimate; neither is a confirmed copy.
        ensure!(
            servers[1]["status"] == "failed" || servers[1]["status"] == "not_attempted",
            "unexpected failing-server status: {}",
            servers[1]["status"]
        );
        ensure!(
            servers[1]["presence_check_only"] == true,
            "a failed preflight must be recorded as presence-only"
        );
    }
    ensure!(output["result"]["blossom"]["confirmed_operations"] == 3);
    // Presence-only diagnostics cannot prove a replica is missing, so they are
    // deliberately kept out of the replication warning.
    ensure!(
        !warning_codes(&output)?.contains(&"blossom_replication_incomplete".to_owned()),
        "a presence-only failure must not raise a replication warning"
    );

    let event = single_manifest(&harness, published.maintainer_keys.public_key()).await?;
    ensure!(path_tags(&event).len() == 3);
    // Both selected servers stay on the manifest: the failed one is a
    // diagnostic, not a de-selection.
    let tags = tag_slices(&event);
    ensure!(tags.contains(&vec!["server", healthy_root.as_str()]));
    ensure!(tags.contains(&vec!["server", failing_root.as_str()]));
    Ok(())
}

/// When no server confirms a blob, ngit signs no manifest, publishes nothing,
/// and reports the uncertain PUT as a possible orphan.
#[tokio::test]
async fn no_confirmed_copy_publishes_no_manifest_event() -> Result<()> {
    let (harness, publisher, published) = setup().await?;
    let site = publisher.dir().join("dist");
    write_site(&site, &[("index.html", INDEX_HTML)])?;
    let blossom = BlossomServer::start().await?;
    // The upload is read in full and then abandoned: the client cannot tell
    // whether the server stored it.
    blossom.add_rule(BlossomRule::upload().close_after_request());

    let failure = run_json_expecting_failure(
        &publisher,
        &[
            "nsite",
            "publish",
            "dist",
            "--blossom-server",
            blossom.base_url(),
            "--json",
        ],
    )
    .await?;
    let server_root = blossom.base_url_with_slash();
    let requests = blossom.finish().await?;

    ensure!(failure["format_version"] == 2);
    ensure!(failure["command_status"] == "error");
    ensure!(failure.get("ok").is_none());
    ensure!(failure["error"]["code"] == "blossom_upload_failed");
    let details = &failure["error"]["details"];
    ensure!(details["blobs"][0]["sha256"] == hex_hash(INDEX_HTML));
    ensure!(details["blobs"][0]["servers"][0]["server"] == server_root.as_str());
    ensure!(details["blobs"][0]["servers"][0]["status"] == "unknown");
    let orphans = details["possible_orphan_blobs"]
        .as_array()
        .context("possible orphan blobs were not an array")?;
    ensure!(
        orphans.len() == 1,
        "expected one possible orphan, found {orphans:?}"
    );
    ensure!(orphans[0]["server"] == server_root.as_str());
    ensure!(orphans[0]["sha256"] == hex_hash(INDEX_HTML));

    // Every abandoned PUT is followed by a verification HEAD before the next
    // attempt, and the attempt plan is bounded.
    ensure!(upload_requests(&requests).len() == 3);
    ensure!(presence_requests(&requests).len() == 4);
    assert_presence_precedes_every_upload(&requests)?;

    let events = manifest_events(&harness, published.maintainer_keys.public_key()).await?;
    ensure!(
        events.is_empty(),
        "an nsite manifest was published without a confirmed blob copy"
    );
    Ok(())
}

/// `--fallback` maps the named build-output page to `/404.html` without
/// uploading its bytes again, and the manifest carries the full NIP-5A tag
/// structure in its documented order.
#[tokio::test]
async fn fallback_maps_the_named_page_to_404_in_the_manifest() -> Result<()> {
    let (harness, publisher, published) = setup().await?;
    let site = publisher.dir().join("dist");
    write_site(
        &site,
        &[("index.html", INDEX_HTML), ("assets/app.js", APP_JS)],
    )?;
    let blossom = BlossomServer::start().await?;
    let relay = harness.relay("default").url().to_string();

    let output = run_json(
        &publisher,
        &[
            "nsite",
            "publish",
            "dist",
            "--fallback",
            "index.html",
            "--title",
            "Fallback site",
            "--description",
            "Serves the SPA entry point for unknown paths",
            "--source",
            "https://example.invalid/fallback-site",
            "--blossom-server",
            blossom.base_url(),
            "--relay",
            &relay,
            "--json",
        ],
    )
    .await?;
    let requests = blossom.finish().await?;

    ensure!(output["result"]["fallback"] == "index.html");
    ensure!(output["result"]["file_count"] == 3);
    ensure!(output["result"]["unique_blob_count"] == 2);
    // The fallback mapping reuses the entry point's immutable snapshot.
    ensure!(upload_requests(&requests).len() == 2);

    let event = single_manifest(&harness, published.maintainer_keys.public_key()).await?;
    let index_hash = hex_hash(INDEX_HTML);
    let app_hash = hex_hash(APP_JS);
    let aggregate = output["result"]["aggregate_sha256"]
        .as_str()
        .context("aggregate hash missing from the JSON result")?;
    let server = output["result"]["servers"][0]
        .as_str()
        .context("server missing from the JSON result")?;
    let relay_tag = output["result"]["relays"][0]
        .as_str()
        .context("relay missing from the JSON result")?;
    let expected = vec![
        vec!["path", "/404.html", index_hash.as_str()],
        vec!["path", "/assets/app.js", app_hash.as_str()],
        vec!["path", "/index.html", index_hash.as_str()],
        vec!["x", aggregate, "aggregate"],
        vec!["server", server],
        vec!["relay", relay_tag],
        vec!["title", "Fallback site"],
        vec![
            "description",
            "Serves the SPA entry point for unknown paths",
        ],
        vec!["source", "https://example.invalid/fallback-site"],
    ];
    ensure!(
        tag_slices(&event) == expected,
        "unexpected manifest tags: {:?}",
        tag_slices(&event)
    );
    ensure!(event.content.is_empty());
    ensure!(
        aggregate == published_aggregate(&event)?,
        "the published aggregate tag does not describe the published path tags"
    );
    Ok(())
}

/// Republishing an unchanged directory re-checks presence, uploads nothing,
/// and leaves the existing manifest event in place.
#[tokio::test]
async fn unchanged_rerun_checks_presence_and_keeps_the_manifest() -> Result<()> {
    let (harness, publisher, published) = setup().await?;
    let site = publisher.dir().join("dist");
    write_site(
        &site,
        &[("index.html", INDEX_HTML), ("assets/app.js", APP_JS)],
    )?;
    let blossom = BlossomServer::start().await?;
    let relay = harness.relay("default").url().to_string();
    let args = [
        "nsite",
        "publish",
        "dist",
        "--blossom-server",
        blossom.base_url(),
        "--relay",
        &relay,
        "--json",
    ];

    let first = run_json(&publisher, &args).await?;
    let after_first = blossom.request_count();
    let second = run_json(&publisher, &args).await?;
    let requests = blossom.finish().await?;

    ensure!(first["result"]["changed"] == true);
    ensure!(first["result"]["previous_event_id"].is_null());
    ensure!(
        first["result"]["publication"]["relays"]
            .as_array()
            .context("first publication had no relay results")?
            .iter()
            .any(|result| result["status"] == "accepted")
    );

    // The second run is a no-op: presence confirms both blobs, so no
    // authorization is signed and no replacement event is published.
    let rerun = requests
        .get(after_first..)
        .context("the Blossom fixture lost its first-run requests")?;
    ensure!(
        upload_requests(rerun).is_empty(),
        "an unchanged rerun uploaded a blob"
    );
    ensure!(
        presence_requests(rerun).len() == 2,
        "expected one presence check per blob on the rerun, found {}",
        presence_requests(rerun).len()
    );
    ensure!(second["result"]["changed"] == false);
    ensure!(second["result"]["previous_event_id"] == first["result"]["event_id"]);
    ensure!(second["result"]["event_id"] == first["result"]["event_id"]);
    ensure!(second["result"]["publication"]["status"] == "not_attempted");
    ensure!(second["result"]["publication"]["reason"] == "manifest_unchanged");
    ensure!(second["result"]["blossom"]["already_present"] == 2);
    ensure!(second["result"]["blossom"]["stored"] == 0);

    let event = single_manifest(&harness, published.maintainer_keys.public_key()).await?;
    ensure!(event.id.to_hex() == first["result"]["event_id"]);
    Ok(())
}

/// An upload the server accepts but does not immediately serve is not counted
/// until a verification HEAD proves it: ngit retries the placement, and the
/// success it reports is backed by a `200` presence response.
#[tokio::test]
async fn an_accepted_upload_is_verified_before_it_counts_as_stored() -> Result<()> {
    let (harness, publisher, published) = setup().await?;
    let site = publisher.dir().join("dist");
    write_site(&site, &[("index.html", INDEX_HTML)])?;
    let blossom = BlossomServer::start().await?;
    let visible = BlossomGate::closed();
    blossom.hide_uploads_until(&visible);

    let child = publisher
        .ngit([
            "nsite",
            "publish",
            "dist",
            "--blossom-server",
            blossom.base_url(),
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn ngit nsite publish")?;
    // Preflight HEAD, the accepted PUT, and the verification HEAD which cannot
    // yet see the blob. Releasing visibility here — rather than after a
    // wall-clock delay — is what makes the retry deterministic.
    blossom.wait_for_requests(3).await?;
    visible.open();
    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for ngit nsite publish")?;
    ensure!(
        output.status.success(),
        "publication of an initially invisible blob failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)
        .context("ngit nsite publish did not emit one JSON document")?;
    let requests = blossom.finish().await?;

    let hash = hex_hash(INDEX_HTML);
    ensure!(
        requests.len() == 5,
        "expected preflight, two upload attempts, and two verifications, found {}",
        requests.len()
    );
    let uploads = upload_requests(&requests);
    ensure!(uploads.len() == 2);
    for upload in &uploads {
        ensure!(upload.hash() == Some(hash.as_str()));
        ensure!(
            requests.iter().any(|request| request.is_presence_check()
                && request.index > upload.index
                && request.hash() == Some(hash.as_str())),
            "an upload was counted without a following verification check"
        );
    }
    ensure!(requests[4].is_presence_check());
    ensure!(result["result"]["blossom"]["blobs"][0]["servers"][0]["status"] == "stored");
    ensure!(result["result"]["changed"] == true);

    single_manifest(&harness, published.maintainer_keys.public_key()).await?;
    Ok(())
}

async fn setup() -> Result<(Harness, Repo, PublishedRepo)> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;
    let default_relay = harness.relay("default").url().to_string();
    let (publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            identifier: Some("nsite-api-repository".to_owned()),
            extra_repo_relays: vec![default_relay],
            ..Default::default()
        })
        .await?;
    Ok((harness, publisher, published))
}

fn write_site(root: &Path, files: &[(&str, &[u8])]) -> Result<()> {
    for (path, bytes) in files {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create site directory {}", parent.display()))?;
        }
        fs::write(&target, bytes)
            .with_context(|| format!("failed to write site file {}", target.display()))?;
    }
    Ok(())
}

fn hex_hash(bytes: &[u8]) -> String {
    sha256::Hash::hash(bytes).to_string()
}

async fn run_json(repo: &Repo, args: &[&str]) -> Result<Value> {
    let output = repo
        .ngit(args.iter().copied())
        .output()
        .await
        .with_context(|| format!("failed to spawn ngit {args:?}"))?;
    if !output.status.success() {
        bail!(
            "ngit {args:?} exited {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    parse_json(&output.stdout, args)
}

async fn run_json_expecting_failure(repo: &Repo, args: &[&str]) -> Result<Value> {
    let output = repo
        .ngit(args.iter().copied())
        .output()
        .await
        .with_context(|| format!("failed to spawn ngit {args:?}"))?;
    if output.status.success() {
        bail!(
            "ngit {args:?} unexpectedly succeeded\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    parse_json(&output.stdout, args)
}

fn parse_json(stdout: &[u8], args: &[&str]) -> Result<Value> {
    serde_json::from_slice(stdout).with_context(|| {
        format!(
            "ngit {args:?} did not emit one JSON document: {}",
            String::from_utf8_lossy(stdout)
        )
    })
}

fn warning_codes(output: &Value) -> Result<Vec<String>> {
    Ok(output["warnings"]
        .as_array()
        .context("warnings were not an array")?
        .iter()
        .filter_map(|warning| warning["code"].as_str().map(str::to_owned))
        .collect())
}

async fn manifest_events(harness: &Harness, author: PublicKey) -> Result<Vec<Event>> {
    harness
        .relay("default")
        .events(Filter::new().kind(NSITE_ROOT_KIND).author(author))
        .await
}

async fn single_manifest(harness: &Harness, author: PublicKey) -> Result<Event> {
    let events = manifest_events(harness, author).await?;
    let [event] = events.as_slice() else {
        bail!("expected one nsite manifest event, found {}", events.len());
    };
    event.verify()?;
    Ok(event.clone())
}

fn tag_slices(event: &Event) -> Vec<Vec<&str>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().iter().map(String::as_str).collect())
        .collect()
}

/// `("/path", "<sha256>")` for every `path` tag, in event order.
fn path_tags(event: &Event) -> Vec<(String, String)> {
    event
        .tags
        .iter()
        .filter_map(|tag| match tag.as_slice() {
            [name, path, sha256, ..] if name == "path" => Some((path.clone(), sha256.clone())),
            _ => None,
        })
        .collect()
}

/// Recompute the NIP-5A aggregate from the event's own path tags.
fn published_aggregate(event: &Event) -> Result<String> {
    let mut lines = path_tags(event)
        .into_iter()
        .map(|(path, sha256)| format!("{sha256} {path}\n"))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    ensure!(!lines.is_empty(), "manifest event carried no path tags");
    Ok(aggregate_from_lines(&lines))
}

fn aggregate_from_lines(lines: &[String]) -> String {
    use bitcoin_hashes::HashEngine as _;

    let mut engine = sha256::Hash::engine();
    for line in lines {
        engine.input(line.as_bytes());
    }
    sha256::Hash::from_engine(engine).to_string()
}

/// Every uploaded hash was checked for presence before its bytes were sent.
fn assert_presence_precedes_every_upload(requests: &[BlossomRequest]) -> Result<()> {
    for upload in upload_requests(requests) {
        let hash = upload.hash().context("an upload omitted X-SHA-256")?;
        ensure!(
            requests.iter().any(|request| request.is_presence_check()
                && request.index < upload.index
                && request.hash() == Some(hash)),
            "blob {hash} was uploaded without a preceding presence check"
        );
    }
    Ok(())
}

fn assert_uploads_are_nostr_authorized(requests: &[BlossomRequest]) -> Result<()> {
    for upload in upload_requests(requests) {
        ensure!(
            upload
                .header("authorization")
                .is_some_and(|value| value.starts_with("Nostr ")),
            "an upload omitted its Nostr authorization"
        );
    }
    Ok(())
}
