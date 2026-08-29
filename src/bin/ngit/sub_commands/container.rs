use std::{collections::HashSet, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use futures::future::join_all;
use ngit::{
    NgitSigner,
    blossom::{
        LocalFileRequest, MultiServerUpload, blossom_server_list_filter,
        blossom_server_list_from_events, canonicalize_blossom_server_root, snapshot_local_file,
        upload_snapshot_to_servers,
    },
    client::{Connect, Params, RelayProgressReporter, send_events, sign_draft_event},
    event_ordering::{finalize_ordered_unsigned, latest_event},
    git::Repo,
    login,
    oci::{
        CONTAINER_REPOSITORY_KIND, ContainerRepository, OciBlob, OciLayout,
        is_valid_repository_name,
    },
};
use nostr::prelude::{
    Coordinate, Event, Filter, PublicKey, RelayUrl, ToBech32, Url, nip19::Nip19Coordinate,
};
use serde_json::{Value, json};

use crate::{
    cli::{ContainerPublishArgs, SignerParams},
    client::Client,
};

pub async fn publish(
    args: &ContainerPublishArgs,
    signer_params: SignerParams<'_>,
    json_output: bool,
) -> Result<()> {
    validate_metadata_args(args)?;
    let explicit_servers = parse_blossom_servers(&args.blossom_servers)?;
    let source = args.source.as_deref().map(parse_source_url).transpose()?;
    let layout = OciLayout::load(&args.layout).with_context(|| {
        format!(
            "failed to validate OCI image layout {}",
            args.layout.display()
        )
    })?;

    let git_repo = Repo::discover().ok();
    let git_repo_ref = git_repo.as_ref();
    let mut client = Client::new(Params::with_git_config_relay_defaults(&git_repo_ref));
    let (signer, user, _) = login::login_or_signup(
        &git_repo_ref,
        signer_params.info,
        signer_params.password,
        Some(&client),
        false,
    )
    .await?;
    client.set_signer(Arc::clone(&signer)).await;

    let public_key = signer
        .get_public_key()
        .await
        .context("failed to read the container publisher public key")?;
    ensure!(
        public_key == user.public_key,
        "active signer does not match the loaded Nostr account"
    );
    let publication_relays = publication_relays(&client, user.relays.write(), &args.relays)?;
    client.nip42_register_publish_relays(publication_relays.clone());
    let servers = if explicit_servers.is_empty() {
        discover_blossom_servers(&client, &publication_relays, public_key).await?
    } else {
        explicit_servers
    };
    let existing =
        fetch_current_repository(&client, &publication_relays, public_key, &args.repository)
            .await?;
    let repository = merged_repository(args, &layout, &servers, source, existing.as_ref())?;
    let event_builder = repository
        .event_builder()
        .context("failed to construct the container repository event")?;

    let uploads = upload_layout(&layout, &servers, &signer, json_output).await?;

    let rechecked =
        fetch_current_repository(&client, &publication_relays, public_key, &args.repository)
            .await
            .context("failed to re-check the container repository after uploading blobs")?;
    ensure_unchanged(existing.as_ref(), rechecked.as_ref())?;

    let unsigned = finalize_ordered_unsigned(event_builder, public_key, existing.as_ref())
        .context("failed to order the container repository update after uploading blobs")?;
    let event = sign_draft_event(unsigned, &signer, "container repository".to_owned())
        .await
        .context("failed to sign the container repository event; uploaded blobs are reusable")?;
    let relay_results = send_events(
        &client,
        None,
        vec![event.clone()],
        publication_relays.iter().map(ToString::to_string).collect(),
        vec![],
        !json_output,
        json_output,
    )
    .await
    .context("failed to publish the container repository event; uploaded blobs are reusable")?;
    ensure!(
        relay_results.iter().any(|(_, accepted)| *accepted),
        "no relay accepted the container repository event; uploaded blobs are reusable"
    );

    render_success(
        &PublishSuccess {
            args,
            public_key,
            publication_relays: &publication_relays,
            event: &event,
            repository: &repository,
            layout: &layout,
            uploads: &uploads,
            relay_results: &relay_results,
        },
        json_output,
    )?;

    client.disconnect().await?;
    Ok(())
}

struct PublishSuccess<'a> {
    args: &'a ContainerPublishArgs,
    public_key: PublicKey,
    publication_relays: &'a [RelayUrl],
    event: &'a Event,
    repository: &'a ContainerRepository,
    layout: &'a OciLayout,
    uploads: &'a [(OciBlob, MultiServerUpload)],
    relay_results: &'a [(String, bool)],
}

