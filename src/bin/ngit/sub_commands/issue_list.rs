use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use ngit::{
    client::{Params, get_events_from_local_cache, get_issues_from_cache},
    git_events::{
        KIND_COMMENT, KIND_COVER_NOTE, KIND_LABEL, get_labels_and_subject, get_status,
        process_cover_note, status_kinds, tag_value,
    },
};
use nostr::prelude::{Kind, RelayUrl, ToBech32, filter::SingleLetterTag, nip19::Nip19Event};

use crate::{
    cli::SignerParams,
    client::{Client, Connect, get_repo_ref_from_cache, warn_if_invited_as_maintainer},
    git::{Repo, RepoActions},
    repo_ref::get_repo_coordinates_when_remote_unknown,
    sub_commands::{
        id_resolver::resolve_issue_id_or_prefix, repository_fetch::fetching_with_account,
    },
};

/// `(event, status_kind, labels, comment_count, subject_override)`
type IssueRow<'a> = (
    &'a nostr::prelude::Event,
    Kind,
    Vec<String>,
    usize,
    Option<String>,
);

fn get_issue_title(event: &nostr::prelude::Event, subject_override: Option<&str>) -> String {
    if let Some(s) = subject_override {
        return s.to_string();
    }
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
    issues: &[nostr::prelude::Event],
) -> Result<HashMap<nostr::prelude::EventId, usize>> {
    if issues.is_empty() {
        return Ok(HashMap::new());
    }

    // Comments use an uppercase `E` tag pointing to the root event ID.
    let comments = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .custom_tags(SingleLetterTag::UPPERCASE_E, issues.iter().map(|e| e.id))
                .kind(KIND_COMMENT),
        ],
    )
    .await?;

    let mut counts: HashMap<nostr::prelude::EventId, usize> = HashMap::new();
    for comment in &comments {
        // Find the uppercase E tag that matches one of our issue IDs.
        for tag in comment.tags.iter() {
            let s = tag.as_slice();
            if s.len() >= 2 && s[0].eq("E") {
                if let Ok(root_id) = nostr::prelude::EventId::parse(&s[1]) {
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
    issue_id: &nostr::prelude::EventId,
) -> Result<Vec<nostr::prelude::Event>> {
    let mut comments = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .custom_tags(SingleLetterTag::UPPERCASE_E, std::iter::once(*issue_id))
                .kind(KIND_COMMENT),
        ],
    )
    .await?;
    comments.retain(|c| {
        c.tags.iter().any(|t| {
            let s = t.as_slice();
            s.len() >= 2
                && s[0].eq("E")
                && nostr::prelude::EventId::parse(&s[1]).is_ok_and(|id| id == *issue_id)
        })
    });
    comments.sort_by_key(|e| e.created_at);
    Ok(comments)
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub async fn launch(
    status: String,
    labels: Vec<String>,
    json: bool,
    show_comments: bool,
    show_history: bool,
    id: Option<String>,
    offline: bool,
    auth: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let mut repo_coordinates =
        get_repo_coordinates_when_remote_unknown(&git_repo, &mut client).await?;

    if !offline {
        fetching_with_account(
            &git_repo,
            git_repo_path,
            &mut client,
            &mut repo_coordinates,
            auth,
        )
        .await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    let issues: Vec<nostr::prelude::Event> =
        get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;

    if issues.is_empty() {
        if json {
            crate::output::set(Vec::<serde_json::Value>::new())?;
        } else {
            println!("no issues found");
        }
        return Ok(());
    }

    let statuses: Vec<nostr::prelude::Event> = {
        let mut statuses = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(issues.iter().map(|e| e.id)),
                nostr::prelude::Filter::default()
                    .custom_tags(SingleLetterTag::UPPERCASE_E, issues.iter().map(|e| e.id))
                    .kinds(status_kinds().clone()),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };

    // Fetch NIP-32 kind-1985 label events for all issues.
    let label_events: Vec<nostr::prelude::Event> = get_events_from_local_cache(
        git_repo_path,
        vec![
            nostr::prelude::Filter::default()
                .events(issues.iter().map(|e| e.id))
                .kind(KIND_LABEL),
        ],
    )
    .await?;

    let comment_counts = get_comment_counts(git_repo_path, &issues).await?;

    let status_filter: HashSet<&str> = status.split(',').map(str::trim).collect();

    // OR filter: issue must have at least one of the requested labels.
    let label_filter: HashSet<String> = labels.iter().map(|l| l.trim().to_lowercase()).collect();

    // Use an empty vec as the "all_pr_roots" argument — issues don't have PR
    // revisions, so we pass an empty slice.
    let empty_proposals: Vec<nostr::prelude::Event> = vec![];

    let filtered: Vec<IssueRow<'_>> = issues
        .iter()
        .filter_map(|issue| {
            let status_kind = get_status(issue, &repo_ref, &statuses, &empty_proposals);
            let status_str = status_kind_to_str(status_kind);
            if !status_filter.contains(status_str) && !status_filter.contains("unknown") {
                return None;
            }
            let (issue_labels, subject_override) =
                get_labels_and_subject(issue, &repo_ref, &label_events);
            if !label_filter.is_empty() {
                let issue_labels_lower: HashSet<String> =
                    issue_labels.iter().map(|t| t.to_lowercase()).collect();
                if !label_filter.iter().any(|l| issue_labels_lower.contains(l)) {
                    return None;
                }
            }
            let comment_count = comment_counts.get(&issue.id).copied().unwrap_or(0);
            Some((
                issue,
                status_kind,
                issue_labels,
                comment_count,
                subject_override,
            ))
        })
        .collect();

    if filtered.is_empty() {
        if json {
            crate::output::set(Vec::<serde_json::Value>::new())?;
        } else {
            println!("no issues found matching the given filters");
        }
        return Ok(());
    }

    if let Some(ref event_id_or_nevent) = id {
        // Resolve the target issue ID so we can fetch its comments.
        let target_id = resolve_issue_id_or_prefix(
            event_id_or_nevent,
            filtered.iter().map(|(issue, _, _, _, _)| *issue),
            |issue| describe_issue_row(issue, &filtered),
        )?;
        let comments = if show_comments {
            get_comments_for_issue(git_repo_path, &target_id).await?
        } else {
            vec![]
        };
        // Fetch kind-1624 cover note events for this issue.
        let cover_note_events = get_events_from_local_cache(
            git_repo_path,
            vec![
                nostr::prelude::Filter::default()
                    .event(target_id)
                    .kind(KIND_COVER_NOTE),
            ],
        )
        .await?;
        let relay_hint = repo_ref.relays.first();
        return show_issue_details(
            &filtered,
            target_id,
            json,
            show_comments,
            show_history,
            &comments,
            &label_events,
            &cover_note_events,
            &repo_ref,
            relay_hint,
        );
    }

    let relay_hint = repo_ref.relays.first();
    if json {
        output_json(&filtered, relay_hint)?;
    } else {
        output_table(&filtered, &status, &label_filter);
    }

    Ok(())
}

/// Extract the parent comment ID from a NIP-22 comment event.
/// Returns `Some(id)` when the lowercase `e` tag differs from the root `E` tag
/// (i.e. the comment is a reply to another comment, not a top-level comment).
fn comment_reply_to(comment: &nostr::prelude::Event) -> Option<nostr::prelude::EventId> {
    let root_id = comment.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.len() >= 2 && s[0].eq("E") {
            nostr::prelude::EventId::parse(&s[1]).ok()
        } else {
            None
        }
    })?;
    comment.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.len() >= 2 && s[0].eq("e") {
            let parent_id = nostr::prelude::EventId::parse(&s[1]).ok()?;
            if parent_id == root_id {
                None
            } else {
                Some(parent_id)
            }
        } else {
            None
        }
    })
}

