use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context, Result};
use ngit::{
    blossom::{
        BatchUploadResult, BlossomServerStatus, blossom_server_list_filter,
        blossom_server_list_from_events, canonicalize_blossom_server_root,
        upload_resilient_snapshot_batch_to_servers_with_progress,
    },
    client::{send_public_events, sign_draft_event},
    event_ordering::{latest_event, wait_for_strictly_later_timestamp},
    nsite::{
        NSITE_NAMED_KIND, NSITE_ROOT_KIND, NsiteManifestInput, apply_nsite_fallback,
        event_matches_manifest, load_nsite_project_config, manifest_event_builder,
        snapshot_nsite_directory, unique_blob_snapshots, validate_named_site_identifier,
    },
};
use nostr::prelude::{
    Event, Filter, PublicKey, ToBech32 as _, Url, event::FinalizeUnsignedEvent as _,
    nip19::Nip19Event,
};
use serde_json::{Value, json};

use super::publication::{
    BlossomUploadProgress, PublicationContext, PublicationError, WarningJson,
    blossom_replication_warning, coded_error, coded_error_with_details, repository_json,
};
use crate::{
    cli::{NsiteCommands, NsitePublishArgs, NsiteSubCommandArgs, SignerParams},
    cli_interactor::CliError,
    output,
};

const MAX_DESCRIPTION_BYTES: u64 = 64 * 1024;

struct NsiteOutput {
    repository: Value,
    authority: Value,
    warnings: Value,
    result: Value,
    human: String,
}

struct ResolvedNsitePublish {
    config_path: Option<PathBuf>,
    identifier: Option<String>,
    title: Option<String>,
    description: Option<String>,
    source: Option<String>,
    fallback: Option<String>,
    blossom_servers: Vec<String>,
    relays: Vec<String>,
    unsupported_config_publications: Vec<&'static str>,
}

