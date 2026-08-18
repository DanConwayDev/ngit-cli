//! Verified NIP-05 identities and the repository-domain ladder.
//!
//! A signer is operationally associated with the repository when a NIP-05
//! identity that resolves back to its own pubkey sits on a domain the
//! repository's clone URLs already name. Two candidate identities are
//! considered for every signer: the `nip05` in its kind-0 profile, and the
//! synthetic root identity `_@<grasp-domain>` for each GRASP domain the
//! resolved repository lists. The root candidate needs no kind-0 duplication —
//! the repository names the domain and the domain's NIP-05 document names its
//! root key.
//!
//! Domains are compared on DNS-label boundaries only. `runner.grasp.example`
//! is a subdomain of `grasp.example`; `grasp.example.evil.test` is not, and
//! neither is the sibling `notgrasp.example`. Bare string suffixes are never
//! used.
//!
//! A lookup that fails is *not* a failed verification. It leaves the signer's
//! coverage partial — the canonical
//! [`CONTEXT_INCOMPLETE_LABEL`](super::trust::CONTEXT_INCOMPLETE_LABEL) caveat
//! — and never produces a negative claim.

use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use nostr::prelude::{PublicKey, Timestamp};
use serde::{Deserialize, Serialize};

use super::trust::{EvidenceClassification, EvidenceScope, TrustEvidence, TrustEvidenceKind};
use crate::{
    get_dirs,
    repo_ref::{is_grasp_server_clone_url, normalize_grasp_server_url},
};

/// How a signer's verified domain relates to a repository-listed domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainRelationship {
    /// The same domain.
    Exact,
    /// A proper parent or child domain, on DNS-label boundaries.
    Subdomain,
}

/// A matched repository domain and how the identity relates to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainMatch {
    pub relationship: DomainRelationship,
    /// The normalized repository domain that matched.
    pub repository_domain: String,
}

/// Normalize a domain for comparison: trimmed, lowercased, any `:port`
/// suffix dropped, and one trailing root dot removed.
///
/// The port is dropped before the dot, unlike the TypeScript reference, which
/// strips the dot first and so leaves `grasp.example.:443` normalized as
/// `grasp.example.`. Both spellings name the same DNS host, so the reference
/// order only ever loses a true match.
#[must_use]
pub fn normalized_domain(value: &str) -> String {
    let lowered = value.trim().to_lowercase();
    let without_port = lowered.split(':').next().unwrap_or_default();
    without_port
        .strip_suffix('.')
        .unwrap_or(without_port)
        .to_owned()
}

/// Whether `candidate` is a proper DNS subdomain of `parent`.
///
/// The comparison requires the label boundary, so a shared suffix that does
/// not end on a `.` never matches.
#[must_use]
pub fn is_proper_subdomain(candidate: &str, parent: &str) -> bool {
    !parent.is_empty() && candidate != parent && candidate.ends_with(&format!(".{parent}"))
}

/// Classify an identity domain against the repository's domains.
///
/// An exact match beats a subdomain relationship, in either direction. Two
/// domains that merely share a parent are siblings and are not evidence.
#[must_use]
pub fn classify_domain_relationship(
    identity_domain: &str,
    repository_domains: &[String],
) -> Option<DomainMatch> {
    let identity = normalized_domain(identity_domain);
    if identity.is_empty() {
        return None;
    }
    for repository_domain in repository_domains {
        let repository_domain = normalized_domain(repository_domain);
        if identity == repository_domain && !repository_domain.is_empty() {
            return Some(DomainMatch {
                relationship: DomainRelationship::Exact,
                repository_domain,
            });
        }
    }
    for repository_domain in repository_domains {
        let repository_domain = normalized_domain(repository_domain);
        if is_proper_subdomain(&identity, &repository_domain)
            || is_proper_subdomain(&repository_domain, &identity)
        {
            return Some(DomainMatch {
                relationship: DomainRelationship::Subdomain,
                repository_domain,
            });
        }
    }
    None
}

