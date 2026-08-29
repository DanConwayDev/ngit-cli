use std::{collections::HashSet, fs};

use anyhow::{Context, Result};
use ngit::{
    blossom::{
        BatchUploadResult, BlossomServerStatus, blossom_server_list_filter,
        blossom_server_list_from_events, canonicalize_blossom_server_root,
        upload_snapshot_batch_to_servers,
    },
    client::{send_events, sign_event},
    event_ordering::latest_event,
    nsite::{
        NSITE_NAMED_KIND, NSITE_ROOT_KIND, NsiteManifestInput, manifest_event_builder,
        snapshot_nsite_directory, unique_blob_snapshots, validate_named_site_identifier,
    },
};
use nostr::prelude::{Event, Filter, PublicKey, ToBech32 as _, Url};
use serde_json::{Value, json};

use super::release::support::{
    ReleaseContext, ReleaseError, coded_error, coded_error_with_details, repository_json,
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
            let (code, message, details) = error.downcast_ref::<ReleaseError>().map_or_else(
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
    if let Some(identifier) = args.identifier.as_deref() {
        validate_named_site_identifier(identifier)?;
    }
    let mut context = ReleaseContext::load_for_write(&args.relays, false, signer_params).await?;
    let author = context
        .current_signer()
        .ok_or_else(|| coded_error("not_logged_in", "nostr account required"))?;
    let signer = context
        .signer
        .as_ref()
        .context("nostr signer was not initialized")?
        .clone();
    let description = resolve_description(args)?;
    let servers = resolve_servers(&mut context, author, &args.blossom_servers).await?;
    let previous = load_current_manifest(&mut context, author, args.identifier.as_deref()).await?;

    let files = snapshot_nsite_directory(&args.directory).await?;
    let blobs = unique_blob_snapshots(&files);
    let source = args.source.clone().or_else(|| {
        Some(
            context
                .repo_ref
                .to_nostr_git_url(&Some(&context.git_repo))
                .to_string(),
        )
    });
    let manifest_input = NsiteManifestInput {
        identifier: args.identifier.clone(),
        title: args.title.clone(),
        description,
        source,
        servers: servers.clone(),
    };
    // Validate all manifest values before invoking the signer or touching a
    // Blossom server.
    let (_, aggregate) = manifest_event_builder(&files, &manifest_input)?;

    context.emit_human_warnings_before_signing(json_output);
    if !json_output {
        eprintln!(
            "confirming {} unique blob(s) across {} Blossom server(s)...",
            blobs.len(),
            servers.len()
        );
    }
    let blossom =
        upload_snapshot_batch_to_servers(&servers, &blobs, signer.as_ref(), args.concurrency)
            .await
            .map_err(|error| {
                let details = serde_json::to_value(&error).unwrap_or_else(|_| json!({}));
                coded_error_with_details("blossom_upload_failed", error.message, details)
            })?;

    let current = load_current_manifest(&mut context, author, args.identifier.as_deref()).await?;
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
    let event = sign_event(builder, &signer, "NIP-5A nsite manifest".to_owned()).await?;
    let publication = publish_manifest(&context, &event, json_output).await?;
    let coordinate = manifest_coordinate(author, args.identifier.as_deref());
    let npub = author.to_bech32()?;
    let warnings = std::mem::take(&mut context.warnings);
    let repository = serde_json::to_value(repository_json(&context))?;
    let result = json!({
        "coordinate": coordinate,
        "kind": event.kind.as_u16(),
        "event_id": event.id.to_hex(),
        "event_id_bech32": event.id.to_bech32().ok(),
        "author": author.to_hex(),
        "author_npub": npub,
        "identifier": args.identifier,
        "aggregate_sha256": aggregate,
        "file_count": files.len(),
        "unique_blob_count": blobs.len(),
        "previous_event_id": previous.as_ref().map(|event| event.id.to_hex()),
        "servers": servers.iter().map(Url::as_str).collect::<Vec<_>>(),
        "blossom": blossom_summary(&blossom),
        "publication": publication,
    });
    let site_label = args.identifier.as_deref().map_or_else(
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
        human: format!(
            "published {site_label} with {} file(s)\nevent: {}",
            files.len(),
            event.id
        ),
    })
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
    context: &mut ReleaseContext,
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
    context: &mut ReleaseContext,
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
    let events = context.query(vec![filter], true).await?;
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
    context: &ReleaseContext,
    event: &Event,
    json_output: bool,
) -> Result<Value> {
    let (user_write, repo_relays) = context.publication_relays();
    let results = send_events(
        &context.client,
        Some(context.git_repo_path()?),
        vec![event.clone()],
        user_write,
        repo_relays,
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
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

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
        }
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
                }],
            }],
        };

        assert_eq!(blossom_summary(&result)["confirmed_operations"], 1);
    }
}