pub(crate) async fn launch(
    args: &NsiteSubCommandArgs,
    signer: SignerParams<'_>,
    json_output: bool,
) -> Result<()> {
    let result = match &args.nsite_command {
        NsiteCommands::Publish(args) => publish(args, signer, json_output).await,
    };
    match result {
        Ok(value) if json_output => {
            output::set_value(json!({
                "format_version": 1,
                "ok": true,
                "command": "nsite.publish",
                "repository": value.repository,
                "authority": value.authority,
                "warnings": value.warnings,
                "result": value.result,
            }));
            Ok(())
        }
        Ok(value) => {
            println!("{}", value.human);
            Ok(())
        }
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
                "command": "nsite.publish",
                "repository": null,
                "authority": null,
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

#[allow(clippy::too_many_lines)]
async fn publish(
    args: &NsitePublishArgs,
    signer_params: SignerParams<'_>,
    json_output: bool,
) -> Result<NsiteOutput> {
    let resolved = resolve_publish_settings(args)?;
    if let Some(identifier) = resolved.identifier.as_deref() {
        validate_named_site_identifier(identifier)?;
    }
    let mut context = PublicationContext::load_for_write(&resolved.relays, signer_params).await?;
    for option in &resolved.unsupported_config_publications {
        context.warnings.push(WarningJson {
            code: "unsupported_nsite_config_option".to_owned(),
            message: format!(
                ".nsite/config.json option {option} is not published by ngit nsite yet"
            ),
            details: json!({ "option": option }),
        });
    }
    let author = context
        .current_signer()
        .ok_or_else(|| coded_error("not_logged_in", "nostr account required"))?;
    let signer = context
        .signer
        .as_ref()
        .context("nostr signer was not initialized")?
        .clone();
    let servers = resolve_servers(&mut context, author, &resolved.blossom_servers).await?;
    let previous =
        load_current_manifest(&mut context, author, resolved.identifier.as_deref()).await?;

    let mut files = snapshot_nsite_directory(&args.directory).await?;
    if let Some(fallback) = resolved.fallback.as_deref() {
        apply_nsite_fallback(&mut files, fallback)?;
    }
    append_snapshot_warnings(&mut context, &files)?;
    let blobs = unique_blob_snapshots(&files)?;
    let source = resolved.source.clone().or_else(|| {
        (!context.repo_ref.private).then(|| {
            context
                .repo_ref
                .to_nostr_git_url(&Some(&context.git_repo))
                .to_string()
        })
    });
    let manifest_input = NsiteManifestInput {
        identifier: resolved.identifier.clone(),
        title: resolved.title.clone(),
        description: resolved.description.clone(),
        source,
        servers: servers.clone(),
        relays: context.explicit_relays.clone(),
    };
    // Validate all manifest values before invoking the signer or touching a
    // Blossom server.
    let (_, aggregate) = manifest_event_builder(&files, &manifest_input)?;

    context.emit_human_warnings_before_signing(json_output);
    let progress = BlossomUploadProgress::new(json_output)?;
    let blossom = upload_resilient_snapshot_batch_to_servers_with_progress(
        &servers,
        &blobs,
        signer.as_ref(),
        args.concurrency,
        progress,
    )
    .await
    .map_err(|error| {
        let details = serde_json::to_value(&error).unwrap_or_else(|_| json!({}));
        coded_error_with_details("blossom_upload_failed", error.message, details)
    })?;
    append_blossom_replication_warning(&mut context, &blossom);
    context.emit_human_warnings_before_signing(json_output);

    let current =
        load_current_manifest(&mut context, author, resolved.identifier.as_deref()).await?;
    if current.as_ref().map(|event| event.id) != previous.as_ref().map(|event| event.id) {
        return Err(coded_error_with_details(
            "concurrent_state_changed",
            "the nsite manifest changed during upload; inspect the current site and retry",
            json!({
                "expected_event_id": previous.as_ref().map(|event| event.id.to_hex()),
                "current_event_id": current.as_ref().map(|event| event.id.to_hex()),
                "blossom": blossom_summary(&blossom),
            }),
        ));
    }

    let (builder, rebuilt_aggregate) = manifest_event_builder(&files, &manifest_input)?;
    debug_assert_eq!(aggregate, rebuilt_aggregate);
    let unchanged_event = current
        .as_ref()
        .filter(|event| event_matches_manifest(event, &builder))
        .cloned();
    let unchanged = unchanged_event.is_some();
    let (event, publication) = if let Some(event) = unchanged_event {
        (
            event,
            json!({
                "status": "not_attempted",
                "reason": "manifest_unchanged",
                "relays": [],
            }),
        )
    } else {
        let created_at = wait_for_strictly_later_timestamp(current.as_ref()).await?;
        let unsigned = builder
            .custom_created_at(created_at)
            .finalize_unsigned(author);
        let event = sign_draft_event(unsigned, &signer, "NIP-5A nsite manifest".to_owned()).await?;
        let publication = publish_manifest(&context, &event, json_output).await?;
        (event, publication)
    };
    let coordinate = manifest_coordinate(author, resolved.identifier.as_deref());
    let npub = author.to_bech32()?;
    let warnings = std::mem::take(&mut context.warnings);
    let repository = serde_json::to_value(repository_json(&context))?;
    let result = json!({
        "coordinate": coordinate,
        "kind": event.kind.as_u16(),
        "event_id": event.id.to_hex(),
        "event_id_bech32": event_id_bech32(&event),
        "author": author.to_hex(),
        "author_npub": npub,
        "identifier": resolved.identifier,
        "config_path": resolved.config_path,
        "fallback": resolved.fallback,
        "aggregate_sha256": aggregate,
        "file_count": files.len(),
        "unique_blob_count": blobs.len(),
        "previous_event_id": previous.as_ref().map(|event| event.id.to_hex()),
        "changed": !unchanged,
        "servers": servers.iter().map(Url::as_str).collect::<Vec<_>>(),
        "relays": context
            .explicit_relays
            .iter()
            .map(nostr::types::RelayUrl::as_str)
            .collect::<Vec<_>>(),
        "blossom": blossom_summary(&blossom),
        "publication": publication,
    });
    let site_label = resolved.identifier.as_deref().map_or_else(
        || format!("root site for {npub}"),
        |identifier| format!("named site {identifier:?} for {npub}"),
    );

    Ok(NsiteOutput {
        repository,
        authority: json!({
            "current_signer": author.to_hex(),
            "current_signer_npub": npub,
            "can_publish": true,
            "blocker": null,
        }),
        warnings: serde_json::to_value(warnings)?,
        result,
        human: if unchanged {
            format!(
                "{site_label} is unchanged with {} file(s)\nevent: {}",
                files.len(),
                event.id
            )
        } else {
            format!(
                "published {site_label} with {} file(s)\nevent: {}",
                files.len(),
                event.id
            )
        },
    })
}

fn resolve_publish_settings(args: &NsitePublishArgs) -> Result<ResolvedNsitePublish> {
    let loaded =
        load_nsite_project_config(args.config.as_deref(), args.no_config).map_err(|error| {
            coded_error(
                "invalid_nsite_config",
                format!("failed to load nsite project configuration: {error:#}"),
            )
        })?;
    let config_path = loaded.as_ref().map(|loaded| loaded.path.clone());
    let config = loaded.map(|loaded| loaded.config).unwrap_or_default();
    let cli_description = resolve_description(args)?;
    let mut unsupported_config_publications = Vec::new();
    if config.publish_profile {
        unsupported_config_publications.push("publishProfile");
    }
    if config.publish_relay_list {
        unsupported_config_publications.push("publishRelayList");
    }
    if config.publish_server_list {
        unsupported_config_publications.push("publishServerList");
    }
    if config.publish_app_handler {
        unsupported_config_publications.push("publishAppHandler");
    }

    Ok(ResolvedNsitePublish {
        config_path,
        identifier: args
            .identifier
            .clone()
            .or_else(|| nonempty_config_value(config.id)),
        title: args
            .title
            .clone()
            .or_else(|| nonempty_config_value(config.title)),
        description: cli_description.or_else(|| nonempty_config_value(config.description)),
        source: args
            .source
            .clone()
            .or_else(|| nonempty_config_value(config.source)),
        fallback: args
            .fallback
            .clone()
            .or_else(|| nonempty_config_value(config.fallback)),
        blossom_servers: if args.blossom_servers.is_empty() {
            config.servers
        } else {
            args.blossom_servers.clone()
        },
        relays: if args.relays.is_empty() {
            config.relays
        } else {
            args.relays.clone()
        },
        unsupported_config_publications,
    })
}

fn nonempty_config_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn resolve_description(args: &NsitePublishArgs) -> Result<Option<String>> {
    if let Some(description) = &args.description {
        return Ok(Some(description.clone()));
    }
    let Some(path) = &args.description_file else {
        return Ok(None);
    };
    let metadata = fs::metadata(path).with_context(|| {
        format!(
            "failed to inspect nsite description file {}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_DESCRIPTION_BYTES {
        return Err(coded_error(
            "invalid_description_file",
            format!(
                "nsite description file must be a regular file no larger than {MAX_DESCRIPTION_BYTES} bytes"
            ),
        ));
    }
    let value = fs::read_to_string(path)
        .with_context(|| format!("failed to read nsite description file {}", path.display()))?;
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(&value)
        .to_owned();
    Ok(Some(value))
}

async fn resolve_servers(
    context: &mut PublicationContext,
    author: PublicKey,
    explicit: &[String],
) -> Result<Vec<Url>> {
    if !explicit.is_empty() {
        let mut seen = HashSet::new();
        let mut servers = Vec::new();
        for value in explicit {
            let server = canonicalize_blossom_server_root(value)?;
            if seen.insert(server.to_string()) {
                servers.push(server);
            }
        }
        return Ok(servers);
    }

    context.add_author_relays(author).await?;
    let events = context
        .query_with_required_discovery_route(vec![blossom_server_list_filter(author)])
        .await?;
    blossom_server_list_from_events(author, &events)
        .map(|list| list.servers)
        .map_err(|error| {
            coded_error_with_details(
                "blossom_servers_not_found",
                format!("{error:#}; provide at least one --blossom-server"),
                json!({ "author": author.to_hex() }),
            )
        })
}

async fn load_current_manifest(
    context: &mut PublicationContext,
    author: PublicKey,
    identifier: Option<&str>,
) -> Result<Option<Event>> {
    let mut filter = Filter::new().author(author).kind(if identifier.is_some() {
        NSITE_NAMED_KIND
    } else {
        NSITE_ROOT_KIND
    });
    if let Some(identifier) = identifier {
        filter = filter.identifier(identifier);
    }
    let events = context
        .query_account_publication_preflight(vec![filter])
        .await?;
    Ok(latest_event(events.iter().filter(|event| {
        event.pubkey == author
            && event.kind
                == if identifier.is_some() {
                    NSITE_NAMED_KIND
                } else {
                    NSITE_ROOT_KIND
                }
            && manifest_identifier(event) == identifier
    }))
    .cloned())
}

fn manifest_identifier(event: &Event) -> Option<&str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "d" => Some(value.as_str()),
        _ => None,
    })
}

async fn publish_manifest(
    context: &PublicationContext,
    event: &Event,
    json_output: bool,
) -> Result<Value> {
    let (user_write, _) = context.publication_relays();
    let additional_relays = manifest_additional_relays(&context.explicit_relays);
    let results = send_public_events(
        &context.client,
        Some(context.git_repo_path()?),
        vec![event.clone()],
        user_write,
        additional_relays,
        !json_output,
        json_output,
    )
    .await?;
    let values = results
        .iter()
        .map(|(url, accepted)| {
            json!({
                "url": url,
                "status": if *accepted { "accepted" } else { "rejected" },
            })
        })
        .collect::<Vec<_>>();
    if !results.iter().any(|(_, accepted)| *accepted) {
        return Err(coded_error_with_details(
            "publication_failed",
            "no relay acknowledged the nsite manifest; uploaded blobs remain reusable",
            json!({
                "event_id": event.id.to_hex(),
                "relays": values,
            }),
        ));
    }
    Ok(json!({ "relays": values }))
}

fn manifest_additional_relays(
    explicit_relays: &[nostr::prelude::RelayUrl],
) -> Vec<nostr::prelude::RelayUrl> {
    explicit_relays.to_vec()
}

fn event_id_bech32(event: &Event) -> Option<String> {
    Nip19Event::new(event.id)
        .author(event.pubkey)
        .kind(event.kind)
        .to_bech32()
        .ok()
}

fn manifest_coordinate(author: PublicKey, identifier: Option<&str>) -> String {
    identifier.map_or_else(
        || format!("{}:{}:", NSITE_ROOT_KIND.as_u16(), author.to_hex()),
        |identifier| {
            format!(
                "{}:{}:{identifier}",
                NSITE_NAMED_KIND.as_u16(),
                author.to_hex()
            )
        },
    )
}

fn blossom_summary(result: &BatchUploadResult) -> Value {
    let mut stored = 0_usize;
    let mut already_present = 0_usize;
    for outcome in result.blobs.iter().flat_map(|blob| blob.servers.iter()) {
        match outcome.status {
            BlossomServerStatus::Stored => stored += 1,
            BlossomServerStatus::AlreadyPresent => already_present += 1,
            _ => {}
        }
    }
    json!({
        "stored": stored,
        "already_present": already_present,
        "confirmed_operations": stored + already_present,
        "blobs": result.blobs,
    })
}

fn append_snapshot_warnings(
    context: &mut PublicationContext,
    files: &[ngit::nsite::NsiteFileSnapshot],
) -> Result<()> {
    context.warnings.extend(grouped_snapshot_warnings(files)?);
    Ok(())
}

fn grouped_snapshot_warnings(files: &[ngit::nsite::NsiteFileSnapshot]) -> Result<Vec<WarningJson>> {
    let mut records = Vec::new();
    for file in files {
        for warning in &file.snapshot.warnings {
            let code = serde_json::to_value(warning.code)?
                .as_str()
                .unwrap_or("nsite_snapshot_warning")
                .to_owned();
            records.push((code, warning.message.clone(), file.path.clone()));
        }
    }
    Ok(group_snapshot_warning_records(records))
}

fn group_snapshot_warning_records(records: Vec<(String, String, String)>) -> Vec<WarningJson> {
    let mut grouped = BTreeMap::<(String, String), Vec<String>>::new();
    for (code, message, path) in records {
        grouped.entry((code, message)).or_default().push(path);
    }
    grouped
        .into_iter()
        .map(|((code, message), paths)| {
            let count = paths.len();
            let examples = paths.iter().take(5).collect::<Vec<_>>();
            let omitted = count.saturating_sub(examples.len());
            WarningJson {
                code,
                message: if count == 1 {
                    message
                } else {
                    format!("{message} ({count} files)")
                },
                details: json!({
                    "count": count,
                    "paths": examples,
                    "omitted": omitted,
                }),
            }
        })
        .collect()
}

fn append_blossom_replication_warning(
    context: &mut PublicationContext,
    blossom: &BatchUploadResult,
) {
    context.warnings.extend(blossom_replication_warning(
        blossom.blobs.iter().map(|blob| blob.servers.as_slice()),
    ));
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use nostr::prelude::{EventBuilder, Keys, RelayUrl, event::SignEvent as _};

    use super::*;
    use crate::cli::{Cli, Commands};

    #[test]
    fn parses_named_publish_api_and_alias() {
        for flag in ["--id", "--name"] {
            let cli = Cli::try_parse_from([
                "ngit",
                "nsite",
                "publish",
                "dist",
                flag,
                "workshop",
                "--title",
                "Git Workshop",
                "--blossom-server",
                "https://blossom.example",
            ])
            .unwrap();
            let Some(Commands::Nsite(nsite)) = cli.command else {
                panic!("nsite command was not parsed");
            };
            let NsiteCommands::Publish(args) = nsite.nsite_command;
            assert_eq!(args.identifier.as_deref(), Some("workshop"));
            assert_eq!(args.directory, std::path::Path::new("dist"));
            assert!(args.config.is_none());
            assert!(!args.no_config);
        }
    }

    #[test]
    fn nsyte_config_supplies_defaults_and_cli_values_win() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config_path = directory.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
                "id": "docs",
                "title": "Config title",
                "description": "Config description",
                "source": "https://config.example/repo",
                "fallback": "/index.html",
                "servers": ["https://config-blossom.example"],
                "relays": ["wss://config-relay.example"],
                "publishServerList": true
            }"#,
        )?;
        let cli = Cli::try_parse_from([
            "ngit",
            "nsite",
            "publish",
            "dist",
            "--config",
            config_path.to_str().unwrap(),
            "--title",
            "CLI title",
            "--fallback",
            "app.html",
            "--blossom-server",
            "https://cli-blossom.example",
            "--relay",
            "wss://cli-relay.example",
        ])?;
        let Some(Commands::Nsite(nsite)) = cli.command else {
            panic!("nsite command was not parsed");
        };
        let NsiteCommands::Publish(args) = nsite.nsite_command;

        let resolved = resolve_publish_settings(&args)?;

        assert_eq!(resolved.config_path.as_deref(), Some(config_path.as_path()));
        assert_eq!(resolved.identifier.as_deref(), Some("docs"));
        assert_eq!(resolved.title.as_deref(), Some("CLI title"));
        assert_eq!(resolved.description.as_deref(), Some("Config description"));
        assert_eq!(
            resolved.source.as_deref(),
            Some("https://config.example/repo")
        );
        assert_eq!(resolved.fallback.as_deref(), Some("app.html"));
        assert_eq!(resolved.blossom_servers, ["https://cli-blossom.example"]);
        assert_eq!(resolved.relays, ["wss://cli-relay.example"]);
        assert_eq!(
            resolved.unsupported_config_publications,
            ["publishServerList"]
        );
        Ok(())
    }

    #[test]
    fn explicit_config_and_no_config_conflict() {
        let Err(error) = Cli::try_parse_from([
            "ngit",
            "nsite",
            "publish",
            "dist",
            "--config",
            ".nsite/config.json",
            "--no-config",
        ]) else {
            panic!("conflicting config options were accepted");
        };

        assert!(error.to_string().contains("cannot be used with"));
    }

    #[test]
    fn blossom_summary_counts_confirmed_operations() {
        let server = Url::parse("https://blossom.example").unwrap();
        let result = BatchUploadResult {
            blobs: vec![ngit::blossom::BatchBlobUploadOutcome {
                sha256: "a".repeat(64),
                servers: vec![ngit::blossom::BlossomServerOutcome {
                    server,
                    operation: ngit::blossom::BlossomServerOperation::Upload,
                    status: BlossomServerStatus::AlreadyPresent,
                    descriptor: None,
                    message: None,
                    presence_check_only: false,
                }],
            }],
        };

        assert_eq!(blossom_summary(&result)["confirmed_operations"], 1);
        assert_eq!(
            blossom_summary(&result)["blobs"][0]["servers"][0]["status"],
            "already_present"
        );
    }

    #[test]
    fn snapshot_warnings_are_grouped_for_large_sites() {
        let warnings = group_snapshot_warning_records(vec![
            (
                "mime_conflict".to_owned(),
                "asset MIME hints disagree".to_owned(),
                "/first.js".to_owned(),
            ),
            (
                "mime_conflict".to_owned(),
                "asset MIME hints disagree".to_owned(),
                "/second.js".to_owned(),
            ),
        ]);
        let warning = &warnings[0];
        assert_eq!(warning.details["count"], 2);
        assert_eq!(warning.details["paths"].as_array().map(Vec::len), Some(2));
        assert_eq!(warning.details["omitted"], 0);
    }

    #[test]
    fn incomplete_blossom_replication_identifies_unconfirmed_servers() {
        let unavailable = Url::parse("https://unavailable.example").unwrap();
        let confirmed = Url::parse("https://confirmed.example").unwrap();
        let result = BatchUploadResult {
            blobs: vec![ngit::blossom::BatchBlobUploadOutcome {
                sha256: "b".repeat(64),
                servers: vec![
                    ngit::blossom::BlossomServerOutcome {
                        server: unavailable.clone(),
                        operation: ngit::blossom::BlossomServerOperation::Upload,
                        status: BlossomServerStatus::Unknown,
                        descriptor: None,
                        message: Some("timed out".to_owned()),
                        presence_check_only: false,
                    },
                    ngit::blossom::BlossomServerOutcome {
                        server: confirmed,
                        operation: ngit::blossom::BlossomServerOperation::Upload,
                        status: BlossomServerStatus::AlreadyPresent,
                        descriptor: None,
                        message: None,
                        presence_check_only: false,
                    },
                ],
            }],
        };
        let warning =
            blossom_replication_warning(result.blobs.iter().map(|blob| blob.servers.as_slice()))
                .unwrap();
        assert_eq!(warning.code, "blossom_replication_incomplete");
        assert_eq!(warning.details["confirmed"], 1);
        assert_eq!(warning.details["placements"], 2);
        assert_eq!(warning.details["servers"][0], unavailable.as_str());
        assert!(warning.message.contains("1/1 blobs are available"));
        assert!(warning.message.contains("1 already stored"));
        assert!(warning.message.contains("1 uncertain"));
        assert_eq!(warning.details["blobs"]["available"], 1);
        assert_eq!(warning.details["copies_by_server"][1]["already_stored"], 1);
    }

    #[test]
    fn nsite_manifests_only_use_explicit_additional_relays() {
        let explicit = RelayUrl::parse("wss://public.example").unwrap();

        assert_eq!(
            manifest_additional_relays(std::slice::from_ref(&explicit)),
            vec![explicit]
        );
    }

    #[test]
    fn json_event_identifier_is_contextual_nevent() {
        let keys = Keys::generate();
        let event = keys
            .sign_event(EventBuilder::new(NSITE_ROOT_KIND, "").finalize_unsigned(keys.public_key()))
            .unwrap();

        assert!(event_id_bech32(&event).unwrap().starts_with("nevent1"));
    }
}
