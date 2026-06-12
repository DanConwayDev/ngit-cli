//! Event-shape capture harness (manual review aid, not an assertion test).
//!
//! Context: the nostr 0.44 -> 0.45 upgrade refactored every event-building
//! call site (chiefly `src/lib/git_events.rs`). Functional regressions are
//! covered by the behavioural e2e tests; what they do *not* check is whether
//! the *shape* of the events we emit (kinds, tag structure, content) still
//! matches the NIP / GRASP specs. No version of rust-nostr validates spec
//! compliance for us.
//!
//! This `#[ignore]`d test exercises the breadth of event kinds ngit emits,
//! pulls every event back off the relay + grasp surfaces over the wire, and
//! writes them pretty-printed and grouped by kind to a single file for a
//! one-time human eyeball against the specs (optionally piped through an
//! independent implementation such as `nak`).
//!
//! It is deliberately *not* part of the normal suite — it asserts nothing
//! about shape (that's the human's job) and only fails if a scenario it drives
//! errors outright. Run it explicitly:
//!
//! ```sh
//! cargo test --test capture_event_shapes -- --ignored --nocapture
//! # output written to ./event-shapes.md (override with NGIT_EVENT_SHAPE_OUT)
//! ```
//!
//! The supplementary maintainer-driven steps (comment / label / status /
//! issue) are best-effort: a failure there is logged and capture continues so
//! the file still contains everything that *did* land, rather than aborting
//! the whole capture on one flaky sub-command.

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use test_harness::{
    CloneLogin, Harness, PublishPatchSeriesOpts, PublishPrOpts, PublishRepoOpts, Repo,
};

const IDENTIFIER: &str = "event-shape-capture";

/// Map the kinds ngit emits to a human label for the review file headers.
/// Unknown kinds fall through to `kind <n>` so nothing is silently dropped.
fn kind_label(kind: Kind) -> String {
    let n = kind.as_u16();
    let name = match n {
        0 => "metadata (NIP-01)",
        1111 => "comment (NIP-22)",
        1617 => "git patch (NIP-34)",
        1618 => "pull request (ngit)",
        1619 => "pull request update (ngit)",
        1621 => "git issue (NIP-34)",
        1624 => "cover note (ngit)",
        1630 => "git status: open (NIP-34)",
        1631 => "git status: applied/merged (NIP-34)",
        1632 => "git status: closed (NIP-34)",
        1633 => "git status: draft (NIP-34)",
        1985 => "label (NIP-32)",
        10002 => "relay list metadata (NIP-65)",
        10317 => "user grasp server list (GRASP)",
        30617 => "git repository announcement (NIP-34)",
        30618 => "git repository state (NIP-34)",
        _ => "unrecognised",
    };
    format!("kind {n} — {name}")
}

/// Best-effort step: run `f`, log a warning on error, never propagate.
async fn best_effort<F, Fut>(label: &str, f: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if let Err(e) = f().await {
        eprintln!("[capture] supplementary step {label:?} skipped: {e:#}");
    }
}