/// The GRASP domains a repository's clone URLs name.
///
/// Grasp detection and URL normalization are
/// [`is_grasp_server_clone_url`] and [`normalize_grasp_server_url`]; only the
/// host (with any port) is kept, since a GRASP server mounted under a path
/// still lives at one domain.
#[must_use]
pub fn repository_grasp_domains(clone_urls: &[String]) -> Vec<String> {
    let mut domains: Vec<String> = Vec::new();
    for url in clone_urls {
        if !is_grasp_server_clone_url(url) {
            continue;
        }
        let Ok(normalized) = normalize_grasp_server_url(url) else {
            continue;
        };
        // `normalize_grasp_server_url` emits either a bare `host[:port][/path]`
        // or the same prefixed with `http://` for a plaintext server.
        let host = normalized
            .strip_prefix("http://")
            .unwrap_or(&normalized)
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        if !host.is_empty() && !domains.contains(&host) {
            domains.push(host);
        }
    }
    domains
}

/// A NIP-05 identifier split into its parts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Nip05Identity {
    /// The standardized `<local-part>@<domain>` address.
    pub nip05: String,
    pub local_part: String,
    pub domain: String,
}

impl Nip05Identity {
    /// How the identity is shown to people: a `_` local part displays as the
    /// bare domain, per NIP-05 root-identity semantics.
    #[must_use]
    pub fn display(&self) -> &str {
        if self.local_part == "_" {
            &self.domain
        } else {
            &self.nip05
        }
    }
}

/// Parse a NIP-05 value, standardizing a bare domain to its root identity.
#[must_use]
pub fn parse_nip05(value: &str) -> Option<Nip05Identity> {
    let trimmed = value.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let standardized = if trimmed.contains('@') {
        trimmed
    } else {
        format!("_@{trimmed}")
    };
    let separator = standardized.find('@')?;
    if separator == 0 || separator == standardized.len() - 1 {
        return None;
    }
    Some(Nip05Identity {
        local_part: standardized[..separator].to_owned(),
        domain: standardized[separator + 1..].to_owned(),
        nip05: standardized,
    })
}

/// The identities worth checking for one signer: its profile `nip05` plus the
/// root identity of every repository GRASP domain.
#[must_use]
pub fn identity_candidates(
    profile_nip05: Option<&str>,
    repository_domains: &[String],
) -> Vec<Nip05Identity> {
    let mut candidates: Vec<Nip05Identity> = Vec::new();
    for raw in profile_nip05.map(ToOwned::to_owned).into_iter().chain(
        repository_domains
            .iter()
            .map(|domain| format!("_@{domain}")),
    ) {
        let Some(identity) = parse_nip05(&raw) else {
            continue;
        };
        if !candidates
            .iter()
            .any(|existing| existing.nip05 == identity.nip05)
        {
            candidates.push(identity);
        }
    }
    candidates
}

/// A candidate identity after resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    pub identity: Nip05Identity,
    /// The document resolved the local part to this signer.
    pub verified: bool,
    /// The lookup did not settle — coverage is partial, and no negative claim
    /// may be made from it.
    pub failed: bool,
}

