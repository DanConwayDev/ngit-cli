use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ngit::{
    blossom::{
        BatchBlobUploadOutcome, BatchUploadError, BlossomServerStatus, DEFAULT_UPLOAD_CONCURRENCY,
        LocalFileRequest, PossibleOrphanBlob, blossom_server_list_filter,
        blossom_server_list_from_events, canonicalize_blossom_server_root, snapshot_local_file,
        upload_resilient_snapshot_batch_to_servers_with_progress,
    },
    client::{Connect, send_events, sign_draft_event},
    container_manifest::{load_container_manifest, resolve_container_manifest_path},
    event_ordering::{finalize_ordered_unsigned, latest_event},
    git::{Repo, RepoActions},
    oci::{
        CONTAINER_REPOSITORY_KIND, ContainerRepository, OciBlob, OciLayout,
        is_valid_repository_name,
    },
};
use nostr::prelude::{
    Coordinate, Event, Filter, PublicKey, RelayUrl, ToBech32, Url, nip19::Nip19Coordinate,
};
use serde_json::{Value, json};

use super::publication::{
    BlossomUploadProgress, PublicationContext, PublicationError, WarningJson,
    blossom_replication_warning, coded_error_with_details,
};
use crate::{
    cli::{ContainerPublishArgs, SignerParams},
    cli_interactor::CliError,
    output,
};

pub async fn launch(
    args: &ContainerPublishArgs,
    signer_params: SignerParams<'_>,
    json_output: bool,
) -> Result<()> {
    let result = publish(args, signer_params, json_output).await;
    match result {
        Ok(()) => Ok(()),
        Err(error) if json_output => {
            let (code, message, details) = error.downcast_ref::<PublicationError>().map_or_else(
                || {
                    (
                        "operation_failed",
                        format!("{error:#}"),
                        Value::Object(serde_json::Map::new()),
                    )
                },
                |error| (error.code, error.message.clone(), error.details.clone()),
            );
            output::set_value(json!({
                "format_version": 1,
                "ok": false,
                "command": "container.publish",
                "warnings": [],
                "result": null,
                "error": {
                    "code": code,
                    "message": message,
                    "details": details,
                }
            }));
            Err(CliError::already_handled())
        }
        Err(error) => Err(error),
    }
}

async fn publish(
    args: &ContainerPublishArgs,
    signer_params: SignerParams<'_>,
    json_output: bool,
) -> Result<()> {
    let PreparedContainerPublish {
        resolved,
        explicit_servers,
        source,
        layout,
    } = prepare_container_publish(args)?;

    let mut context = PublicationContext::load_for_write(&resolved.relays, signer_params).await?;
    let public_key = context
        .current_signer()
        .context("failed to read the container publisher public key")?;
    ensure!(
        context
            .repo_ref
            .confirmed_maintainers()
            .contains(&public_key),
        "only a confirmed repository maintainer can publish related containers"
    );
    let signer = context
        .signer
        .as_ref()
        .context("nostr signer was not initialized")?
        .clone();
    let repository_coordinate = context.selected_coordinate.coordinate.clone();
    let repository_relays = repository_relays(context.repo_ref.relays.clone(), &resolved.relays)?;
    context
        .client
        .nip42_register_publish_relays(repository_relays.clone());
    let servers = resolve_blossom_servers(&mut context, public_key, explicit_servers).await?;
    let existing = load_current_repository(&mut context, public_key, &resolved.repository).await?;
    let repository = merged_repository(
        &resolved,
        &repository_coordinate,
        &layout,
        &servers,
        source,
        existing.as_ref(),
    )?;
    let event_builder = repository
        .event_builder()
        .context("failed to construct the container repository event")?;

    context.emit_human_warnings_before_signing(json_output);
    let uploads = upload_layout(&layout, &servers, &signer, json_output).await?;
    context.warnings.extend(blossom_replication_warning(
        uploads.iter().map(|(_, upload)| upload.servers.as_slice()),
    ));
    context.emit_human_warnings_before_signing(json_output);

    let rechecked = load_current_repository(&mut context, public_key, &resolved.repository)
        .await
        .context("failed to re-check the container repository after uploading blobs")?;
    ensure_unchanged(existing.as_ref(), rechecked.as_ref())?;
    context.emit_human_warnings_before_signing(json_output);

    let unsigned = finalize_ordered_unsigned(event_builder, public_key, existing.as_ref())
        .context("failed to order the container repository update after uploading blobs")?;
    let event = sign_draft_event(unsigned, &signer, "container repository".to_owned())
        .await
        .context("failed to sign the container repository event; uploaded blobs are reusable")?;
    let relay_results = send_events(
        &context.client,
        Some(context.git_repo_path()?),
        vec![event.clone()],
        vec![],
        repository_relays.clone(),
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
            settings: &resolved,
            public_key,
            repository_relays: &repository_relays,
            event: &event,
            repository: &repository,
            layout: &layout,
            uploads: &uploads,
            relay_results: &relay_results,
            warnings: &context.warnings,
        },
        json_output,
    )?;

    context.client.disconnect().await?;
    Ok(())
}

