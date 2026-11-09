//! Read signed CI service and execution events without turning discovery into
//! trust.
use anyhow::{Context, Result, bail};
use ngit::{
    client::{Client, Connect, Params, fetch_filters_to_local_cache, get_events_from_local_cache},
    git::{Repo, RepoActions},
    repo_lookup::RepositoryLookup,
};
use nostr::prelude::{EventId, Filter, Kind, PublicKey, SingleLetterTag, Timestamp};

use crate::cli::{CiInspectArgs, SignerParams};

pub const CI_KINDS: &[u16] = &[
    9840, 9841, 9842, 9843, 9844, 9845, 9846, 9847, 9848, 9849, 19843, 19844, 29846, 39842, 39844,
    39845, 39846,
];

/// Inspect exact signed envelopes; no service/provider policy is inferred.
pub async fn launch(args: &CiInspectArgs, auth: SignerParams<'_>) -> Result<()> {
    if args.id.is_none() && args.event.is_none() && args.kinds.is_empty() && args.author.is_none() {
        anyhow::ensure!(
            !args.raw && !args.include_expired,
            "--raw and --include-expired require an exact event or diagnostic filter"
        );
        return super::ci_status::launch(
            args.target.as_deref(),
            false,
            args.offline,
            None,
            super::ci_status::View {
                detailed: true,
                log_tail: selected_log_mode(args.logs, args.no_logs, crate::output::is_json()),
            },
            crate::output::is_json(),
            auth,
        )
        .await;
    }
    launch_events(args, auth).await
}

fn selected_log_mode(all: bool, none: bool, json: bool) -> crate::cli::LogTailMode {
    use crate::cli::LogTailMode;
    if all {
        LogTailMode::All
    } else if none || json {
        LogTailMode::None
    } else {
        LogTailMode::Auto
    }
}

async fn launch_events(args: &CiInspectArgs, auth: SignerParams<'_>) -> Result<()> {
    if args.kinds.iter().any(|k| !CI_KINDS.contains(k)) {
        bail!("--kind must name a NIP-C1 CI event kind");
    }
    let author = args
        .author
        .as_deref()
        .map(PublicKey::parse)
        .transpose()
        .context("invalid CI event author")?;
    let id = args
        .id
        .as_deref()
        .or(args.event.as_deref())
        .map(EventId::parse)
        .transpose()
        .context("invalid CI event id")?;
    let repo = Repo::discover().context("failed to find repository")?;
    let path = repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&repo)));
    let selected =
        crate::repo_ref::get_resolved_repo_coordinate_when_remote_unknown(&repo, &mut client)
            .await?;
    let mut lookup = RepositoryLookup::from_resolved(&repo, &selected)?;
    let mut coordinates = selected.coordinate;
    if !args.offline {
        super::repository_fetch::fetching_with_account(
            &repo,
            path,
            &mut client,
            &mut coordinates,
            &mut lookup,
            auth,
        )
        .await?;
    }
    let reference = lookup.load(path).await?;
    let filters = event_filters(args, author, id, &reference)?;
    let mut failures = Vec::new();
    if !args.offline {
        for (relay, result) in
            fetch_filters_to_local_cache(&client, path, &reference.relays, &filters).await?
        {
            if let Err(error) = result {
                failures.push(format!("{relay}: {error:#}"));
            }
        }
    }
    let events = get_events_from_local_cache(path, filters).await?;
    if id.is_some() && events.is_empty() {
        bail!("CI event not found in the selected relays or local cache");
    }
    let now = Timestamp::now();
    let mut output = Vec::new();
    for event in events {
        if event.verify().is_err() {
            continue;
        }
        let live = event_live(&event, now);
        if !live && !args.include_expired && id.is_none() {
            continue;
        }
        if !crate::output::is_json() {
            println!(
                "kind {} {} by {}{}",
                event.kind,
                event.id,
                event.pubkey,
                if live {
                    ""
                } else {
                    " (expired or invalid expiry)"
                }
            );
        }
        let mut entry = inspect_event(
            args,
            &client,
            path,
            &reference.relays,
            &event,
            live,
            &mut failures,
        )
        .await?;
        if id.is_some() {
            entry["status_context"] =
                inspection_status(&repo, path, &reference, &client, &event, args).await?;
        }
        output.push(entry);
    }
    crate::output::set_value(
        serde_json::json!({"command_status": "ok", "entity": "ci", "action": "inspect", "events": output, "relay_failures": failures, "offline": args.offline, "limit_per_kind": args.limit}),
    );
    Ok(())
}

