use std::path::Path;

use anyhow::{Context, Result, bail};
use ngit::{
    client::{get_issues_from_cache, get_proposals_and_revisions_from_cache},
    git_events::{event_is_revision_root, tag_value},
    repo_ref::RepoRef,
};
use nostr::{EventId, FromBech32, ToBech32, nips::nip19::Nip19};

use crate::git_events::event_to_cover_letter;

pub fn parse_event_id(id: &str) -> Result<EventId> {
    let id = id.trim();

    if let Ok(nip19) = Nip19::from_bech32(id) {
        match nip19 {
            Nip19::Event(e) => return Ok(e.event_id),
            Nip19::EventId(event_id) => return Ok(event_id),
            _ => {}
        }
    }

    let hex = id.strip_prefix('#').unwrap_or(id);
    EventId::from_hex(hex).with_context(|| format!("invalid event-id or nevent: {id}"))
}

pub fn resolve_event_id_or_prefix<'a, I, F>(
    id: &str,
    candidates: I,
    item_name: &str,
    describe: F,
) -> Result<EventId>
where
    I: IntoIterator<Item = &'a nostr::Event>,
    F: Fn(&nostr::Event) -> String,
{
    let id = id.trim();

    if !id.starts_with('#') {
        if let Ok(event_id) = parse_event_id(id) {
            return Ok(event_id);
        }
    }

    let prefix = id.strip_prefix('#').unwrap_or(id).to_ascii_lowercase();
    if prefix.is_empty() || prefix.len() >= 64 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        return parse_event_id(id);
    }

    let matches: Vec<&nostr::Event> = candidates
        .into_iter()
        .filter(|event| event.id.to_hex().starts_with(&prefix))
        .collect();

    match matches.as_slice() {
        [] => bail!("no {item_name} matches event-id prefix #{prefix}"),
        [only] => Ok(only.id),
        _ => bail!(
            "event-id prefix #{prefix} matches multiple {item_name}s:\n{}\nspecify a longer prefix, full event-id, or nevent",
            matching_items(&matches, describe)
        ),
    }
}

pub fn resolve_event_or_prefix<'a, I, F>(
    id: &str,
    candidates: I,
    item_name: &str,
    describe: F,
) -> Result<&'a nostr::Event>
where
    I: IntoIterator<Item = &'a nostr::Event>,
    F: Fn(&nostr::Event) -> String,
{
    let candidates: Vec<&nostr::Event> = candidates.into_iter().collect();
    let event_id = resolve_event_id_or_prefix(id, candidates.iter().copied(), item_name, describe)?;

    candidates
        .into_iter()
        .find(|event| event.id == event_id)
        .with_context(|| {
            format!(
                "{item_name} with id {} not found in cache",
                event_id.to_hex()
            )
        })
}

pub fn proposal_roots(events: &[nostr::Event]) -> impl Iterator<Item = &nostr::Event> {
    events.iter().filter(|event| !event_is_revision_root(event))
}

pub fn pr_description(event: &nostr::Event) -> String {
    event_to_cover_letter(event).map_or_else(|_| String::new(), |cover| cover.title)
}

pub fn issue_description(issue: &nostr::Event) -> String {
    tag_value(issue, "subject")
        .ok()
        .filter(|subject| !subject.is_empty())
        .unwrap_or_else(|| {
            issue
                .content
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
}

pub fn resolve_pr_root_or_prefix<'a, I, F>(
    id: &str,
    candidates: I,
    describe: F,
) -> Result<&'a nostr::Event>
where
    I: IntoIterator<Item = &'a nostr::Event>,
    F: Fn(&nostr::Event) -> String,
{
    resolve_event_or_prefix(
        id,
        candidates
            .into_iter()
            .filter(|event| !event_is_revision_root(event)),
        "PR",
        describe,
    )
}

pub fn resolve_pr_root_id_or_prefix<'a, I, F>(
    id: &str,
    candidates: I,
    describe: F,
) -> Result<EventId>
where
    I: IntoIterator<Item = &'a nostr::Event>,
    F: Fn(&nostr::Event) -> String,
{
    let candidates: Vec<&nostr::Event> = candidates
        .into_iter()
        .filter(|event| !event_is_revision_root(event))
        .collect();
    let event_id = resolve_event_id_or_prefix(id, candidates.iter().copied(), "PR", describe)?;

    if candidates.iter().any(|event| event.id == event_id) {
        Ok(event_id)
    } else {
        bail!("PR with id {} not found in cache", event_id.to_hex())
    }
}

pub fn resolve_issue_or_prefix<'a, I, F>(
    id: &str,
    candidates: I,
    describe: F,
) -> Result<&'a nostr::Event>
where
    I: IntoIterator<Item = &'a nostr::Event>,
    F: Fn(&nostr::Event) -> String,
{
    resolve_event_or_prefix(id, candidates, "issue", describe)
}

pub fn resolve_issue_id_or_prefix<'a, I, F>(id: &str, candidates: I, describe: F) -> Result<EventId>
where
    I: IntoIterator<Item = &'a nostr::Event>,
    F: Fn(&nostr::Event) -> String,
{
    let candidates: Vec<&nostr::Event> = candidates.into_iter().collect();
    let event_id = resolve_event_id_or_prefix(id, candidates.iter().copied(), "issue", describe)?;

    if candidates.iter().any(|event| event.id == event_id) {
        Ok(event_id)
    } else {
        bail!("issue with id {} not found in cache", event_id.to_hex())
    }
}

pub async fn load_and_resolve_pr_root(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    id: &str,
) -> Result<nostr::Event> {
    let proposals_and_revisions =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates()).await?;
    Ok(resolve_pr_root_or_prefix(id, proposals_and_revisions.iter(), pr_description)?.clone())
}