struct PreparedContainerPublish {
    resolved: ResolvedContainerPublish,
    explicit_servers: Vec<Url>,
    source: Option<Url>,
    layout: OciLayout,
}

fn prepare_container_publish(args: &ContainerPublishArgs) -> Result<PreparedContainerPublish> {
    let git_repo =
        Repo::discover().context("container publication must run inside a Nostr Git repository")?;
    let resolved = resolve_publish_settings(git_repo.get_path()?, args)?;
    validate_metadata_args(&resolved)?;
    let explicit_servers = parse_blossom_servers(&resolved.blossom_servers)?;
    let source = resolved
        .source
        .as_deref()
        .map(parse_source_url)
        .transpose()?;
    let layout = OciLayout::load(&resolved.layout).with_context(|| {
        format!(
            "failed to validate OCI image layout {}",
            resolved.layout.display()
        )
    })?;
    Ok(PreparedContainerPublish {
        resolved,
        explicit_servers,
        source,
        layout,
    })
}

struct PublishSuccess<'a> {
    settings: &'a ResolvedContainerPublish,
    public_key: PublicKey,
    repository_relays: &'a [RelayUrl],
    event: &'a Event,
    repository: &'a ContainerRepository,
    layout: &'a OciLayout,
    uploads: &'a [(OciBlob, BatchBlobUploadOutcome)],
    relay_results: &'a [(String, bool)],
    warnings: &'a [WarningJson],
}