/// Build the Level 2 domain evidence for one signer.
///
/// Only verified identities contribute. Identical items — the same kind and
/// wording, e.g. two candidates resolving on the same domain — are collapsed.
#[must_use]
pub fn domain_evidence(
    identities: &[VerifiedIdentity],
    repository_domains: &[String],
) -> Vec<TrustEvidence> {
    let mut evidence: Vec<TrustEvidence> = Vec::new();
    for identity in identities.iter().filter(|identity| identity.verified) {
        let Some(matched) =
            classify_domain_relationship(&identity.identity.domain, repository_domains)
        else {
            continue;
        };
        let display = identity.identity.display();
        let item = match matched.relationship {
            DomainRelationship::Exact => TrustEvidence {
                kind: TrustEvidenceKind::RepositoryDomain,
                classification: EvidenceClassification::OperationallyAssociated,
                summary: "Uses repository-listed infrastructure".to_owned(),
                detail: format!(
                    "{display} resolves to this signer and is a GRASP domain listed by the resolved repository graph."
                ),
                authors: Vec::new(),
                scope: EvidenceScope::Current,
            },
            DomainRelationship::Subdomain => TrustEvidence {
                kind: TrustEvidenceKind::RepositorySubdomain,
                classification: EvidenceClassification::OperationallyAssociated,
                summary: "Related repository infrastructure".to_owned(),
                detail: format!(
                    "{display} is verified on a parent or child domain of repository-listed {}. Subdomains may have separate operators, so this is weaker evidence than an exact match.",
                    matched.repository_domain
                ),
                authors: Vec::new(),
                scope: EvidenceScope::Current,
            },
        };
        if !evidence
            .iter()
            .any(|existing| existing.kind == item.kind && existing.detail == item.detail)
        {
            evidence.push(item);
        }
    }
    evidence
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolves a NIP-05 address to the pubkey its document names.
///
/// Implemented over the network by [`NetworkNip05Lookup`]; unit tests inject
/// their own resolutions instead.
#[async_trait]
pub trait Nip05Lookup {
    /// Resolve `address`.
    ///
    /// # Errors
    ///
    /// Any error means the lookup did not settle. It is never a statement
    /// that the identity is invalid.
    async fn lookup(&self, address: &str) -> Result<PublicKey>;
}

/// How long one NIP-05 lookup may take before it is abandoned.
///
/// The shared fetch path has no deadline of its own, so a `.well-known` host
/// that accepts a connection and never answers would stall the command. The
/// trust model requires a *bounded* resolution failure to settle rather than
/// wait, so an elapsed lookup becomes a failed one — partial coverage, no
/// negative claim. Five seconds matches gitworkshop's `IDENTITY_TIMEOUT_MS`.
pub const NIP05_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The real lookup: ngit's shared NIP-05 fetch path, the same one that
/// resolves `nostr://` URLs, bounded by [`NIP05_LOOKUP_TIMEOUT`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkNip05Lookup;

#[async_trait]
impl Nip05Lookup for NetworkNip05Lookup {
    async fn lookup(&self, address: &str) -> Result<PublicKey> {
        Ok(
            tokio::time::timeout(NIP05_LOOKUP_TIMEOUT, crate::client::nip05_query(address))
                .await
                .with_context(|| {
                    format!(
                        "nip05 lookup for {address} did not answer within {}s",
                        NIP05_LOOKUP_TIMEOUT.as_secs()
                    )
                })??
                .public_key,
        )
    }
}

/// A settled lookup, as stored in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedLookup {
    /// The document named this pubkey for the local part.
    Resolved(PublicKey),
    /// The lookup did not settle. Cached briefly so a broken domain does not
    /// stall every command, and re-tried sooner than a success.
    Failed,
}

/// How long a successful NIP-05 lookup is reused.
pub const NIP05_SUCCESS_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a failed NIP-05 lookup is reused. Shorter than a success: a
/// failure is usually transient and must not suppress evidence for long.
pub const NIP05_FAILURE_TTL: Duration = Duration::from_secs(5 * 60);

/// Directory name under ngit's cache directory.
const NIP05_CACHE_DIR: &str = "ci-nip05";

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    address: String,
    /// `None` records a lookup that did not settle.
    pubkey: Option<String>,
    fetched_at: u64,
}

/// A TTL cache of NIP-05 lookups in ngit's local cache directory.
///
/// One file per address. A missing, unreadable, malformed or expired file is
/// treated as absent, so a corrupt cache costs a lookup and never a wrong
/// answer.
#[derive(Debug, Clone)]
pub struct Nip05Cache {
    dir: PathBuf,
    success_ttl: Duration,
    failure_ttl: Duration,
}