fn render_success(success: &PublishSuccess<'_>, json_output: bool) -> Result<()> {
    let naddr = Nip19Coordinate {
        coordinate: Coordinate::new(CONTAINER_REPOSITORY_KIND, success.public_key)
            .identifier(success.args.repository.clone()),
        relays: success.publication_relays.to_vec(),
    }
    .to_bech32()?;
    let npub = success.public_key.to_bech32()?;

    if json_output {
        crate::output::set_value(json!({
            "format_version": 1,
            "ok": true,
            "command": "container.publish",
            "result": {
                "repository": success.args.repository,
                "npub": npub,
                "name": format!("{npub}/{}", success.args.repository),
                "naddr": naddr,
                "event_id": success.event.id.to_hex(),
                "tags": success.repository.tags,
                "updated_tags": success.layout.tags,
                "blobs": success.uploads.iter().map(upload_json).collect::<Vec<_>>(),
                "blossom_servers": success.repository.servers,
                "relays": success.relay_results.iter().map(|(url, accepted)| json!({
                    "url": url,
                    "accepted": accepted,
                })).collect::<Vec<_>>(),
            }
        }));
    } else {
        println!(
            "published container repository {npub}/{}",
            success.args.repository
        );
        for tag in &success.layout.tags {
            println!("  {} -> sha256:{}", tag.name, tag.digest);
        }
        println!("  event: {naddr}");
    }
    Ok(())
}

fn validate_metadata_args(args: &ContainerPublishArgs) -> Result<()> {
    ensure!(
        is_valid_repository_name(&args.repository),
        "container repository name {:?} is invalid; use lowercase letters, digits, and . _ - separators",
        args.repository
    );
    if let Some(title) = args.title.as_deref() {
        ensure!(!title.is_empty(), "--title must not be empty");
    }
    if let Some(description) = args.description.as_deref() {
        ensure!(!description.is_empty(), "--description must not be empty");
    }
    Ok(())
}

fn merged_repository(
    args: &ContainerPublishArgs,
    layout: &OciLayout,
    uploaded_servers: &[Url],
    source: Option<Url>,
    existing: Option<&Event>,
) -> Result<ContainerRepository> {
    let previous = existing
        .map(ContainerRepository::from_event)
        .transpose()
        .context("failed to parse the current container repository event")?;
    if args.replace {
        return Ok(new_repository(args, layout, uploaded_servers, source));
    }
    let Some(previous) = previous else {
        return Ok(new_repository(args, layout, uploaded_servers, source));
    };
    let mut tags = previous.tags;
    for new_tag in &layout.tags {
        if let Some(existing) = tags.iter_mut().find(|entry| entry.name == new_tag.name) {
            *existing = new_tag.clone();
        } else {
            tags.push(new_tag.clone());
        }
    }
    let mut servers = previous.servers;
    let mut server_keys = servers
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    for server in uploaded_servers {
        if server_keys.insert(server.to_string()) {
            servers.push(server.clone());
        }
    }
    Ok(ContainerRepository {
        name: args.repository.clone(),
        tags,
        servers,
        title: args
            .title
            .clone()
            .or(previous.title)
            .or_else(|| Some(args.repository.clone())),
        description: args.description.clone().or(previous.description),
        source: source.or(previous.source),
        extra_tags: previous.extra_tags,
    })
}

fn new_repository(
    args: &ContainerPublishArgs,
    layout: &OciLayout,
    servers: &[Url],
    source: Option<Url>,
) -> ContainerRepository {
    ContainerRepository {
        name: args.repository.clone(),
        tags: layout.tags.clone(),
        servers: servers.to_vec(),
        title: Some(
            args.title
                .clone()
                .unwrap_or_else(|| args.repository.clone()),
        ),
        description: args.description.clone(),
        source,
        extra_tags: vec![],
    }
}

fn parse_blossom_servers(values: &[String]) -> Result<Vec<Url>> {
    let mut seen = HashSet::new();
    let mut servers = Vec::with_capacity(values.len());
    for value in values {
        let server = canonicalize_blossom_server_root(value)
            .with_context(|| format!("invalid --blossom-server {value:?}"))?;
        if seen.insert(server.to_string()) {
            servers.push(server);
        }
    }
    Ok(servers)
}