pub async fn load_and_resolve_issue(
    git_repo_path: &Path,
    repo_ref: &RepoRef,
    id: &str,
) -> Result<nostr::Event> {
    let issues = get_issues_from_cache(git_repo_path, repo_ref.coordinates()).await?;
    Ok(resolve_issue_or_prefix(id, issues.iter(), issue_description)?.clone())
}

fn matching_items<F>(matches: &[&nostr::Event], describe: F) -> String
where
    F: Fn(&nostr::Event) -> String,
{
    matches
        .iter()
        .map(|event| {
            let description = describe(event).trim().to_string();
            let context = event_context(event);
            if description.is_empty() {
                format!("  {}  {context}", event.id)
            } else {
                format!("  {}  {context}  {description}", event.id)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn event_context(event: &nostr::Event) -> String {
    let author = event
        .pubkey
        .to_bech32()
        .unwrap_or_else(|_| event.pubkey.to_hex());
    let author_shorthand = &author[..author.len().min(16)];
    format!("kind={} author={author_shorthand}", event.kind.as_u16())
}

#[cfg(test)]
mod tests {
    use nostr::{
        Keys, Tag, ToBech32,
        event::{EventBuilder, FinalizeEvent},
    };

    use super::{
        parse_event_id, resolve_event_id_or_prefix, resolve_event_or_prefix,
        resolve_pr_root_id_or_prefix, resolve_pr_root_or_prefix,
    };

    fn make_event(content: &str) -> nostr::Event {
        EventBuilder::new(nostr::Kind::TextNote, content)
            .finalize(&Keys::generate())
            .expect("test event should finalize")
    }

    fn make_revision_root(content: &str) -> nostr::Event {
        EventBuilder::new(nostr::Kind::GitPatch, content)
            .tags([Tag::parse(["t", "revision-root"]).expect("tag parses")])
            .finalize(&Keys::generate())
            .expect("test event should finalize")
    }

    #[test]
    fn parse_event_id_accepts_hash_prefixed_full_hex() {
        let event = make_event("exact");
        let parsed = parse_event_id(&format!("#{}", event.id)).expect("hash-prefixed id parses");

        assert_eq!(parsed, event.id);
    }

    #[test]
    fn parse_event_id_accepts_nevent() {
        let event = make_event("nevent");
        let nevent = event.id.to_bech32().expect("event id encodes");
        let parsed = parse_event_id(&nevent).expect("nevent parses");

        assert_eq!(parsed, event.id);
    }

    #[test]
    fn resolve_event_id_or_prefix_accepts_plain_and_hash_shorthand() {
        let event = make_event("target");
        let other = make_event("other");
        let events = [event.clone(), other];
        let prefix = &event.id.to_hex()[..8];

        let plain =
            resolve_event_id_or_prefix(prefix, events.iter(), "proposal", |_| String::new())
                .expect("plain shorthand resolves");
        let hash =
            resolve_event_id_or_prefix(&format!("#{prefix}"), events.iter(), "proposal", |_| {
                String::new()
            })
            .expect("hash shorthand resolves");

        assert_eq!(plain, event.id);
        assert_eq!(hash, event.id);
    }

    #[test]
    fn resolve_event_or_prefix_returns_matching_event() {
        let event = make_event("target");
        let other = make_event("other");
        let events = [event.clone(), other];
        let prefix = &event.id.to_hex()[..8];

        let resolved = resolve_event_or_prefix(prefix, events.iter(), "issue", |_| String::new())
            .expect("event resolves");

        assert_eq!(resolved.id, event.id);
    }

    #[test]
    fn resolve_event_id_or_prefix_reports_ambiguous_matches() {
        let mut events = Vec::new();
        loop {
            let event = make_event(&format!("event-{}", events.len()));
            let first = event.id.to_hex()[..1].to_string();
            if events
                .iter()
                .any(|existing: &nostr::Event| existing.id.to_hex().starts_with(&first))
            {
                events.push(event);
                let err = resolve_event_id_or_prefix(&first, events.iter(), "issue", |event| {
                    event.content.clone()
                })
                .expect_err("ambiguous prefix should fail")
                .to_string();

                assert!(err.contains("matches multiple issues"));
                assert!(err.contains("specify a longer prefix"));
                assert!(err.contains("event-"));
                break;
            }
            events.push(event);
        }
    }

    #[test]
    fn resolve_pr_root_or_prefix_ignores_revision_roots() {
        let revision = make_revision_root("revision-root");
        let events = [revision.clone()];
        let prefix = &revision.id.to_hex()[..8];

        let prefix_err = resolve_pr_root_or_prefix(prefix, events.iter(), |_| String::new())
            .expect_err("revision-root prefix must not resolve as a PR root")
            .to_string();
        let full_err =
            resolve_pr_root_or_prefix(&revision.id.to_hex(), events.iter(), |_| String::new())
                .expect_err("full revision-root id must not resolve as a PR root")
                .to_string();

        assert!(prefix_err.contains("no PR matches event-id prefix"));
        assert!(full_err.contains("not found in cache"));
    }

    #[test]
    fn resolve_pr_root_id_or_prefix_verifies_full_ids_are_roots() {
        let revision = make_revision_root("revision-root");
        let events = [revision.clone()];

        let err =
            resolve_pr_root_id_or_prefix(&revision.id.to_hex(), events.iter(), |_| String::new())
                .expect_err("full revision-root id must not resolve as a PR root id")
                .to_string();

        assert!(err.contains("PR with id"));
        assert!(err.contains("not found in cache"));
    }
}
