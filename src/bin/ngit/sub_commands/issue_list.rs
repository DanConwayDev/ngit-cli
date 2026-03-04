use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use ngit::{
    client::{Params, get_issues_from_cache},
    git_events::{get_status, status_kinds, tag_value},
};
use nostr::{
    FromBech32,
    filter::{Alphabet, SingleLetterTag},
    nips::nip19::Nip19,
};
use nostr_sdk::Kind;

use crate::{
    client::{Client, Connect, fetching_with_report, get_events_from_local_cache, get_repo_ref_from_cache},
    git::{Repo, RepoActions},
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

fn get_issue_title(event: &nostr::Event) -> String {
    tag_value(event, "subject")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let first_line = event.content.lines().next().unwrap_or("").trim().to_string();
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

    let status_filter: HashSet<&str> = status.split(',').map(str::trim).collect();

    let hashtag_filter: Option<HashSet<String>> = hashtag.map(|h| {
        h.split(',')
            .map(|s| s.trim().to_lowercase())
            .collect::<HashSet<String>>()
    });

    // Use an empty vec as the "all_pr_roots" argument — issues don't have PR
    // revisions, so we pass an empty slice.
    let empty_proposals: Vec<nostr::Event> = vec![];

    let filtered: Vec<(&nostr::Event, Kind, Vec<String>)> = issues
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
            Some((issue, status_kind, tags))
        })
        .collect();

    if filtered.is_empty() {
        println!("no issues found matching the given filters");
        return Ok(());
    }

    if let Some(ref event_id_or_nevent) = id {
        return show_issue_details(&filtered, event_id_or_nevent, json);
    }

    if json {
        output_json(&filtered)?;
    } else {
        output_table(&filtered, &status, hashtag_filter.as_ref());
    }

    Ok(())
}

fn show_issue_details(
    issues: &[(&nostr::Event, Kind, Vec<String>)],
    event_id_or_nevent: &str,
    json: bool,
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

    let (issue, status_kind, tags) = issues
        .iter()
        .find(|(e, _, _)| e.id == target_id)
        .context("issue not found")?;

    let title = get_issue_title(issue);
    let status = status_kind_to_str(*status_kind);

    if json {
        use nostr::ToBech32;
        let json_output = serde_json::json!({
            "id": issue.id.to_string(),
            "status": status,
            "title": title,
            "author": issue.pubkey.to_bech32().unwrap_or_default(),
            "hashtags": tags,
            "description": issue.content,
        });
        println!("{}", serde_json::to_string_pretty(&json_output)?);
        return Ok(());
    }

    println!("Title:  {title}");
    use nostr::ToBech32;
    println!("Author: {}", issue.pubkey.to_bech32().unwrap_or_default());
    println!("Status: {status}");
    if !tags.is_empty() {
        let tags_str = tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ");
        println!("Tags:   {tags_str}");
    }

    if !issue.content.is_empty() {
        println!();
        for line in issue.content.lines() {
            println!("  {line}");
        }
    }

    Ok(())
}

fn output_table(
    issues: &[(&nostr::Event, Kind, Vec<String>)],
    status_filter: &str,
    hashtag_filter: Option<&HashSet<String>>,
) {
    println!("{:<66} {:<8} TITLE  HASHTAGS", "ID", "STATUS");
    for (issue, status_kind, tags) in issues {
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
            println!("{id:<66} {status:<8} {title}");
        } else {
            println!("{id:<66} {status:<8} {title}  {tags_str}");
        }
    }

    println!();
    print!("--status {status_filter}");
    if let Some(hf) = hashtag_filter {
        let tags: Vec<&String> = hf.iter().collect();
        print!("  --hashtag {}", tags.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","));
    }
    println!();
}

fn output_json(issues: &[(&nostr::Event, Kind, Vec<String>)]) -> Result<()> {
    use nostr::ToBech32;
    let json_output: Vec<serde_json::Value> = issues
        .iter()
        .map(|(issue, status_kind, tags)| {
            serde_json::json!({
                "id": issue.id.to_string(),
                "status": status_kind_to_str(*status_kind),
                "title": get_issue_title(issue),
                "author": issue.pubkey.to_bech32().unwrap_or_default(),
                "hashtags": tags,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json_output)?);
    Ok(())
}
