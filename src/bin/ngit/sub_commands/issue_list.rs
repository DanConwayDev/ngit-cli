use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, get_events_from_local_cache, get_issues_from_cache},
    git_events::{KIND_COMMENT, get_status, status_kinds, tag_value},
};
use nostr::{
    FromBech32, ToBech32,
    filter::{Alphabet, SingleLetterTag},
    nips::nip19::Nip19,
};
use nostr_sdk::Kind;

use crate::{
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions},
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

fn get_issue_title(event: &nostr::Event) -> String {
    tag_value(event, "subject")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let first_line = event
                .content
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if first_line.is_empty() {
                event.id.to_string()
            } else {
                first_line
            }
        })
}

fn get_issue_hashtags(event: &nostr::Event) -> Vec<String> {
    event
        .tags
        .iter()
        .filter(|t| {
            let s = t.as_slice();
            s.len() >= 2 && s[0].eq("t")
        })
        .map(|t| t.as_slice()[1].clone())
        .collect()
}

fn status_kind_to_str(kind: Kind) -> &'static str {
    match kind {
        Kind::GitStatusOpen => "open",
        Kind::GitStatusDraft => "draft",
        Kind::GitStatusClosed => "closed",
        Kind::GitStatusApplied => "applied",
        _ => "unknown",
    }
}

/// Fetch NIP-22 kind-1111 comment counts per issue from the local cache.
/// Returns a map from issue `EventId` to comment count.
async fn get_comment_counts(
    git_repo_path: &std::path::Path,
    issues: &[nostr::Event],
) -> Result<HashMap<nostr::EventId, usize>> {
    if issues.is_empty() {
        return Ok(HashMap::new());
    }

    // Comments use an uppercase `E` tag pointing to the root event ID.
    let comments = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .custom_tags(
                    SingleLetterTag::uppercase(Alphabet::E),
                    issues.iter().map(|e| e.id),
                )
                .kind(KIND_COMMENT),
        ],
    )
    .await?;

    let mut counts: HashMap<nostr::EventId, usize> = HashMap::new();
    for comment in &comments {
        // Find the uppercase E tag that matches one of our issue IDs.
        for tag in comment.tags.iter() {
            let s = tag.as_slice();
            if s.len() >= 2 && s[0].eq("E") {
                if let Ok(root_id) = nostr::EventId::parse(&s[1]) {
                    if issues.iter().any(|e| e.id == root_id) {
                        *counts.entry(root_id).or_insert(0) += 1;
                        break;
                    }
                }
            }
        }
    }
    Ok(counts)
}

/// Fetch NIP-22 kind-1111 comments for a single issue, sorted oldest-first.
async fn get_comments_for_issue(
    git_repo_path: &std::path::Path,
    issue_id: &nostr::EventId,
) -> Result<Vec<nostr::Event>> {
    let mut comments = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::Filter::default()
                .custom_tags(
                    SingleLetterTag::uppercase(Alphabet::E),
                    std::iter::once(*issue_id),
                )
                .kind(KIND_COMMENT),
        ],
    )
    .await?;
    comments.retain(|c| {
        c.tags.iter().any(|t| {
            let s = t.as_slice();
            s.len() >= 2
                && s[0].eq("E")
                && nostr::EventId::parse(&s[1])
                    .map(|id| id == *issue_id)
                    .unwrap_or(false)
        })
    });
    comments.sort_by_key(|e| e.created_at);
    Ok(comments)
}