fn event_filters(
    args: &CiInspectArgs,
    author: Option<PublicKey>,
    id: Option<EventId>,
    reference: &ngit::repo_ref::RepoRef,
) -> Result<Vec<Filter>> {
    let kinds = if args.kinds.is_empty() {
        if id.is_some() {
            CI_KINDS.to_vec()
        } else {
            vec![19843, 19844, 39844, 39845]
        }
    } else {
        args.kinds.clone()
    };
    let mut filters = Vec::new();
    for kind in kinds {
        let mut filter = Filter::new()
            .kind(Kind::Custom(kind))
            .limit(args.limit as usize);
        if let Some(author) = author {
            filter = filter.author(author);
        }
        if let Some(id) = id {
            filter = filter.id(id);
        } else if !matches!(kind, 19843 | 19844 | 39845 | 9847 | 9849) {
            filter = filter.custom_tags(
                SingleLetterTag::LOWERCASE_A,
                reference
                    .coordinates()
                    .iter()
                    .map(|c| c.coordinate.to_string())
                    .collect::<Vec<_>>(),
            );
        } else if matches!(kind, 9847 | 9849) && author.is_none() {
            bail!(
                "allocation cancellation inspection requires --author or --event to bound its scope"
            );
        }
        filters.push(filter);
    }
    Ok(filters)
}

/// Follow exact quoted evidence one hop, preserving the referenced event ID.
/// Missing/expired evidence stays missing, never replaced by another attempt.
async fn related_events(
    client: &Client,
    path: &std::path::Path,
    relays: &[nostr::prelude::RelayUrl],
    event: &nostr::prelude::Event,
    offline: bool,
    failures: &mut Vec<String>,
) -> Result<Vec<nostr::prelude::Event>> {
    let ids: Vec<_> = event
        .tags
        .iter()
        .filter_map(|t| {
            let t = t.as_slice();
            matches!(t[0].as_str(), "q" | "U")
                .then(|| t.get(1).and_then(|id| EventId::from_hex(id).ok()))
                .flatten()
        })
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let filters = vec![Filter::new().ids(ids)];
    if !offline {
        for (relay, result) in fetch_filters_to_local_cache(client, path, relays, &filters).await? {
            if let Err(error) = result {
                failures.push(format!("{relay}: {error:#}"));
            }
        }
    }
    let events = get_events_from_local_cache(path, filters).await?;
    Ok(events.into_iter().filter(|e| e.verify().is_ok()).collect())
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactManifest {
    version: u8,
    files: Vec<[String; 3]>,
}

/// Fetch only manifests, never their archives or file payloads. Reuse the
/// bounded streaming downloader and verify content against the Blossom digest.
async fn expand_artifacts(event: &nostr::prelude::Event) -> Result<Vec<serde_json::Value>> {
    use ngit::release_download::{UrlAssetRequest, download_url_asset_to_path};
    let mut output = Vec::new();
    let temp = tempfile::tempdir()?;
    for (index, tag) in event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "artifacts")
        .enumerate()
    {
        let tag = tag.as_slice();
        let raw = tag.get(1).context("missing artifact manifest URL")?;
        let url = nostr::prelude::Url::parse(raw)?;
        let digest = url
            .path_segments()
            .and_then(Iterator::last)
            .and_then(|s| s.split('.').next())
            .context("missing manifest digest")?;
        anyhow::ensure!(
            digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid manifest digest"
        );
        let path = temp.path().join(index.to_string());
        let mut request = UrlAssetRequest::new(raw);
        request.limits.max_bytes = 16 * 1024 * 1024;
        request.limits.total_timeout = std::time::Duration::from_secs(60);
        let downloaded = download_url_asset_to_path(request, &path).await?;
        anyhow::ensure!(
            downloaded.sha256 == digest.to_lowercase(),
            "artifact manifest digest mismatch"
        );
        let manifest: ArtifactManifest = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        anyhow::ensure!(
            manifest.version == 1 && !manifest.files.is_empty(),
            "unsupported or empty artifact manifest"
        );
        if let Some(count) = tag.get(3).filter(|s| !s.is_empty()) {
            anyhow::ensure!(
                count.parse::<usize>()? == manifest.files.len(),
                "artifact manifest count mismatch"
            );
        }
        if !crate::output::is_json() {
            for [url, filename, name] in &manifest.files {
                println!("  artifact {name}/{filename}: {url}");
            }
        }
        output.push(
            serde_json::json!({"url": raw, "sha256": downloaded.sha256, "manifest": manifest}),
        );
    }
    Ok(output)
}