impl Nip05Cache {
    /// A cache rooted at `dir`, with the default TTLs.
    #[must_use]
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            success_ttl: NIP05_SUCCESS_TTL,
            failure_ttl: NIP05_FAILURE_TTL,
        }
    }

    /// The cache in ngit's own cache directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform cache directory cannot be located.
    pub fn discover() -> Result<Self> {
        Ok(Self::in_dir(get_dirs()?.cache_dir().join(NIP05_CACHE_DIR)))
    }

    /// Override the TTLs, e.g. to pin them in a test.
    #[must_use]
    pub fn with_ttls(mut self, success: Duration, failure: Duration) -> Self {
        self.success_ttl = success;
        self.failure_ttl = failure;
        self
    }

    /// The entry for `address`, when one is present and still fresh.
    #[must_use]
    pub fn get(&self, address: &str, now: Timestamp) -> Option<CachedLookup> {
        let raw = fs::read_to_string(self.path(address)).ok()?;
        let entry: CacheEntry = serde_json::from_str(&raw).ok()?;
        if entry.address != address {
            return None;
        }
        let lookup = match &entry.pubkey {
            Some(hex) => CachedLookup::Resolved(PublicKey::parse(hex).ok()?),
            None => CachedLookup::Failed,
        };
        let ttl = match lookup {
            CachedLookup::Resolved(_) => self.success_ttl,
            CachedLookup::Failed => self.failure_ttl,
        };
        // A future `fetched_at` is a clock change, not freshness.
        let age = now.as_secs().checked_sub(entry.fetched_at)?;
        (age <= ttl.as_secs()).then_some(lookup)
    }

    /// Record a settled lookup.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry cannot be written. Callers treat a
    /// write failure as a missed cache, not a failed lookup.
    pub fn put(&self, address: &str, lookup: CachedLookup, now: Timestamp) -> Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| {
            format!(
                "failed to create nip05 cache directory {}",
                self.dir.display()
            )
        })?;
        let entry = CacheEntry {
            address: address.to_owned(),
            pubkey: match lookup {
                CachedLookup::Resolved(pubkey) => Some(pubkey.to_hex()),
                CachedLookup::Failed => None,
            },
            fetched_at: now.as_secs(),
        };
        let path = self.path(address);
        fs::write(
            &path,
            serde_json::to_string(&entry).context("failed to serialize nip05 cache entry")?,
        )
        .with_context(|| format!("failed to write nip05 cache entry {}", path.display()))
    }

    fn path(&self, address: &str) -> PathBuf {
        self.dir.join(format!("{}.json", hex_encode(address)))
    }
}

/// Hex-encode the address for a filename that cannot escape the cache
/// directory or collide with another address.
fn hex_encode(value: &str) -> String {
    use std::fmt::Write as _;
    value
        .as_bytes()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Resolve one candidate identity for `signer`.
///
/// Verification is exact: the document must map the local part to the
/// signer's own pubkey. A document naming somebody else is a settled
/// non-match, not a failure.
pub async fn verify_identity<L: Nip05Lookup + ?Sized + Sync>(
    lookup: &L,
    cache: Option<&Nip05Cache>,
    identity: Nip05Identity,
    signer: PublicKey,
    now: Timestamp,
) -> VerifiedIdentity {
    let cached = cache.and_then(|cache| cache.get(&identity.nip05, now));
    let settled = match cached {
        Some(settled) => settled,
        None => {
            let settled = match lookup.lookup(&identity.nip05).await {
                Ok(pubkey) => CachedLookup::Resolved(pubkey),
                Err(_) => CachedLookup::Failed,
            };
            if let Some(cache) = cache {
                // A cache we cannot write is only a missed optimization.
                let _ = cache.put(&identity.nip05, settled, now);
            }
            settled
        }
    };
    match settled {
        CachedLookup::Resolved(pubkey) => VerifiedIdentity {
            identity,
            verified: pubkey == signer,
            failed: false,
        },
        CachedLookup::Failed => VerifiedIdentity {
            identity,
            verified: false,
            failed: true,
        },
    }
}

/// Resolve every candidate identity for every signer.
///
/// `profile_nip05` holds the `nip05` value from each signer's kind-0 profile,
/// where one is known. Candidates are resolved one at a time; with each
/// lookup bounded by [`NIP05_LOOKUP_TIMEOUT`] and successes cached, the total
/// wait is bounded by the number of distinct candidates.
pub async fn verify_identities<L: Nip05Lookup + ?Sized + Sync>(
    lookup: &L,
    cache: Option<&Nip05Cache>,
    signers: &[PublicKey],
    profile_nip05: &HashMap<PublicKey, String>,
    repository_domains: &[String],
    now: Timestamp,
) -> HashMap<PublicKey, Vec<VerifiedIdentity>> {
    let mut verified: HashMap<PublicKey, Vec<VerifiedIdentity>> = HashMap::new();
    for signer in signers {
        let candidates = identity_candidates(
            profile_nip05.get(signer).map(String::as_str),
            repository_domains,
        );
        let mut identities = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            identities.push(verify_identity(lookup, cache, candidate, *signer, now).await);
        }
        verified.insert(*signer, identities);
    }
    verified
}