async fn discover_blossom_servers(
    client: &Client,
    relays: &[RelayUrl],
    author: PublicKey,
) -> Result<Vec<Url>> {
    let progress = RelayProgressReporter::hidden();
    let filters = vec![blossom_server_list_filter(author)];
    let progress_handle = progress.handle();
    let results = join_all(relays.iter().cloned().map(|relay| {
        let filters = filters.clone();
        let progress = progress_handle.clone();
        async move {
            let result = async {
                let mut relay_results = client
                    .get_events_per_relay(vec![relay.clone()], filters, progress)
                    .await
                    .with_context(|| format!("failed to query Blossom relay {relay}"))?;
                ensure!(
                    relay_results.len() == 1,
                    "relay {relay} did not produce exactly one Blossom query result (got {})",
                    relay_results.len()
                );
                relay_results
                    .pop()
                    .context("Blossom relay result disappeared after its length was checked")?
                    .with_context(|| format!("failed to fetch Blossom events from {relay}"))
            }
            .await;
            (relay, result)
        }
    }))
    .await;
    progress.finish(
        results.iter().any(|(_, result)| result.is_err()),
        results.iter().all(|(_, result)| result.is_err()),
        None,
    )?;

    let mut events = Vec::new();
    let mut failed = Vec::new();
    for (relay, result) in results {
        match result {
            Ok(mut relay_events) => events.append(&mut relay_events),
            Err(error) => failed.push(format!("{relay}: {error:#}")),
        }
    }
    ensure!(
        failed.len() < relays.len(),
        "Blossom server discovery did not complete on any relay: {}; provide --blossom-server or retry",
        failed.join("; ")
    );
    if !failed.is_empty() {
        eprintln!(
            "warning: Blossom server discovery was incomplete on: {}",
            failed.join("; ")
        );
    }
    blossom_server_list_from_events(author, &events)
        .map(|list| list.servers)
        .context("failed to discover the publisher's Blossom servers; provide --blossom-server to override discovery")
}

fn parse_source_url(value: &str) -> Result<Url> {
    let source = Url::parse(value).with_context(|| format!("invalid --source URL {value:?}"))?;
    ensure!(
        matches!(source.scheme(), "http" | "https") && source.host().is_some(),
        "--source must be an absolute HTTP or HTTPS URL"
    );
    ensure!(
        source.username().is_empty() && source.password().is_none(),
        "--source must not contain embedded credentials"
    );
    Ok(source)
}

fn publication_relays(
    client: &Client,
    user_write_relays: Vec<String>,
    explicit: &[String],
) -> Result<Vec<RelayUrl>> {
    let candidates = if user_write_relays.is_empty() && explicit.is_empty() {
        client.get_relay_default_set().clone()
    } else {
        [user_write_relays, explicit.to_vec()].concat()
    };
    let mut seen = HashSet::new();
    let mut relays = Vec::new();
    for value in candidates {
        let relay = RelayUrl::parse(&value)
            .with_context(|| format!("invalid container publication relay {value:?}"))?;
        let key = relay.to_string().trim_end_matches('/').to_owned();
        if seen.insert(key) {
            relays.push(relay);
        }
    }
    ensure!(
        !relays.is_empty(),
        "container publication requires at least one relay"
    );
    Ok(relays)
}

async fn fetch_current_repository(
    client: &Client,
    relays: &[RelayUrl],
    author: PublicKey,
    repository: &str,
) -> Result<Option<Event>> {
    let progress = RelayProgressReporter::hidden();
    let filters = vec![
        Filter::new()
            .author(author)
            .kind(CONTAINER_REPOSITORY_KIND)
            .identifier(repository),
    ];
    let progress_handle = progress.handle();
    let results = join_all(relays.iter().cloned().map(|relay| {
        let filters = filters.clone();
        let progress = progress_handle.clone();
        async move {
            let result = async {
                let mut relay_results = client
                    .get_events_per_relay(vec![relay.clone()], filters, progress)
                    .await
                    .with_context(|| format!("failed to query container relay {relay}"))?;
                ensure!(
                    relay_results.len() == 1,
                    "relay {relay} did not produce exactly one container query result (got {})",
                    relay_results.len()
                );
                relay_results
                    .pop()
                    .context("container relay result disappeared after its length was checked")?
                    .with_context(|| format!("failed to fetch container events from {relay}"))
            }
            .await;
            (relay, result)
        }
    }))
    .await;
    progress.finish(
        results.iter().any(|(_, result)| result.is_err()),
        results.iter().all(|(_, result)| result.is_err()),
        None,
    )?;

    let mut events = Vec::new();
    let mut failed = Vec::new();
    for (relay, result) in results {
        match result {
            Ok(mut relay_events) => events.append(&mut relay_events),
            Err(error) => failed.push(format!("{relay}: {error:#}")),
        }
    }
    if !failed.is_empty() {
        bail!(
            "container publication preflight did not complete on every relay: {}",
            failed.join("; ")
        );
    }
    Ok(latest_event(events.iter()).cloned())
}

fn ensure_unchanged(before: Option<&Event>, after: Option<&Event>) -> Result<()> {
    let before_id = before.map(|event| event.id);
    let after_id = after.map(|event| event.id);
    ensure!(
        before_id == after_id,
        "container repository changed while blobs were uploading; uploaded blobs are reusable, so inspect the latest event and retry"
    );
    Ok(())
}