async fn inspect_event(
    args: &CiInspectArgs,
    client: &Client,
    path: &std::path::Path,
    relays: &[nostr::prelude::RelayUrl],
    event: &nostr::prelude::Event,
    live: bool,
    failures: &mut Vec<String>,
) -> Result<serde_json::Value> {
    let decoded = crate::ci_event_view::decoded(event);
    let execution = matches!(
        ngit::ci::kinds::validate(event),
        Ok(ngit::ci::kinds::CiEvent::WorkflowResult(_)
            | ngit::ci::kinds::CiEvent::WorkflowProgress(_)
            | ngit::ci::kinds::CiEvent::JobResult(_)
            | ngit::ci::kinds::CiEvent::JobProgress(_))
    );
    let show_envelope = !crate::output::is_json()
        && (!execution || args.raw || args.id.is_none() && args.event.is_none());
    if show_envelope {
        for tag in decoded["tags"].as_array().into_iter().flatten() {
            println!(
                "  {}: {}",
                tag["name"].as_str().unwrap_or("tag"),
                tag["values"]
            );
        }
        if !event.content.is_empty() && (!execution || args.logs) {
            println!("{}", event.content);
        }
    }
    let artifacts = if args.artifacts {
        expand_artifacts(event).await?
    } else {
        Vec::new()
    };
    let related = if args.id.is_some() || args.event.is_some() {
        related_events(client, path, relays, event, args.offline, failures).await?
    } else {
        Vec::new()
    };
    if show_envelope {
        for related in &related {
            println!(
                "  related {} {}",
                crate::ci_event_view::event_type(related.kind.as_u16()),
                related.id
            );
            for tag in crate::ci_event_view::decoded(related)["tags"]
                .as_array()
                .into_iter()
                .flatten()
            {
                println!(
                    "    {}: {}",
                    tag["name"].as_str().unwrap_or("tag"),
                    tag["values"]
                );
            }
            if !related.content.is_empty() && args.logs {
                println!("{}", related.content);
            }
        }
    }
    Ok(serde_json::json!({
        "event": if args.raw { serde_json::to_value(event)? } else { decoded },
        "live": live, "signature_verified": true, "related": related.iter().map(|event| {
            if args.raw { serde_json::to_value(event) } else { Ok(crate::ci_event_view::decoded(event)) }
        }).collect::<std::result::Result<Vec<_>, _>>()?, "artifact_manifests": artifacts
    }))
}

fn event_live(event: &nostr::prelude::Event, now: Timestamp) -> bool {
    let expires: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] == "expiration")
        .collect();
    let expiry = expires
        .first()
        .and_then(|t| t.as_slice().get(1))
        .and_then(|v| v.parse::<u64>().ok());
    let lifetime = match event.kind.as_u16() {
        19843 | 39845 | 39842 | 39846 | 9845 => Some(1800),
        19844 | 39844 => Some(86400),
        _ => None,
    };
    if let Some(max) = lifetime {
        expires.len() == 1
            && event.created_at <= now
            && expiry.is_some_and(|t| {
                t > now.as_secs() && t.saturating_sub(event.created_at.as_secs()) <= max
            })
    } else {
        expires.is_empty() || expires.len() == 1 && expiry.is_some_and(|t| t > now.as_secs())
    }
}

async fn inspection_status(
    repo: &Repo,
    path: &std::path::Path,
    reference: &ngit::repo_ref::RepoRef,
    client: &Client,
    event: &nostr::prelude::Event,
    args: &CiInspectArgs,
) -> Result<serde_json::Value> {
    use ngit::ci::kinds::CiEvent;

    use crate::ci_projection::{ProjectionRequest, Target, Tier, build_event_report};
    let common = match ngit::ci::kinds::validate(event) {
        Ok(CiEvent::WorkflowResult(r)) => r.common,
        Ok(CiEvent::WorkflowProgress(r)) => r.common,
        Ok(CiEvent::JobResult(r)) => r.common,
        Ok(CiEvent::JobProgress(r)) => r.common,
        _ => return Ok(serde_json::Value::Null),
    };
    let target = Target::Commit {
        ids: common.commits,
        described: "inspected execution".into(),
    };
    let report = build_event_report(
        repo,
        path,
        reference,
        client,
        &ProjectionRequest {
            target: &target,
            tier: if args.offline {
                Tier::Cache
            } else {
                Tier::Full
            },
            include_outdated: false,
            input_coverage: ngit::ci::trust::Coverage::Partial,
        },
        Some(event.id),
    )
    .await?;
    if !crate::output::is_json() {
        report.print(&target, selected_log_mode(args.logs, args.no_logs, false));
    }
    Ok(report.to_json(
        &target,
        reference.relays.first(),
        crate::cli::LogTailMode::All,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::selected_log_mode;
    use crate::cli::LogTailMode;

    #[test]
    fn human_failures_are_default_but_json_tails_remain_opt_in() {
        assert_eq!(selected_log_mode(false, false, false), LogTailMode::Auto);
        assert_eq!(selected_log_mode(false, true, false), LogTailMode::None);
        assert_eq!(selected_log_mode(false, false, true), LogTailMode::None);
        assert_eq!(selected_log_mode(true, false, true), LogTailMode::All);
    }
}