#[allow(clippy::too_many_lines)]
pub async fn launch(
    status: String,
    hashtag: Option<String>,
    json: bool,
    id: Option<String>,
    offline: bool,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !offline {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let issues: Vec<nostr::Event> =
        get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    if issues.is_empty() {
        println!("no issues found");
        return Ok(());
    }

    let statuses: Vec<nostr::Event> = {
        let mut statuses = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(issues.iter().map(|e| e.id)),
                nostr::Filter::default()
                    .custom_tags(
                        SingleLetterTag::uppercase(Alphabet::E),
                        issues.iter().map(|e| e.id),
                    )
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };

    let comment_counts = get_comment_counts(git_repo_path, &issues).await?;

    let status_filter: HashSet<&str> = status.split(',').map(str::trim).collect();

    let hashtag_filter: Option<HashSet<String>> = hashtag.map(|h| {
        h.split(',')
            .map(|s| s.trim().to_lowercase())
            .collect::<HashSet<String>>()
    });

    // Use an empty vec as the "all_pr_roots" argument — issues don't have PR
    // revisions, so we pass an empty slice.
    let empty_proposals: Vec<nostr::Event> = vec![];

    let filtered: Vec<(&nostr::Event, Kind, Vec<String>, usize)> = issues
        .iter()
        .filter_map(|issue| {
            let status_kind = get_status(issue, &repo_ref, &statuses, &empty_proposals);
            let status_str = status_kind_to_str(status_kind);
            if !status_filter.contains(status_str) && !status_filter.contains("unknown") {
                return None;
            }
            let tags = get_issue_hashtags(issue);
            if let Some(ref hf) = hashtag_filter {
                let issue_tags_lower: HashSet<String> =
                    tags.iter().map(|t| t.to_lowercase()).collect();
                if !hf.iter().any(|h| issue_tags_lower.contains(h)) {
                    return None;
                }
            }
            let comment_count = comment_counts.get(&issue.id).copied().unwrap_or(0);
            Some((issue, status_kind, tags, comment_count))
        })
        .collect();

    if filtered.is_empty() {
        println!("no issues found matching the given filters");
        return Ok(());
    }

    if let Some(ref event_id_or_nevent) = id {
        // Resolve the target issue ID so we can fetch its comments.
        let target_id = if event_id_or_nevent.starts_with("nevent") {
            let nip19 = nostr::nips::nip19::Nip19::from_bech32(event_id_or_nevent)
                .context("failed to parse nevent")?;
            match nip19 {
                nostr::nips::nip19::Nip19::EventId(id) => id,
                nostr::nips::nip19::Nip19::Event(event) => event.event_id,
                _ => anyhow::bail!("invalid nevent format"),
            }
        } else {
            nostr::EventId::from_hex(event_id_or_nevent).context("failed to parse event id")?
        };
        let comments = get_comments_for_issue(git_repo_path, &target_id).await?;
        return show_issue_details(&filtered, event_id_or_nevent, json, &comments);
    }

    if json {
        output_json(&filtered)?;
    } else {
        output_table(&filtered, &status, hashtag_filter.as_ref());
    }

    Ok(())
}

fn show_issue_details(
    issues: &[(&nostr::Event, Kind, Vec<String>, usize)],
    event_id_or_nevent: &str,
    json: bool,
    comments: &[nostr::Event],
) -> Result<()> {
    let target_id = if event_id_or_nevent.starts_with("nevent") {
        let nip19 = Nip19::from_bech32(event_id_or_nevent).context("failed to parse nevent")?;
        match nip19 {
            Nip19::EventId(id) => id,
            Nip19::Event(event) => event.event_id,
            _ => bail!("invalid nevent format"),
        }
    } else {
        nostr::EventId::from_hex(event_id_or_nevent).context("failed to parse event id")?
    };

    let (issue, status_kind, tags, _comment_count) = issues
        .iter()
        .find(|(e, _, _, _)| e.id == target_id)
        .context("issue not found")?;

    let title = get_issue_title(issue);
    let status = status_kind_to_str(*status_kind);

    if json {
        let comments_json: Vec<serde_json::Value> = comments
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id.to_string(),
                    "author": c.pubkey.to_bech32().unwrap_or_default(),
                    "created_at": c.created_at.as_secs(),
                    "body": c.content,
                })
            })
            .collect();
        let json_output = serde_json::json!({
            "id": issue.id.to_string(),
            "status": status,
            "title": title,
            "author": issue.pubkey.to_bech32().unwrap_or_default(),
            "hashtags": tags,
            "comments": comments_json,
            "description": issue.content,
        });
        println!("{}", serde_json::to_string_pretty(&json_output)?);
        return Ok(());
    }

    println!("Title:    {title}");
    println!("Author:   {}", issue.pubkey.to_bech32().unwrap_or_default());
    println!("Status:   {status}");
    if !tags.is_empty() {
        let tags_str = tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!("Tags:     {tags_str}");
    }

    if !issue.content.is_empty() {
        println!();
        for line in issue.content.lines() {
            println!("  {line}");
        }
    }

    if comments.is_empty() {
        println!("Comments: 0");
    } else {
        println!();
        println!("Comments ({}):", comments.len());
        let dim = console::Style::new().color256(247);
        for comment in comments {
            let author = comment.pubkey.to_bech32().unwrap_or_default();
            let ts = chrono_timestamp(comment.created_at.as_secs());
            println!();
            println!("{}", dim.apply_to(format!("  {author}  {ts}")));
            for line in comment.content.lines() {
                println!("  {line}");
            }
        }
    }

    Ok(())
}

fn chrono_timestamp(unix_secs: u64) -> String {
    let secs = unix_secs % 60;
    let mins = (unix_secs / 60) % 60;
    let hours = (unix_secs / 3600) % 24;
    let days_since_epoch = unix_secs / 86400;

    let z = days_since_epoch + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let d = day_of_year - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hours:02}:{mins:02}:{secs:02} UTC")
}

fn output_table(
    issues: &[(&nostr::Event, Kind, Vec<String>, usize)],
    status_filter: &str,
    hashtag_filter: Option<&HashSet<String>>,
) {
    println!("{:<66} {:<8} {:<5} TITLE  HASHTAGS", "ID", "STATUS", "CMTS");
    for (issue, status_kind, tags, comment_count) in issues {
        let id = issue.id.to_string();
        let status = status_kind_to_str(*status_kind);
        let title = get_issue_title(issue);
        let tags_str = if tags.is_empty() {
            String::new()
        } else {
            tags.iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        if tags_str.is_empty() {
            println!("{id:<66} {status:<8} {comment_count:<5} {title}");
        } else {
            println!("{id:<66} {status:<8} {comment_count:<5} {title}  {tags_str}");
        }
    }

    println!();
    print!("--status {status_filter}");
    if let Some(hf) = hashtag_filter {
        let tags: Vec<&String> = hf.iter().collect();
        print!(
            "  --hashtag {}",
            tags.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    println!();
}

fn output_json(issues: &[(&nostr::Event, Kind, Vec<String>, usize)]) -> Result<()> {
    let json_output: Vec<serde_json::Value> = issues
        .iter()
        .map(|(issue, status_kind, tags, comment_count)| {
            serde_json::json!({
                "id": issue.id.to_string(),
                "status": status_kind_to_str(*status_kind),
                "title": get_issue_title(issue),
                "author": issue.pubkey.to_bech32().unwrap_or_default(),
                "hashtags": tags,
                "comments": comment_count,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json_output)?);
    Ok(())
}