async fn upload_layout(
    layout: &OciLayout,
    servers: &[Url],
    signer: &Arc<NgitSigner>,
    json_output: bool,
) -> Result<Vec<(OciBlob, MultiServerUpload)>> {
    let mut uploads = Vec::with_capacity(layout.blobs.len());
    for (index, blob) in layout.blobs.iter().enumerate() {
        if !json_output {
            eprintln!(
                "uploading blob {}/{}: sha256:{} ({} bytes)",
                index + 1,
                layout.blobs.len(),
                blob.digest,
                blob.size
            );
        }
        let mut request = LocalFileRequest::new(&blob.path);
        request.filename = Some(blob.digest.clone());
        request.mime_type = Some("application/octet-stream".to_owned());
        request.max_bytes = u64::MAX;
        let snapshot = snapshot_local_file(request)
            .await
            .with_context(|| format!("failed to snapshot OCI blob {}", blob.digest))?;
        ensure!(
            snapshot.sha256 == blob.digest && snapshot.size == blob.size,
            "OCI blob {} changed after layout validation; no container event was published",
            blob.digest
        );
        let upload = upload_snapshot_to_servers(servers, &snapshot, signer)
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| format!("failed to store OCI blob {} on Blossom", blob.digest))?;
        uploads.push((blob.clone(), upload));
    }
    Ok(uploads)
}

fn upload_json((blob, upload): &(OciBlob, MultiServerUpload)) -> Value {
    json!({
        "sha256": blob.digest,
        "size": blob.size,
        "servers": upload.servers,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ngit::oci::ContainerTag;
    use nostr::prelude::{
        EventBuilder, Keys, Kind, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
    };

    use super::*;

    #[test]
    fn explicit_relays_extend_and_deduplicate_user_write_relays() -> Result<()> {
        let client = Client::new(Params::default());
        let relays = publication_relays(
            &client,
            vec!["wss://relay.one".to_owned()],
            &["wss://relay.one/".to_owned(), "wss://relay.two".to_owned()],
        )?;
        assert_eq!(
            relays.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["wss://relay.one", "wss://relay.two"]
        );
        Ok(())
    }

    #[test]
    fn concurrent_repository_change_is_rejected() -> Result<()> {
        let keys = Keys::generate();
        let before = keys.sign_event(
            EventBuilder::new(Kind::TextNote, "before").finalize_unsigned(keys.public_key()),
        )?;
        let after = keys.sign_event(
            EventBuilder::new(Kind::TextNote, "after").finalize_unsigned(keys.public_key()),
        )?;

        ensure_unchanged(Some(&before), Some(&before))?;
        assert!(ensure_unchanged(Some(&before), Some(&after)).is_err());
        assert!(ensure_unchanged(None, Some(&after)).is_err());
        Ok(())
    }

    #[test]
    fn ordinary_publish_merges_tags_metadata_servers_and_unknown_fields() -> Result<()> {
        let keys = Keys::generate();
        let previous = ContainerRepository {
            name: "app".to_owned(),
            tags: vec![
                ContainerTag {
                    name: "latest".to_owned(),
                    digest: "a".repeat(64),
                },
                ContainerTag {
                    name: "stable".to_owned(),
                    digest: "b".repeat(64),
                },
            ],
            servers: vec![Url::parse("https://old.example/")?],
            title: Some("Existing title".to_owned()),
            description: Some("Existing description".to_owned()),
            source: Some(Url::parse("https://example.com/source")?),
            extra_tags: vec![Tag::parse(["future", "preserved"])?],
        };
        let previous = keys.sign_event(
            previous
                .event_builder()?
                .finalize_unsigned(keys.public_key()),
        )?;
        let layout = OciLayout {
            tags: vec![ContainerTag {
                name: "latest".to_owned(),
                digest: "c".repeat(64),
            }],
            blobs: vec![],
        };
        let args = ContainerPublishArgs {
            repository: "app".to_owned(),
            layout: PathBuf::from("layout"),
            blossom_servers: vec![],
            relays: vec![],
            title: None,
            description: None,
            source: None,
            replace: false,
        };

        let merged = merged_repository(
            &args,
            &layout,
            &[Url::parse("https://new.example/")?],
            None,
            Some(&previous),
        )?;
        assert_eq!(merged.tags.len(), 2);
        assert_eq!(merged.tags[0].digest, "c".repeat(64));
        assert_eq!(merged.tags[1].name, "stable");
        assert_eq!(merged.servers.len(), 2);
        assert_eq!(merged.title.as_deref(), Some("Existing title"));
        assert_eq!(merged.description.as_deref(), Some("Existing description"));
        assert_eq!(merged.extra_tags[0].as_slice(), ["future", "preserved"]);
        Ok(())
    }
}