/// Run an ngit subcommand from `repo`, returning Err with stderr on non-zero.
async fn ngit_ok(repo: &Repo, args: &[&str]) -> Result<String> {
    let out = repo
        .ngit(args.iter().copied())
        .output()
        .await
        .with_context(|| format!("failed to spawn ngit {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "ngit {args:?} exited {:?}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[tokio::test]
#[ignore = "manual review aid: dumps emitted event shapes to a file, asserts nothing about them"]
async fn capture_event_shapes() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .with_grasp_server("repo")
    .build()
    .await?;

    // --- 1. Maintainer publishes the repo -----------------------------------
    // Produces, across the relay + grasp surfaces: kind 0 + 10002 (account
    // create), kind 30617 (announcement), kind 30618 (state, via the push).
    let (_publisher, published) = harness
        .publish_repo(PublishRepoOpts {
            display_name: Some("event-shape maintainer".into()),
            identifier: Some(IDENTIFIER.into()),
            ..Default::default()
        })
        .await?;

    // --- 2. Contributor opens a PR-kind proposal (kind 1618) ----------------
    let pr = harness
        .publish_pr(
            &published,
            PublishPrOpts {
                branch: Some("feature-pr".into()),
                commits: vec![("pr-file.md".into(), "pr content\n".into())],
                title: "a pull request".into(),
                description: "pull request description".into(),
                in_reply_to: vec![],
            },
        )
        .await?;

    // --- 3. Contributor opens a patch series w/ cover letter (kind 1617) ----
    harness
        .publish_patch_series(
            &published,
            PublishPatchSeriesOpts {
                branch: Some("feature-patch".into()),
                commits: vec![
                    ("patch-a.md".into(), "patch a\n".into()),
                    ("patch-b.md".into(), "patch b\n".into()),
                ],
                cover_letter: Some(("a patch series".into(), "patch series cover letter".into())),
                in_reply_to: vec![],
            },
        )
        .await?;

    // --- 4. Maintainer-driven supplementary kinds (best-effort) -------------
    // A maintainer clone drives the status / label / comment / issue paths so
    // the capture covers kinds 1630-1633, 1985, 1111, 1621. Each is wrapped so
    // one failing sub-command doesn't lose the rest of the dump.
    let maintainer = harness
        .clone_published_repo(&published, CloneLogin::AsMaintainer)
        .await?;
    let pr_id = pr.event_id.to_hex();

    best_effort("pr comment", || async {
        ngit_ok(
            &maintainer,
            &[
                "pr",
                "comment",
                &pr_id,
                "--body",
                "a review comment on the PR",
            ],
        )
        .await
        .map(|_| ())
    })
    .await;

    best_effort("pr label", || async {
        ngit_ok(
            &maintainer,
            &[
                "pr",
                "label",
                &pr_id,
                "--label",
                "bug",
                "--label",
                "help-wanted",
            ],
        )
        .await
        .map(|_| ())
    })
    .await;

    best_effort("pr close (status)", || async {
        ngit_ok(
            &maintainer,
            &["pr", "close", &pr_id, "--reason", "superseded"],
        )
        .await
        .map(|_| ())
    })
    .await;

    best_effort("issue create", || async {
        ngit_ok(
            &maintainer,
            &[
                "issue",
                "create",
                "--title",
                "an example issue",
                "--body",
                "issue body text",
                "--label",
                "bug",
            ],
        )
        .await
        .map(|_| ())
    })
    .await;

    // --- 5. Drain every surface and collect unique events -------------------
    let mut by_id: BTreeMap<EventId, Event> = BTreeMap::new();
    for ev in harness.relay("default").events(Filter::new()).await? {
        by_id.insert(ev.id, ev);
    }
    for ev in harness.grasp("repo").events(Filter::new()).await? {
        by_id.insert(ev.id, ev);
    }

    // Group by kind, then sort within a kind by created_at then id for a
    // stable, diff-friendly file.
    let mut by_kind: BTreeMap<u16, Vec<Event>> = BTreeMap::new();
    for ev in by_id.into_values() {
        by_kind.entry(ev.kind.as_u16()).or_default().push(ev);
    }
    for events in by_kind.values_mut() {
        events.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    // --- 6. Render the review file ------------------------------------------
    let total: usize = by_kind.values().map(Vec::len).sum();
    let mut out = String::new();
    writeln!(out, "# ngit emitted-event shapes")?;
    writeln!(out)?;
    writeln!(
        out,
        "Captured by `tests/capture_event_shapes.rs` for manual NIP/GRASP \
         spec review after the nostr 0.45 upgrade."
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "{total} unique events across {} kind(s). Dynamic fields (`id`, \
         `pubkey`, `created_at`, `sig`, commit/tree oids, ephemeral keys, \
         relay ports) vary per run — review **tag structure, kinds and \
         content**, not the values.",
        by_kind.len()
    )?;
    writeln!(out)?;
    for (kind, events) in &by_kind {
        writeln!(
            out,
            "## {} ({} event(s))",
            kind_label(Kind::from(*kind)),
            events.len()
        )?;
        writeln!(out)?;
        for ev in events {
            let json = serde_json::to_value(ev).context("serialize event")?;
            let pretty = serde_json::to_string_pretty(&json).context("pretty-print event")?;
            writeln!(out, "```json")?;
            writeln!(out, "{pretty}")?;
            writeln!(out, "```")?;
            writeln!(out)?;
        }
    }

    let path = std::env::var_os("NGIT_EVENT_SHAPE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("event-shapes.md"));
    std::fs::write(&path, &out).with_context(|| format!("writing {}", path.display()))?;

    // Compact one-event-per-line NDJSON sidecar, ideal for piping through an
    // independent implementation for a signature/parse cross-check, e.g.:
    //   cat event-shapes.ndjson | while read -r e; do echo "$e" | nak verify; done
    let mut ndjson = String::new();
    for events in by_kind.values() {
        for ev in events {
            writeln!(
                ndjson,
                "{}",
                serde_json::to_string(ev).context("compact event")?
            )?;
        }
    }
    let ndjson_path = path.with_extension("ndjson");
    std::fs::write(&ndjson_path, &ndjson)
        .with_context(|| format!("writing {}", ndjson_path.display()))?;

    eprintln!(
        "[capture] wrote {total} events across {} kinds to {} (+ {})",
        by_kind.len(),
        path.display(),
        ndjson_path.display()
    );

    Ok(())
}