fn describe_issue_row(issue: &nostr::prelude::Event, rows: &[IssueRow<'_>]) -> String {
    let Some((_, status_kind, labels, comment_count, subject_override)) =
        rows.iter().find(|(row, _, _, _, _)| row.id == issue.id)
    else {
        return get_issue_title(issue, None);
    };

    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(
            " labels={}",
            labels
                .iter()
                .map(|label| format!("#{label}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!(
        "status={} comments={comment_count}{labels} {}",
        status_kind_to_str(*status_kind),
        get_issue_title(issue, subject_override.as_deref())
    )
}

fn issue_author_role(
    issue: &nostr::prelude::Event,
    author: nostr::prelude::PublicKey,
    confirmed_maintainers: &[nostr::prelude::PublicKey],
    confirmed_moderators: &[nostr::prelude::PublicKey],
) -> &'static str {
    if author == issue.pubkey {
        "author"
    } else if confirmed_maintainers.contains(&author) {
        "maintainer"
    } else if confirmed_moderators.contains(&author) {
        "moderator"
    } else {
        "unknown"
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn show_issue_details(
    issues: &[IssueRow<'_>],
    target_id: nostr::prelude::EventId,
    json: bool,
    show_comments: bool,
    show_history: bool,
    comments: &[nostr::prelude::Event],
    label_events: &[nostr::prelude::Event],
    cover_note_events: &[nostr::prelude::Event],
    repo_ref: &ngit::repo_ref::RepoRef,
    relay_hint: Option<&RelayUrl>,
) -> Result<()> {
    let (issue, status_kind, labels, comment_count, subject_override) = issues
        .iter()
        .find(|(e, _, _, _, _)| e.id == target_id)
        .context("issue not found")?;

    let title = get_issue_title(issue, subject_override.as_deref());
    let status = status_kind_to_str(*status_kind);

    // Resolve the effective cover note (kind 1624) for this issue.
    let cover_note = process_cover_note(issue, repo_ref, cover_note_events);
    let edit_history = if show_history {
        issue_edit_history(issue, repo_ref, label_events, cover_note_events, relay_hint)
    } else {
        vec![]
    };

    if json {
        let confirmed_maintainers = repo_ref.confirmed_maintainers();
        let confirmed_moderators = repo_ref.confirmed_moderators();
        let cover_note_json = cover_note.as_ref().map(|(cn, _)| {
            let author_role = issue_author_role(
                issue,
                cn.pubkey,
                &confirmed_maintainers,
                &confirmed_moderators,
            );
            let mut obj = serde_json::json!({
                "id": event_id_to_nevent(cn.id, relay_hint),
                "author": cn.pubkey.to_bech32().unwrap_or_default(),
                "author_role": author_role,
                "created_at": cn.created_at.as_secs(),
                "body": cn.content,
            });
            if author_role == "maintainer" {
                obj["by_maintainer"] = serde_json::Value::Bool(true);
            }
            obj
        });

        let mut json_obj = serde_json::json!({
            "id": event_id_to_nevent(issue.id, relay_hint),
            "status": status,
            "subject": title,
            "author": issue.pubkey.to_bech32().unwrap_or_default(),
            "labels": labels,
            "comment_count": comment_count,
            "description": issue.content,
        });
        if let Some(cn) = cover_note_json {
            json_obj["cover_note"] = cn;
        }
        if show_comments {
            let comments_json: Vec<serde_json::Value> = comments
                .iter()
                .map(|c| {
                    let reply_to = comment_reply_to(c).map(|id| event_id_to_nevent(id, relay_hint));
                    serde_json::json!({
                        "id": event_id_to_nevent(c.id, relay_hint),
                        "author": c.pubkey.to_bech32().unwrap_or_default(),
                        "created_at": c.created_at.as_secs(),
                        "reply_to": reply_to,
                        "body": c.content,
                    })
                })
                .collect();
            json_obj["comments"] = serde_json::Value::Array(comments_json);
        }
        if show_history {
            json_obj["edit_history"] = serde_json::Value::Array(edit_history);
        }
        crate::output::set_value(json_obj);
        return Ok(());
    }

    println!("Subject:  {title}");
    println!("Author:   {}", issue.pubkey.to_bech32().unwrap_or_default());
    println!("Status:   {status}");
    if !labels.is_empty() {
        let labels_str = labels
            .iter()
            .map(|l| format!("#{l}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!("Labels:   {labels_str}");
    }

    if let Some((cn, by_different_author)) = &cover_note {
        println!();
        if *by_different_author {
            println!(
                "Cover Note (by {}):",
                cn.pubkey.to_bech32().unwrap_or_default()
            );
        } else {
            println!("Cover Note:");
        }
        for line in cn.content.lines() {
            println!("  {line}");
        }
        // Show original description only when --comments is used.
        if show_comments && !issue.content.is_empty() {
            println!();
            println!("Original Description:");
            for line in issue.content.lines() {
                println!("  {line}");
            }
        }
    } else if !issue.content.is_empty() {
        println!();
        for line in issue.content.lines() {
            println!("  {line}");
        }
    }

    if show_comments {
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
                if let Some(parent_id) = comment_reply_to(comment) {
                    println!(
                        "{}",
                        dim.apply_to(format!("  ↳ reply to {}", &parent_id.to_hex()[..8]))
                    );
                }
                println!("{}", dim.apply_to(format!("  {author}  {ts}")));
                for line in comment.content.lines() {
                    println!("  {line}");
                }
            }
        }
    } else {
        println!("Comments: {comment_count}  (use --comments to view)");
    }

    if show_history {
        println!();
        println!("Edit History ({}):", edit_history.len());
        for entry in &edit_history {
            let kind = entry["kind"].as_str().unwrap_or("edit");
            let author = entry["author"].as_str().unwrap_or("");
            let created_at = entry["created_at"].as_u64().unwrap_or_default();
            println!();
            println!("  {kind}  {author}  {}", chrono_timestamp(created_at));
            if let Some(subject) = entry["subject"].as_str() {
                println!("  Subject: {subject}");
            }
            if let Some(body) = entry["body"].as_str() {
                for line in body.lines() {
                    println!("  {line}");
                }
            }
        }
    }

    Ok(())
}

fn issue_edit_history(
    issue: &nostr::prelude::Event,
    repo_ref: &ngit::repo_ref::RepoRef,
    label_events: &[nostr::prelude::Event],
    cover_note_events: &[nostr::prelude::Event],
    relay_hint: Option<&RelayUrl>,
) -> Vec<serde_json::Value> {
    let confirmed_maintainers = repo_ref.confirmed_maintainers();
    let confirmed_moderators = repo_ref.confirmed_moderators();
    let is_permitted = |event: &nostr::prelude::Event| {
        event.pubkey == issue.pubkey
            || confirmed_maintainers.contains(&event.pubkey)
            || confirmed_moderators.contains(&event.pubkey)
    };
    let references_issue = |event: &nostr::prelude::Event| {
        event.tags.iter().any(|tag| {
            let values = tag.as_slice();
            values.len() >= 2 && values[0] == "e" && values[1] == issue.id.to_string()
        })
    };

    let original_subject = tag_value(issue, "subject")
        .ok()
        .filter(|subject| !subject.is_empty())
        .unwrap_or_else(|| get_issue_title(issue, None));
    let original = serde_json::json!({
        "kind": "original",
        "id": event_id_to_nevent(issue.id, relay_hint),
        "author": issue.pubkey.to_bech32().unwrap_or_default(),
        "author_role": "author",
        "created_at": issue.created_at.as_secs(),
        "subject": original_subject,
        "body": issue.content,
    });
    let mut revisions = vec![];

    for event in label_events.iter().filter(|event| {
        event.kind == KIND_LABEL
            && is_permitted(event)
            && references_issue(event)
            && event.tags.iter().any(|tag| {
                let values = tag.as_slice();
                values.len() >= 2 && values[0] == "L" && values[1] == "#subject"
            })
    }) {
        let Some(subject) = event.tags.iter().find_map(|tag| {
            let values = tag.as_slice();
            (values.len() >= 3
                && values[0] == "l"
                && values[2] == "#subject"
                && !values[1].is_empty())
            .then(|| values[1].clone())
        }) else {
            continue;
        };
        let author_role = issue_author_role(
            issue,
            event.pubkey,
            &confirmed_maintainers,
            &confirmed_moderators,
        );
        let mut entry = serde_json::json!({
            "kind": "subject",
            "id": event_id_to_nevent(event.id, relay_hint),
            "author": event.pubkey.to_bech32().unwrap_or_default(),
            "author_role": author_role,
            "created_at": event.created_at.as_secs(),
            "subject": subject,
        });
        if author_role == "maintainer" {
            entry["by_maintainer"] = serde_json::Value::Bool(true);
        }
        revisions.push((event.created_at.as_secs(), event.id.to_hex(), entry));
    }

    for event in cover_note_events.iter().filter(|event| {
        event.kind == KIND_COVER_NOTE && is_permitted(event) && references_issue(event)
    }) {
        let author_role = issue_author_role(
            issue,
            event.pubkey,
            &confirmed_maintainers,
            &confirmed_moderators,
        );
        let mut entry = serde_json::json!({
            "kind": "description",
            "id": event_id_to_nevent(event.id, relay_hint),
            "author": event.pubkey.to_bech32().unwrap_or_default(),
            "author_role": author_role,
            "created_at": event.created_at.as_secs(),
            "body": event.content,
        });
        if author_role == "maintainer" {
            entry["by_maintainer"] = serde_json::Value::Bool(true);
        }
        revisions.push((event.created_at.as_secs(), event.id.to_hex(), entry));
    }

    revisions.sort_by(|(left_time, left_id, _), (right_time, right_id, _)| {
        left_time
            .cmp(right_time)
            .then_with(|| right_id.cmp(left_id))
    });
    std::iter::once(original)
        .chain(revisions.into_iter().map(|(_, _, entry)| entry))
        .collect()
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

fn output_table(issues: &[IssueRow<'_>], status_filter: &str, label_filter: &HashSet<String>) {
    println!("{:<66} {:<8} {:<5} TITLE  LABELS", "ID", "STATUS", "CMTS");
    for (issue, status_kind, labels, comment_count, subject_override) in issues {
        let id = issue.id.to_string();
        let status = status_kind_to_str(*status_kind);
        let title = get_issue_title(issue, subject_override.as_deref());
        let labels_str = if labels.is_empty() {
            String::new()
        } else {
            labels
                .iter()
                .map(|l| format!("#{l}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        if labels_str.is_empty() {
            println!("{id:<66} {status:<8} {comment_count:<5} {title}");
        } else {
            println!("{id:<66} {status:<8} {comment_count:<5} {title}  {labels_str}");
        }
    }

    println!();
    print!("--status {status_filter}");
    if !label_filter.is_empty() {
        for l in label_filter {
            print!("  --label {l}");
        }
    }
    println!();
}

/// Convert an event ID to a `nevent1…` bech32 string, including a relay hint
/// when one is available.  Falls back to the plain hex string on error.
fn event_id_to_nevent(event_id: nostr::prelude::EventId, relay: Option<&RelayUrl>) -> String {
    let relays = relay.map(|r| vec![r.clone()]).unwrap_or_default();
    Nip19Event {
        event_id,
        relays,
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_else(|_| event_id.to_hex())
}

fn output_json(issues: &[IssueRow<'_>], relay_hint: Option<&RelayUrl>) -> Result<()> {
    let json_output: Vec<serde_json::Value> = issues
        .iter()
        .map(
            |(issue, status_kind, labels, comment_count, subject_override)| {
                serde_json::json!({
                    "id": event_id_to_nevent(issue.id, relay_hint),
                    "status": status_kind_to_str(*status_kind),
                    "subject": get_issue_title(issue, subject_override.as_deref()),
                    "author": issue.pubkey.to_bech32().unwrap_or_default(),
                    "labels": labels,
                    "comment_count": comment_count,
                })
            },
        )
        .collect();
    crate::output::set(json_output)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ngit::repo_ref::RepoRef;
    use nostr::prelude::{EventBuilder, Keys, Tag, Timestamp, event::FinalizeEvent};

    use super::*;

    fn repo_ref(selected_maintainer: nostr::prelude::PublicKey) -> RepoRef {
        RepoRef {
            name: "test".to_string(),
            description: String::new(),
            identifier: "test".to_string(),
            root_commit: String::new(),
            git_server: vec![],
            web: vec![],
            upstream: vec![],
            relays: vec![],
            blossoms: vec![],
            hashtags: vec![],
            private: false,
            maintainers: vec![selected_maintainer],
            selected_maintainer,
            maintainers_without_annoucnement: None,
            events: HashMap::new(),
            nostr_git_url: None,
            extra_tags: vec![],
            role_tags: vec![],
            moderators: vec![],
            lead: None,
        }
    }

    fn subject_edit(
        keys: &Keys,
        issue: &nostr::prelude::Event,
        subject: &str,
        created_at: u64,
    ) -> nostr::prelude::Event {
        EventBuilder::new(KIND_LABEL, "")
            .tags([
                Tag::parse(["e", &issue.id.to_string()]).unwrap(),
                Tag::parse(["L", "#subject"]).unwrap(),
                Tag::parse(["l", subject, "#subject"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from_secs(created_at))
            .finalize(keys)
            .unwrap()
    }

    fn description_edit(
        keys: &Keys,
        issue: &nostr::prelude::Event,
        body: &str,
        created_at: u64,
    ) -> nostr::prelude::Event {
        EventBuilder::new(KIND_COVER_NOTE, body)
            .tag(Tag::parse(["e", &issue.id.to_string()]).unwrap())
            .custom_created_at(Timestamp::from_secs(created_at))
            .finalize(keys)
            .unwrap()
    }

    #[test]
    fn edit_history_keeps_every_authorised_revision_and_rejects_outsiders() {
        let author = Keys::generate();
        let maintainer = Keys::generate();
        let outsider = Keys::generate();
        let issue = EventBuilder::new(Kind::GitIssue, "original body")
            .tag(Tag::parse(["subject", "original subject"]).unwrap())
            .custom_created_at(Timestamp::from_secs(1))
            .finalize(&author)
            .unwrap();
        let labels = vec![
            subject_edit(&author, &issue, "second subject", 1),
            subject_edit(&outsider, &issue, "spoofed subject", 3),
            subject_edit(&maintainer, &issue, "maintainer subject", 4),
            subject_edit(&author, &issue, "final subject", 5),
        ];
        let covers = vec![
            description_edit(&maintainer, &issue, "maintainer body", 6),
            description_edit(&outsider, &issue, "spoofed body", 7),
            description_edit(&author, &issue, "final body", 8),
        ];

        let history = issue_edit_history(
            &issue,
            &repo_ref(maintainer.public_key()),
            &labels,
            &covers,
            None,
        );

        assert_eq!(history.len(), 6);
        assert_eq!(history[0]["kind"], "original");
        assert_eq!(history[0]["subject"], "original subject");
        assert_eq!(history[0]["body"], "original body");
        assert_eq!(history[0]["author_role"], "author");
        assert_eq!(history[1]["subject"], "second subject");
        assert_eq!(history[1]["author_role"], "author");
        assert!(history[1]["by_maintainer"].is_null());
        assert_eq!(history[2]["subject"], "maintainer subject");
        assert_eq!(history[2]["author_role"], "maintainer");
        assert_eq!(history[2]["by_maintainer"], true);
        assert_eq!(history[3]["subject"], "final subject");
        assert_eq!(history[3]["author_role"], "author");
        assert_eq!(history[4]["body"], "maintainer body");
        assert_eq!(history[4]["author_role"], "maintainer");
        assert_eq!(history[5]["body"], "final body");
        assert_eq!(history[5]["author_role"], "author");
        assert!(history.iter().all(|entry| {
            entry["subject"] != "spoofed subject" && entry["body"] != "spoofed body"
        }));
    }
}