/// Whether any resolution left coverage incomplete.
#[must_use]
pub fn any_lookup_failed(identities: &HashMap<PublicKey, Vec<VerifiedIdentity>>) -> bool {
    identities
        .values()
        .flatten()
        .any(|identity| identity.failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use nostr::prelude::{Keys, ToBech32};
    use tempfile::TempDir;

    use super::*;

    /// A lookup backed by a fixed table; every other address fails, which is
    /// how a test injects an unsettled lookup.
    #[derive(Debug, Default)]
    pub(crate) struct StubNip05Lookup {
        pub(crate) resolutions: HashMap<String, PublicKey>,
    }

    impl StubNip05Lookup {
        pub(crate) fn resolving(address: &str, pubkey: PublicKey) -> Self {
            Self {
                resolutions: [(address.to_owned(), pubkey)].into_iter().collect(),
            }
        }
    }

    #[async_trait]
    impl Nip05Lookup for StubNip05Lookup {
        async fn lookup(&self, address: &str) -> Result<PublicKey> {
            self.resolutions
                .get(address)
                .copied()
                .with_context(|| format!("no stubbed resolution for {address}"))
        }
    }

    /// Counts lookups so a test can prove the cache was used.
    #[derive(Debug, Default)]
    struct CountingLookup {
        inner: StubNip05Lookup,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Nip05Lookup for CountingLookup {
        async fn lookup(&self, address: &str) -> Result<PublicKey> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.lookup(address).await
        }
    }

    fn identity(nip05: &str) -> Nip05Identity {
        parse_nip05(nip05).expect("a valid nip05")
    }

    fn verified(nip05: &str) -> VerifiedIdentity {
        VerifiedIdentity {
            identity: identity(nip05),
            verified: true,
            failed: false,
        }
    }

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_secs(secs)
    }

    #[test]
    fn an_exact_domain_matches_regardless_of_case_trailing_dot_and_port() {
        let repository = vec!["grasp.example".to_owned()];
        for candidate in [
            "grasp.example",
            "GRASP.Example",
            "  grasp.example  ",
            "grasp.example.",
            "grasp.example:8080",
        ] {
            assert_eq!(
                classify_domain_relationship(candidate, &repository),
                Some(DomainMatch {
                    relationship: DomainRelationship::Exact,
                    repository_domain: "grasp.example".to_owned(),
                }),
                "{candidate} should match exactly"
            );
        }
        // The repository domain is normalized the same way.
        assert_eq!(
            classify_domain_relationship("grasp.example", &["GRASP.example.:443".to_owned()])
                .map(|matched| matched.relationship),
            Some(DomainRelationship::Exact)
        );
    }

    #[test]
    fn subdomains_match_only_on_label_boundaries() {
        let repository = vec!["grasp.example".to_owned()];
        assert_eq!(
            classify_domain_relationship("runner.grasp.example", &repository),
            Some(DomainMatch {
                relationship: DomainRelationship::Subdomain,
                repository_domain: "grasp.example".to_owned(),
            })
        );
        // A suffix that does not end on a label boundary is not a subdomain.
        assert_eq!(
            classify_domain_relationship("grasp.example.evil.test", &repository),
            None
        );
        assert_eq!(
            classify_domain_relationship("notgrasp.example", &repository),
            None
        );
    }

    #[test]
    fn the_parent_direction_also_matches() {
        // The repository lists a subdomain; the signer is verified on its
        // parent.
        let repository = vec!["runner.grasp.example".to_owned()];
        assert_eq!(
            classify_domain_relationship("grasp.example", &repository),
            Some(DomainMatch {
                relationship: DomainRelationship::Subdomain,
                repository_domain: "runner.grasp.example".to_owned(),
            })
        );
    }

    #[test]
    fn siblings_are_not_evidence() {
        assert_eq!(
            classify_domain_relationship("a.grasp.example", &["b.grasp.example".to_owned()]),
            None
        );
    }

    #[test]
    fn an_exact_match_beats_a_subdomain_of_another_listed_domain() {
        let repository = vec![
            "runner.grasp.example".to_owned(),
            "grasp.example".to_owned(),
        ];
        assert_eq!(
            classify_domain_relationship("grasp.example", &repository),
            Some(DomainMatch {
                relationship: DomainRelationship::Exact,
                repository_domain: "grasp.example".to_owned(),
            })
        );
    }

    #[test]
    fn empty_domains_never_match() {
        assert_eq!(classify_domain_relationship("", &["".to_owned()]), None);
        assert_eq!(
            classify_domain_relationship("grasp.example", &[String::new()]),
            None
        );
        assert!(!is_proper_subdomain(".", ""));
    }

    #[test]
    fn a_bare_domain_parses_as_the_root_identity_and_displays_as_the_domain() {
        let root = identity("Grasp.Example");
        assert_eq!(root.nip05, "_@grasp.example");
        assert_eq!(root.local_part, "_");
        assert_eq!(root.display(), "grasp.example");

        let named = identity("act-1@grasp.example");
        assert_eq!(named.display(), "act-1@grasp.example");

        assert_eq!(parse_nip05(""), None);
        assert_eq!(parse_nip05("@grasp.example"), None);
        assert_eq!(parse_nip05("act-1@"), None);
    }

    #[test]
    fn candidates_are_the_profile_identity_plus_every_grasp_root() {
        let candidates = identity_candidates(
            Some("act-1@grasp.example"),
            &["grasp.example".to_owned(), "other.example".to_owned()],
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.nip05.as_str())
                .collect::<Vec<_>>(),
            vec!["act-1@grasp.example", "_@grasp.example", "_@other.example"]
        );

        // A profile that already names the root is not checked twice.
        let deduplicated =
            identity_candidates(Some("_@grasp.example"), &["grasp.example".to_owned()]);
        assert_eq!(deduplicated.len(), 1);

        // A signer with no profile still gets the repository's roots.
        assert_eq!(
            identity_candidates(None, &["grasp.example".to_owned()]).len(),
            1
        );
    }

    #[test]
    fn grasp_domains_come_from_grasp_clone_urls_only() {
        let npub = Keys::generate().public_key().to_bech32().unwrap();
        let domains = repository_grasp_domains(&[
            format!("https://grasp.example/{npub}/ngit.git"),
            // Same domain again: deduplicated.
            format!("https://grasp.example/{npub}/other.git"),
            format!("http://localhost:8080/{npub}/ngit.git"),
            // Not a GRASP clone URL.
            "https://github.com/example/ngit.git".to_owned(),
        ]);
        assert_eq!(domains, vec!["grasp.example", "localhost:8080"]);
    }

    #[test]
    fn domain_evidence_uses_the_canonical_wording() {
        let repository = vec!["grasp.example".to_owned()];
        let exact = domain_evidence(&[verified("_@grasp.example")], &repository);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].kind, TrustEvidenceKind::RepositoryDomain);
        assert_eq!(
            exact[0].classification,
            EvidenceClassification::OperationallyAssociated
        );
        assert_eq!(exact[0].scope, EvidenceScope::Current);
        assert_eq!(exact[0].summary, "Uses repository-listed infrastructure");
        assert_eq!(
            exact[0].detail,
            "grasp.example resolves to this signer and is a GRASP domain listed by the resolved repository graph."
        );

        let named = domain_evidence(&[verified("act-1@grasp.example")], &repository);
        assert_eq!(
            named[0].detail,
            "act-1@grasp.example resolves to this signer and is a GRASP domain listed by the resolved repository graph."
        );

        let subdomain = domain_evidence(&[verified("_@runner.grasp.example")], &repository);
        assert_eq!(subdomain[0].kind, TrustEvidenceKind::RepositorySubdomain);
        assert_eq!(subdomain[0].summary, "Related repository infrastructure");
        assert_eq!(
            subdomain[0].detail,
            "runner.grasp.example is verified on a parent or child domain of repository-listed grasp.example. Subdomains may have separate operators, so this is weaker evidence than an exact match."
        );
    }

    #[test]
    fn only_verified_identities_contribute_and_duplicates_collapse() {
        let repository = vec!["grasp.example".to_owned()];
        let unverified = VerifiedIdentity {
            identity: identity("_@grasp.example"),
            verified: false,
            failed: true,
        };
        assert!(domain_evidence(&[unverified], &repository).is_empty());

        // Identical items are reported once, however they arrived.
        let evidence = domain_evidence(
            &[verified("_@grasp.example"), verified("_@grasp.example")],
            &repository,
        );
        assert_eq!(evidence.len(), 1);

        // Different identities on the same domain are distinct evidence.
        let distinct = domain_evidence(
            &[verified("_@grasp.example"), verified("act-1@grasp.example")],
            &repository,
        );
        assert_eq!(distinct.len(), 2);
    }

    #[test]
    fn a_signer_on_an_unlisted_domain_has_no_domain_evidence() {
        assert!(
            domain_evidence(
                &[verified("act-1@unrelated.example")],
                &["grasp.example".to_owned()]
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn verification_requires_the_document_to_name_the_signer() {
        let signer = Keys::generate();
        let other = Keys::generate();
        let lookup = StubNip05Lookup::resolving("_@grasp.example", signer.public_key());

        let matching = verify_identity(
            &lookup,
            None,
            identity("_@grasp.example"),
            signer.public_key(),
            ts(100),
        )
        .await;
        assert!(matching.verified && !matching.failed);

        let mismatched = verify_identity(
            &lookup,
            None,
            identity("_@grasp.example"),
            other.public_key(),
            ts(100),
        )
        .await;
        assert!(
            !mismatched.verified && !mismatched.failed,
            "a document naming somebody else is a settled non-match"
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_is_not_a_failed_verification() {
        let signer = Keys::generate();
        let lookup = StubNip05Lookup::default();
        let resolved = verify_identity(
            &lookup,
            None,
            identity("_@grasp.example"),
            signer.public_key(),
            ts(100),
        )
        .await;
        assert!(!resolved.verified);
        assert!(resolved.failed);
        assert!(any_lookup_failed(
            &[(signer.public_key(), vec![resolved])]
                .into_iter()
                .collect()
        ));
    }

    #[tokio::test]
    async fn a_cached_lookup_is_reused_until_it_expires() {
        let dir = TempDir::new().unwrap();
        let signer = Keys::generate();
        let cache = Nip05Cache::in_dir(dir.path());
        let lookup = CountingLookup {
            inner: StubNip05Lookup::resolving("_@grasp.example", signer.public_key()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };

        for _ in 0..3 {
            let resolved = verify_identity(
                &lookup,
                Some(&cache),
                identity("_@grasp.example"),
                signer.public_key(),
                ts(100),
            )
            .await;
            assert!(resolved.verified);
        }
        assert_eq!(lookup.calls.load(std::sync::atomic::Ordering::Relaxed), 1);

        // Past the success TTL the entry is absent again.
        let refreshed = verify_identity(
            &lookup,
            Some(&cache),
            identity("_@grasp.example"),
            signer.public_key(),
            ts(100 + NIP05_SUCCESS_TTL.as_secs() + 1),
        )
        .await;
        assert!(refreshed.verified);
        assert_eq!(lookup.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_cached_failure_expires_sooner_than_a_success() {
        let dir = TempDir::new().unwrap();
        let signer = Keys::generate();
        let cache = Nip05Cache::in_dir(dir.path());
        cache
            .put("_@grasp.example", CachedLookup::Failed, ts(100))
            .unwrap();

        assert_eq!(
            cache.get("_@grasp.example", ts(100 + NIP05_FAILURE_TTL.as_secs())),
            Some(CachedLookup::Failed)
        );
        assert_eq!(
            cache.get("_@grasp.example", ts(100 + NIP05_FAILURE_TTL.as_secs() + 1)),
            None,
            "a failure is re-tried well before a success would be"
        );
        assert!(NIP05_FAILURE_TTL < NIP05_SUCCESS_TTL);

        // An expired failure does not suppress a later successful lookup.
        let lookup = StubNip05Lookup::resolving("_@grasp.example", signer.public_key());
        let resolved = verify_identity(
            &lookup,
            Some(&cache),
            identity("_@grasp.example"),
            signer.public_key(),
            ts(100 + NIP05_FAILURE_TTL.as_secs() + 1),
        )
        .await;
        assert!(resolved.verified);
    }

    #[test]
    fn a_corrupt_or_foreign_cache_entry_is_treated_as_absent() {
        let dir = TempDir::new().unwrap();
        let signer = Keys::generate();
        let cache = Nip05Cache::in_dir(dir.path());
        cache
            .put(
                "_@grasp.example",
                CachedLookup::Resolved(signer.public_key()),
                ts(100),
            )
            .unwrap();
        let path = dir
            .path()
            .join(format!("{}.json", hex_encode("_@grasp.example")));

        fs::write(&path, "{not json").unwrap();
        assert_eq!(cache.get("_@grasp.example", ts(100)), None);

        // An entry recording a different address is not this address's.
        fs::write(
            &path,
            serde_json::to_string(&CacheEntry {
                address: "_@other.example".to_owned(),
                pubkey: Some(signer.public_key().to_hex()),
                fetched_at: 100,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cache.get("_@grasp.example", ts(100)), None);

        // So is an entry with an unparsable pubkey.
        fs::write(
            &path,
            serde_json::to_string(&CacheEntry {
                address: "_@grasp.example".to_owned(),
                pubkey: Some("not-a-key".to_owned()),
                fetched_at: 100,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cache.get("_@grasp.example", ts(100)), None);

        // And so is one stamped in the future by a clock change.
        cache
            .put(
                "_@grasp.example",
                CachedLookup::Resolved(signer.public_key()),
                ts(500),
            )
            .unwrap();
        assert_eq!(cache.get("_@grasp.example", ts(100)), None);
    }

    #[test]
    fn cache_entries_never_escape_the_cache_directory() {
        let dir = TempDir::new().unwrap();
        let cache = Nip05Cache::in_dir(dir.path());
        let signer = Keys::generate();
        cache
            .put(
                "_@../../etc/passwd",
                CachedLookup::Resolved(signer.public_key()),
                ts(100),
            )
            .unwrap();
        let entries: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].parent(), Some(dir.path()));
    }

    #[tokio::test]
    async fn every_signer_gets_every_candidate_resolved() {
        let coordinator = Keys::generate();
        let provider = Keys::generate();
        let mut resolutions = HashMap::new();
        resolutions.insert("_@grasp.example".to_owned(), coordinator.public_key());
        resolutions.insert("act-1@grasp.example".to_owned(), provider.public_key());
        let lookup = StubNip05Lookup { resolutions };
        let profiles: HashMap<PublicKey, String> =
            [(provider.public_key(), "act-1@grasp.example".to_owned())]
                .into_iter()
                .collect();

        let identities = verify_identities(
            &lookup,
            None,
            &[coordinator.public_key(), provider.public_key()],
            &profiles,
            &["grasp.example".to_owned()],
            ts(100),
        )
        .await;
        assert!(!any_lookup_failed(&identities));
        assert!(identities[&coordinator.public_key()][0].verified);
        assert!(identities[&provider.public_key()][0].verified);
        // The provider is not the root operator, so the root candidate is a
        // settled non-match rather than evidence.
        assert!(!identities[&provider.public_key()][1].verified);
    }
}