fn render_success(success: &PublishSuccess<'_>, json_output: bool) -> Result<()> {
    let naddr = Nip19Coordinate {
        coordinate: Coordinate::new(CONTAINER_REPOSITORY_KIND, success.public_key)
            .identifier(success.settings.repository.clone()),
        relays: success.repository_relays.to_vec(),
    }
    .to_bech32()?;
    let npub = success.public_key.to_bech32()?;

    if json_output {
        crate::output::set_value(json!({
            "format_version": 1,
            "ok": true,
            "command": "container.publish",
            "warnings": success.warnings,
            "result": {
                "repository": success.settings.repository,
                "manifest_path": success.settings.manifest_path,
                "npub": npub,
                "name": format!("{npub}/{}", success.settings.repository),
                "naddr": naddr,
                "event_id": success.event.id.to_hex(),
                "git_repository": success.repository.repository.to_string(),
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
            success.settings.repository
        );
        println!("  git repository: {}", success.repository.repository);
        for tag in &success.layout.tags {
            println!("  {} -> sha256:{}", tag.name, tag.digest);
        }
        println!("  event: {naddr}");
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedContainerPublish {
    repository: String,
    layout: PathBuf,
    blossom_servers: Vec<String>,
    relays: Vec<String>,
    title: Option<String>,
    description: Option<String>,
    source: Option<String>,
    replace: bool,
    manifest_path: Option<PathBuf>,
}

fn resolve_publish_settings(
    repository_root: &Path,
    args: &ContainerPublishArgs,
) -> Result<ResolvedContainerPublish> {
    let default_path = resolve_container_manifest_path(repository_root, None)?;
    let loaded = if args.no_manifest {
        None
    } else if args.manifest.is_some() || default_path.exists() {
        Some(load_container_manifest(
            repository_root,
            args.manifest.as_deref(),
        )?)
    } else {
        None
    };
    let entry = loaded
        .as_ref()
        .map(|loaded| {
            loaded
                .manifest
                .containers
                .get(&args.repository)
                .with_context(|| {
                    format!(
                        "container manifest {} does not define repository {:?}",
                        loaded.path.display(),
                        args.repository
                    )
                })
        })
        .transpose()?;
    let layout = match (
        args.layout.as_ref(),
        entry.and_then(|entry| entry.layout.as_ref()),
    ) {
        (Some(layout), _) => layout.clone(),
        (None, Some(layout)) if layout.is_absolute() => layout.clone(),
        (None, Some(layout)) => repository_root.join(layout),
        (None, None) => anyhow::bail!(
            "container publication requires --layout PATH or a layout in the selected manifest entry"
        ),
    };
    let manifest_publication = loaded.as_ref().map(|loaded| &loaded.manifest.publication);
    let blossom_servers = if args.blossom_servers.is_empty() {
        manifest_publication
            .map(|publication| publication.blossom_servers.clone())
            .unwrap_or_default()
    } else {
        args.blossom_servers.clone()
    };
    let mut relays = manifest_publication
        .map(|publication| publication.relays.clone())
        .unwrap_or_default();
    relays.extend(args.relays.iter().cloned());

    Ok(ResolvedContainerPublish {
        repository: args.repository.clone(),
        layout,
        blossom_servers,
        relays,
        title: args
            .title
            .clone()
            .or_else(|| entry.and_then(|entry| entry.title.clone())),
        description: args
            .description
            .clone()
            .or_else(|| entry.and_then(|entry| entry.description.clone())),
        source: args
            .source
            .clone()
            .or_else(|| entry.and_then(|entry| entry.source.clone())),
        replace: args.replace,
        manifest_path: loaded.map(|loaded| loaded.path),
    })
}

fn validate_metadata_args(args: &ResolvedContainerPublish) -> Result<()> {
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
    args: &ResolvedContainerPublish,
    git_repository: &Coordinate,
    layout: &OciLayout,
    uploaded_servers: &[Url],
    source: Option<Url>,
    existing: Option<&Event>,
) -> Result<ContainerRepository> {
    let previous = existing
        .map(ContainerRepository::from_event)
        .transpose()
        .context("failed to parse the current container repository event")?;
    if let Some(previous) = previous.as_ref() {
        ensure!(
            previous.name == args.repository,
            "current container repository event names {:?} instead of {:?}",
            previous.name,
            args.repository
        );
        ensure!(
            previous.repository == *git_repository,
            "current container repository belongs to {} instead of the selected Git repository {}",
            previous.repository,
            git_repository
        );
    }
    if args.replace {
        return Ok(new_repository(
            args,
            git_repository,
            layout,
            uploaded_servers,
            source,
        ));
    }
    let Some(previous) = previous else {
        return Ok(new_repository(
            args,
            git_repository,
            layout,
            uploaded_servers,
            source,
        ));
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
        repository: git_repository.clone(),
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
    args: &ResolvedContainerPublish,
    git_repository: &Coordinate,
    layout: &OciLayout,
    servers: &[Url],
    source: Option<Url>,
) -> ContainerRepository {
    ContainerRepository {
        name: args.repository.clone(),
        repository: git_repository.clone(),
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

async fn resolve_blossom_servers(
    context: &mut PublicationContext,
    author: PublicKey,
    explicit_servers: Vec<Url>,
) -> Result<Vec<Url>> {
    if !explicit_servers.is_empty() {
        return Ok(explicit_servers);
    }
    context.add_author_relays(author).await?;
    let events = context
        .query_with_required_discovery_route(vec![blossom_server_list_filter(author)])
        .await?;
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

fn repository_relays(announced: Vec<RelayUrl>, explicit: &[String]) -> Result<Vec<RelayUrl>> {
    let mut seen = HashSet::new();
    let mut relays = Vec::new();
    for relay in announced {
        if seen.insert(relay.to_string().trim_end_matches('/').to_owned()) {
            relays.push(relay);
        }
    }
    for value in explicit {
        let relay = RelayUrl::parse(value)
            .with_context(|| format!("invalid container repository relay {value:?}"))?;
        let key = relay.to_string().trim_end_matches('/').to_owned();
        if seen.insert(key) {
            relays.push(relay);
        }
    }
    ensure!(
        !relays.is_empty(),
        "container publication requires at least one repository relay"
    );
    Ok(relays)
}

async fn load_current_repository(
    context: &mut PublicationContext,
    author: PublicKey,
    repository: &str,
) -> Result<Option<Event>> {
    let filters = vec![
        Filter::new()
            .author(author)
            .kind(CONTAINER_REPOSITORY_KIND)
            .identifier(repository),
    ];
    let events = context
        .query_repository_publication_preflight(filters)
        .await?;
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
    signer: &ngit::NgitSigner,
    json_output: bool,
) -> Result<Vec<(OciBlob, BatchBlobUploadOutcome)>> {
    let mut uploads = Vec::with_capacity(layout.blobs.len());
    let mut completed_outcomes = Vec::with_capacity(layout.blobs.len());
    let progress = BlossomUploadProgress::new(json_output)?;
    for (index, blob) in layout.blobs.iter().enumerate() {
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
        progress.set_sequential_file(
            index + 1,
            layout.blobs.len(),
            format!("sha256:{}", &blob.digest[..12]),
        );
        let batch = upload_resilient_snapshot_batch_to_servers_with_progress(
            servers,
            &[&snapshot],
            signer,
            DEFAULT_UPLOAD_CONCURRENCY,
            progress.clone(),
        )
        .await;
        let mut batch = match batch {
            Ok(batch) => batch,
            Err(error) => {
                return Err(container_blossom_failure(blob, &completed_outcomes, error));
            }
        };
        let outcome = batch
            .blobs
            .pop()
            .context("Blossom placement omitted the requested OCI blob")?;
        ensure!(
            batch.blobs.is_empty() && outcome.sha256 == blob.digest,
            "Blossom placement returned the wrong OCI blob outcome"
        );
        completed_outcomes.push(outcome.clone());
        uploads.push((blob.clone(), outcome));
    }
    Ok(uploads)
}

fn container_blossom_failure(
    blob: &OciBlob,
    completed: &[BatchBlobUploadOutcome],
    error: BatchUploadError,
) -> anyhow::Error {
    let BatchUploadError {
        message,
        blobs,
        possible_orphan_blobs,
    } = error;
    let mut all_blobs = completed.to_vec();
    all_blobs.extend(blobs);
    let mut all_orphans = possible_orphans_from_completed(completed);
    all_orphans.extend(possible_orphan_blobs);
    coded_error_with_details(
        "blossom_upload_failed",
        format!(
            "failed to store OCI blob {} on Blossom: {message}",
            blob.digest
        ),
        serde_json::to_value(BatchUploadError {
            message,
            blobs: all_blobs,
            possible_orphan_blobs: all_orphans,
        })
        .unwrap_or_else(|_| json!({})),
    )
}

fn possible_orphans_from_completed(blobs: &[BatchBlobUploadOutcome]) -> Vec<PossibleOrphanBlob> {
    blobs
        .iter()
        .flat_map(|blob| {
            blob.servers
                .iter()
                .filter(|outcome| outcome.status == BlossomServerStatus::Stored)
                .map(|outcome| PossibleOrphanBlob {
                    server: outcome.server.clone(),
                    sha256: blob.sha256.clone(),
                    url: outcome
                        .descriptor
                        .as_ref()
                        .map(|descriptor| descriptor.url.clone()),
                })
        })
        .collect()
}

fn upload_json((blob, upload): &(OciBlob, BatchBlobUploadOutcome)) -> Value {
    json!({
        "sha256": blob.digest,
        "size": blob.size,
        "servers": upload.servers,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use ngit::oci::ContainerTag;
    use nostr::prelude::{
        EventBuilder, Keys, Kind, Tag,
        event::{FinalizeUnsignedEvent, SignEvent},
    };
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn explicit_relays_extend_and_deduplicate_repository_relays() -> Result<()> {
        let relays = repository_relays(
            vec![RelayUrl::parse("wss://relay.one")?],
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
    fn manifest_settings_are_resolved_with_cli_precedence() -> Result<()> {
        let root = tempdir()?;
        fs::create_dir_all(root.path().join(".ngit"))?;
        fs::write(
            root.path().join(".ngit/containers.yaml"),
            r#"schema: 1
publication:
  blossom_servers: [https://manifest-blossom.example]
  relays: [wss://manifest-relay.example]
containers:
  app:
    layout: artifacts/app
    title: Manifest title
    description: Manifest description
    source: https://manifest-source.example/app
"#,
        )?;
        let args = ContainerPublishArgs {
            repository: "app".to_owned(),
            layout: None,
            manifest: None,
            no_manifest: false,
            blossom_servers: vec!["https://cli-blossom.example".to_owned()],
            relays: vec!["wss://cli-relay.example".to_owned()],
            title: Some("CLI title".to_owned()),
            description: None,
            source: None,
            replace: false,
        };

        let resolved = resolve_publish_settings(root.path(), &args)?;
        assert_eq!(resolved.layout, root.path().join("artifacts/app"));
        assert_eq!(resolved.blossom_servers, ["https://cli-blossom.example"]);
        assert_eq!(
            resolved.relays,
            ["wss://manifest-relay.example", "wss://cli-relay.example"]
        );
        assert_eq!(resolved.title.as_deref(), Some("CLI title"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Manifest description")
        );
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://manifest-source.example/app")
        );
        assert_eq!(
            resolved.manifest_path.as_deref(),
            Some(root.path().join(".ngit/containers.yaml").as_path())
        );
        Ok(())
    }

    #[test]
    fn no_manifest_requires_an_explicit_layout() -> Result<()> {
        let root = tempdir()?;
        let args = ContainerPublishArgs {
            repository: "app".to_owned(),
            layout: None,
            manifest: None,
            no_manifest: true,
            blossom_servers: vec![],
            relays: vec![],
            title: None,
            description: None,
            source: None,
            replace: false,
        };
        let error = resolve_publish_settings(root.path(), &args).unwrap_err();
        assert!(error.to_string().contains("requires --layout PATH"));
        Ok(())
    }

    #[test]
    fn discovered_manifest_must_define_the_requested_repository() -> Result<()> {
        let root = tempdir()?;
        fs::create_dir_all(root.path().join(".ngit"))?;
        fs::write(
            root.path().join(".ngit/containers.yaml"),
            "schema: 1\ncontainers:\n  other:\n    layout: artifacts/other\n",
        )?;
        let args = ContainerPublishArgs {
            repository: "app".to_owned(),
            layout: Some(PathBuf::from("artifacts/app")),
            manifest: None,
            no_manifest: false,
            blossom_servers: vec![],
            relays: vec![],
            title: None,
            description: None,
            source: None,
            replace: false,
        };

        let error = resolve_publish_settings(root.path(), &args).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not define repository \"app\"")
        );
        Ok(())
    }

    #[test]
    fn ordinary_publish_merges_tags_metadata_servers_and_unknown_fields() -> Result<()> {
        let keys = Keys::generate();
        let git_repository = Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key())
            .identifier("source-repository");
        let previous = ContainerRepository {
            name: "app".to_owned(),
            repository: git_repository.clone(),
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
        let args = ResolvedContainerPublish {
            repository: "app".to_owned(),
            layout: PathBuf::from("layout"),
            blossom_servers: vec![],
            relays: vec![],
            title: None,
            description: None,
            source: None,
            replace: false,
            manifest_path: None,
        };

        let merged = merged_repository(
            &args,
            &git_repository,
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

        let other_git_repository = Coordinate::new(Kind::GitRepoAnnouncement, keys.public_key())
            .identifier("other-source-repository");
        let error = merged_repository(
            &args,
            &other_git_repository,
            &layout,
            &[Url::parse("https://new.example/")?],
            None,
            Some(&previous),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("instead of the selected Git repository")
        );

        let mut wrong_name = merged;
        wrong_name.name = "other-app".to_owned();
        let wrong_name = keys.sign_event(
            wrong_name
                .event_builder()?
                .finalize_unsigned(keys.public_key()),
        )?;
        let error = merged_repository(
            &args,
            &git_repository,
            &layout,
            &[Url::parse("https://new.example/")?],
            None,
            Some(&wrong_name),
        )
        .unwrap_err();
        assert!(error.to_string().contains("instead of \"app\""));
        Ok(())
    }
}
