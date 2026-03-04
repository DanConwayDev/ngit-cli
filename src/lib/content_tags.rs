//! Parse `nostr:` URI mentions (NIP-21) from event content and produce the
//! corresponding NIP-22 `q` / `p` tags.
//!
//! Rules implemented:
//! - `nostr:npub1…` / `nostr:nprofile1…`  → `["p", "<pubkey-hex>", "<relay>"]`
//! - `nostr:note1…` / `nostr:nevent1…`    → `["q", "<event-id-hex>", "<relay>",
//!   "<pubkey>"]`
//! - `nostr:naddr1…`                       → `["q",
//!   "<kind>:<pubkey-hex>:<identifier>", "<relay>"]`
//!
//! Duplicate tags (same first two elements) are deduplicated within the content
//! scan.  Use [`dedup_tags`] after merging content tags with the rest of the
//! event's tag list to remove cross-source duplicates.

use std::{collections::HashSet, path::Path};

use anyhow::Result;
use nostr::{FromBech32, Tag, nips::nip19::Nip19};
use nostr_sdk::EventId;

use crate::client::get_events_from_local_cache;

/// Regex-free extraction of every `nostr:<bech32>` token from `content`.
fn extract_nostr_uris(content: &str) -> Vec<&str> {
    let mut uris = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find("nostr:") {
        let after = &remaining[start + 6..]; // skip "nostr:"
        // A bech32 token consists of alphanumeric chars (plus the separator '1').
        // We stop at the first non-bech32 character.
        let end = after
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        if end > 0 {
            uris.push(&remaining[start..start + 6 + end]);
        }
        remaining = &remaining[start + 6 + end..];
    }
    uris
}

/// Build `q` / `p` tags for every `nostr:` mention found in `content`.
///
/// `git_repo_path` is used for the optional local-cache lookup that fills in
/// the author pubkey of a cited regular event when it is not embedded in the
/// `nevent` bech32.
pub async fn tags_from_content(content: &str, git_repo_path: Option<&Path>) -> Result<Vec<Tag>> {
    let uris = extract_nostr_uris(content);
    if uris.is_empty() {
        return Ok(vec![]);
    }

    // Collect (tag_name, value0, value1_opt) tuples for deduplication.
    // We use the first two tag elements as the dedup key.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut tags: Vec<Tag> = Vec::new();

    for uri in uris {
        // Strip the "nostr:" prefix to get the raw bech32 string.
        let bech32 = &uri[6..];

        let Ok(nip19) = Nip19::from_bech32(bech32) else {
            continue;
        };

        match nip19 {
            // ── pubkey references → p tag ─────────────────────────────────
            Nip19::Pubkey(pk) => {
                let key = ("p".to_string(), pk.to_hex());
                if seen.insert(key) {
                    let Ok(tag) = Tag::parse(vec!["p".to_string(), pk.to_hex()]) else {
                        continue;
                    };
                    tags.push(tag);
                }
            }
            Nip19::Profile(profile) => {
                let key = ("p".to_string(), profile.public_key.to_hex());
                if seen.insert(key) {
                    let mut parts = vec!["p".to_string(), profile.public_key.to_hex()];
                    if let Some(relay) = profile.relays.first() {
                        parts.push(relay.to_string());
                    }
                    let Ok(tag) = Tag::parse(parts) else { continue };
                    tags.push(tag);
                }
            }

            // ── regular event references → q tag ─────────────────────────
            Nip19::EventId(event_id) => {
                let key = ("q".to_string(), event_id.to_hex());
                if seen.insert(key) {
                    // No relay or pubkey info available; attempt cache lookup.
                    let pubkey = lookup_event_pubkey(&event_id, git_repo_path).await;
                    let Ok(tag) = build_q_tag_for_event(event_id, None, pubkey) else {
                        continue;
                    };
                    tags.push(tag);
                }
            }
            Nip19::Event(nevent) => {
                let key = ("q".to_string(), nevent.event_id.to_hex());
                if seen.insert(key) {
                    let relay = nevent.relays.first().cloned();
                    // Prefer author embedded in nevent; fall back to cache lookup.
                    let pubkey = if nevent.author.is_some() {
                        nevent.author
                    } else {
                        lookup_event_pubkey(&nevent.event_id, git_repo_path).await
                    };
                    let Ok(tag) = build_q_tag_for_event(nevent.event_id, relay, pubkey) else {
                        continue;
                    };
                    tags.push(tag);
                }
            }

            // ── addressable event references → q tag with coordinate ──────
            Nip19::Coordinate(naddr) => {
                let coord = &naddr.coordinate;
                // Format: <kind>:<pubkey-hex>:<identifier>
                let coord_str = format!(
                    "{}:{}:{}",
                    coord.kind.as_u16(),
                    coord.public_key.to_hex(),
                    coord.identifier
                );
                let key = ("q".to_string(), coord_str.clone());
                if seen.insert(key) {
                    let mut parts = vec!["q".to_string(), coord_str];
                    if let Some(relay) = naddr.relays.first() {
                        parts.push(relay.to_string());
                    }
                    let Ok(tag) = Tag::parse(parts) else { continue };
                    tags.push(tag);
                }
            }

            // nsec / ncryptsec — ignore
            _ => {}
        }
    }

    Ok(tags)
}

/// Deduplicate a merged tag list, removing:
///
/// 1. Duplicate `p` tags — keep the first occurrence of each pubkey hex.
/// 2. Duplicate `q` tags — keep the first occurrence of each value.
/// 3. `q` tags whose event-id (position `[1]`) is already referenced by an
///    existing `e` tag — avoids redundant citations when the event is already
///    part of the threading structure.
///
/// All other tags are passed through unchanged and in order.
pub fn dedup_tags(tags: Vec<Tag>) -> Vec<Tag> {
    // First pass: collect the set of event IDs already covered by `e` tags.
    let e_ids: HashSet<String> = tags
        .iter()
        .filter(|t| t.as_slice().first().is_some_and(|k| k == "e"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();

    let mut seen_p: HashSet<String> = HashSet::new();
    let mut seen_q: HashSet<String> = HashSet::new();
    let mut out: Vec<Tag> = Vec::with_capacity(tags.len());

    for tag in tags {
        let slice = tag.as_slice();
        match slice.first().map(String::as_str) {
            Some("p") => {
                if let Some(pk) = slice.get(1) {
                    if seen_p.insert(pk.clone()) {
                        out.push(tag);
                    }
                    // else: duplicate p tag — drop it
                } else {
                    out.push(tag); // malformed, pass through
                }
            }
            Some("q") => {
                if let Some(val) = slice.get(1) {
                    // Suppress if already covered by an e tag (regular event refs only;
                    // coordinate strings contain ':' so they can never match a plain hex id).
                    if e_ids.contains(val) {
                        continue;
                    }
                    if seen_q.insert(val.clone()) {
                        out.push(tag);
                    }
                    // else: duplicate q tag — drop it
                } else {
                    out.push(tag);
                }
            }
            _ => out.push(tag),
        }
    }

    out
}

/// Attempt to find the pubkey of `event_id` in the local cache.
/// Returns `None` if the cache is unavailable or the event is not found.
async fn lookup_event_pubkey(
    event_id: &EventId,
    git_repo_path: Option<&Path>,
) -> Option<nostr_sdk::PublicKey> {
    let path = git_repo_path?;
    let filter = nostr::Filter::default().id(*event_id);
    let events = get_events_from_local_cache(path, vec![filter]).await.ok()?;
    events
        .into_iter()
        .find(|e| e.id == *event_id)
        .map(|e| e.pubkey)
}

/// Build a `["q", "<id-hex>", "<relay>", "<pubkey-hex>"]` tag.
/// Trailing optional elements are omitted when absent.
fn build_q_tag_for_event(
    event_id: EventId,
    relay: Option<nostr_sdk::RelayUrl>,
    pubkey: Option<nostr_sdk::PublicKey>,
) -> Result<Tag> {
    let mut parts = vec!["q".to_string(), event_id.to_hex()];
    match (relay, pubkey) {
        (Some(r), Some(pk)) => {
            parts.push(r.to_string());
            parts.push(pk.to_hex());
        }
        (Some(r), None) => {
            parts.push(r.to_string());
        }
        (None, Some(pk)) => {
            // relay is required before pubkey per the tag spec; use empty string
            parts.push(String::new());
            parts.push(pk.to_hex());
        }
        (None, None) => {}
    }
    Ok(Tag::parse(parts)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_mentions() {
        let tags = tags_from_content("hello world, no mentions here", None)
            .await
            .unwrap();
        assert!(tags.is_empty());
    }

    #[tokio::test]
    async fn test_npub_mention() {
        let content =
            "hello nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6 world";
        let tags = tags_from_content(content, None).await.unwrap();
        assert_eq!(tags.len(), 1);
        let slice = tags[0].as_slice();
        assert_eq!(slice[0], "p");
        // pubkey hex should be 64 chars
        assert_eq!(slice[1].len(), 64);
    }

    #[tokio::test]
    async fn test_note_mention() {
        // note1 encoding of all-zeros event id
        let content = "see nostr:note1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqn2l0z3";
        let tags = tags_from_content(content, None).await.unwrap();
        assert_eq!(tags.len(), 1);
        let slice = tags[0].as_slice();
        assert_eq!(slice[0], "q");
        assert_eq!(slice[1].len(), 64);
    }

    #[tokio::test]
    async fn test_naddr_mention() {
        // naddr for kind 30023 (long-form article)
        let content = "nostr:naddr1qqxnzdesxqmnxvpexqunzvpcqyt8wumn8ghj7un9d3shjtnwdaehgu3wvfskueqzypve7elhmamff3sr5mgxxms4a0rppkmhmn7504h96pfcdkpplvl2jqcyqqq823cnmhuld";
        let tags = tags_from_content(content, None).await.unwrap();
        assert_eq!(tags.len(), 1);
        let slice = tags[0].as_slice();
        assert_eq!(slice[0], "q");
        // format: <kind>:<pubkey-hex>:<identifier>
        let parts: Vec<&str> = slice[1].splitn(3, ':').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<u16>().is_ok(), "kind should be numeric");
        assert_eq!(parts[1].len(), 64, "pubkey should be 64 hex chars");
    }

    #[tokio::test]
    async fn test_deduplication() {
        let npub = "nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
        let content = format!("{npub} and again {npub}");
        let tags = tags_from_content(&content, None).await.unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[tokio::test]
    async fn test_mixed_mentions() {
        // note1 encoding of all-zeros event id
        let content = "nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6 and nostr:note1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqn2l0z3";
        let tags = tags_from_content(content, None).await.unwrap();
        assert_eq!(tags.len(), 2);
        let tag_names: Vec<&str> = tags.iter().map(|t| t.as_slice()[0].as_str()).collect();
        assert!(tag_names.contains(&"p"));
        assert!(tag_names.contains(&"q"));
    }

    // ── dedup_tags tests ──────────────────────────────────────────────────────

    #[test]
    fn dedup_removes_duplicate_p_tags() {
        let pk = "f7234bd4c1394dda46d09f35bd384dd30cc552ad5541990f98844fb06676e9ca";
        let tags = vec![
            Tag::parse(vec!["p".to_string(), pk.to_string()]).unwrap(),
            Tag::parse(vec!["p".to_string(), pk.to_string()]).unwrap(),
        ];
        let result = dedup_tags(tags);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].as_slice()[0], "p");
    }

    #[test]
    fn dedup_keeps_different_p_tags() {
        let pk1 = "f7234bd4c1394dda46d09f35bd384dd30cc552ad5541990f98844fb06676e9ca";
        let pk2 = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
        let tags = vec![
            Tag::parse(vec!["p".to_string(), pk1.to_string()]).unwrap(),
            Tag::parse(vec!["p".to_string(), pk2.to_string()]).unwrap(),
        ];
        let result = dedup_tags(tags);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedup_removes_q_tag_when_e_tag_has_same_id() {
        let id = "0000000000000000000000000000000000000000000000000000000000000000";
        let tags = vec![
            Tag::parse(vec!["e".to_string(), id.to_string()]).unwrap(),
            Tag::parse(vec!["q".to_string(), id.to_string()]).unwrap(),
        ];
        let result = dedup_tags(tags);
        // q tag should be suppressed; e tag kept
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].as_slice()[0], "e");
    }

    #[test]
    fn dedup_keeps_q_tag_for_coordinate_even_if_e_tag_present() {
        // coordinate strings contain ':' so they can never match a plain hex event id
        let coord =
            "30023:f7234bd4c1394dda46d09f35bd384dd30cc552ad5541990f98844fb06676e9ca:my-article";
        let event_id = "0000000000000000000000000000000000000000000000000000000000000000";
        let tags = vec![
            Tag::parse(vec!["e".to_string(), event_id.to_string()]).unwrap(),
            Tag::parse(vec!["q".to_string(), coord.to_string()]).unwrap(),
        ];
        let result = dedup_tags(tags);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedup_removes_duplicate_q_tags() {
        let id = "0000000000000000000000000000000000000000000000000000000000000000";
        let tags = vec![
            Tag::parse(vec!["q".to_string(), id.to_string()]).unwrap(),
            Tag::parse(vec!["q".to_string(), id.to_string()]).unwrap(),
        ];
        let result = dedup_tags(tags);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn dedup_passes_through_other_tags_unchanged() {
        let tags = vec![
            Tag::parse(vec!["subject".to_string(), "hello".to_string()]).unwrap(),
            Tag::parse(vec!["t".to_string(), "rust".to_string()]).unwrap(),
            Tag::parse(vec!["t".to_string(), "rust".to_string()]).unwrap(), /* hashtag dup — not
                                                                             * deduped */
        ];
        let result = dedup_tags(tags);
        // only p and q are deduped; other tags pass through as-is
        assert_eq!(result.len(), 3);
    }
}
