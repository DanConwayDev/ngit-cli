use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use ngit::{
    apk::{APK_MIME_TYPE, ApkInspection, ApkPlatformInference, inspect_apk},
    blossom::{
        BatchUploadError, BatchUploadResult, BlossomServerList, BlossomServerOperation,
        BlossomServerOutcome, BlossomServerStatus, DEFAULT_UPLOAD_CONCURRENCY, FileSnapshot,
        LocalFileRequest, MultiServerUpload, PossibleOrphanBlob, blossom_server_list_filter,
        blossom_server_list_from_events, canonicalize_blossom_server_root,
        multi_server_upload_from_batch_outcome, snapshot_local_file,
        upload_release_snapshot_batch_to_servers_with_progress,
    },
    client::{sign_draft_event, sign_event},
    event_ordering::{finalize_fixed_timestamp_ordered_unsigned, finalize_ordered_unsigned},
    git::{Repo, RepoActions},
    release_download::{UrlAssetRequest, download_url_asset},
    release_manifest::{
        ResolvedReleaseManifest, ResolvedReleaseManifestAsset, ResolvedReleaseManifestMedia,
        ResolvedReleaseManifestSource, extract_keep_a_changelog_release_notes,
        load_release_manifest, resolve_release_manifest_path,
    },
    software_release::{
        AddressPointer, ApplicationInput, AssetInput, ReleaseAssetInput, ReleaseInput,
        SOFTWARE_APPLICATION_KIND, SoftwareApplication, SoftwareAsset, SoftwareRelease,
        application_event_builder, asset_event_builder, release_event_builder,
    },
};
use nostr::prelude::{
    Coordinate, Event, EventId, Filter, FromBech32, PublicKey, Timestamp, nip19::Nip19Coordinate,
};
use reqwest::Url;
use serde::Serialize;
use serde_json::{Value, json};

use super::support::{
    AssetReuseOption, CommandOutput, ReleaseContext, ReleaseError, WarningJson, application_json,
    asset_json, coded_error, coded_error_with_details, load_applications, load_assets,
    load_context_for_write, load_releases, release_json, resolve_application, resolve_release,
};
use crate::{
    cli::{
        ReleaseAppInitArgs, ReleaseAppLinkArgs, ReleaseAssetAddArgs, ReleasePublishArgs,
        SignerParams,
    },
    sub_commands::{
        id_resolver::parse_event_id,
        publication::{BlossomUploadProgress, blossom_replication_warning},
    },
};

const BLOSSOM_RETRY_RECOVERY: &str = "Blossom blobs are content-addressed. Correct the server list and rerun the command; no NIP-82 event was signed or published.";
const BLOSSOM_DOWNSTREAM_RECOVERY: &str = "Do not delete uploaded blobs automatically. Follow the original failure recovery first; the content-addressed blobs can be reused when the NIP-82 operation is safe to retry.";

#[derive(Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
struct EffectivePublicationSettings {
    blossom_servers: Vec<String>,
    blossom_server_source: &'static str,
    relays: Vec<String>,
    zapstore_relay: bool,
    strict_metadata: bool,
    allow_partial_platforms: bool,
    add_application_platforms: bool,
}

pub(super) async fn app_init(
    args: &ReleaseAppInitArgs,
    signer: SignerParams<'_>,
) -> Result<CommandOutput> {
    super::write_app::app_init(args, signer).await
}

pub(super) async fn app_link(
    args: &ReleaseAppLinkArgs,
    signer: SignerParams<'_>,
) -> Result<CommandOutput> {
    super::write_app::app_link(args, signer).await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn release_publish(
    args: &ReleasePublishArgs,
    signer: SignerParams<'_>,
) -> Result<CommandOutput> {
    let repo = Repo::discover().context("failed to find a git repository")?;
    let ResolvedReleaseInvocation {
        release_version,
        tagged_commit,
        manifest,
    } = resolve_release_invocation(&repo, args)?;
    let publication = effective_publication_settings(args, manifest.as_ref());
    let mut context =
        load_context_for_write(&publication.relays, publication.zapstore_relay, signer).await?;
    enforce_manifest_pubkey(&context, manifest.as_ref())?;
    let app_selector = args.app.as_deref().or_else(|| {
        manifest
            .as_ref()
            .and_then(|manifest| manifest.application.as_deref())
    });
    let maintainers = context.repo_ref.confirmed_maintainers();

    // A valid application found on any reachable route is sufficient to
    // reject a signer who does not own it. Do that deterministic authority
    // check before the fail-closed all-publication-relays preflight so an
    // unrelated relay outage cannot obscure the more fundamental refusal.
    if let Some(selector) = app_selector {
        let warning_count = context.warnings.len();
        let preliminary = load_applications(&mut context, maintainers.clone(), false).await?;
        let preliminary_trusted = preliminary
            .iter()
            .filter(|application| context.application_is_trusted(application))
            .cloned()
            .collect::<Vec<_>>();
        if let Ok(application) = select_application(&preliminary_trusted, Some(selector)) {
            context.require_application_author(application)?;
        }
        context.warnings.truncate(warning_count);
    }

    let discovered_applications = load_applications(&mut context, maintainers, true).await?;
    let trusted_applications = discovered_applications
        .iter()
        .filter(|application| context.application_is_trusted(application))
        .cloned()
        .collect::<Vec<_>>();
    let existing_application = if trusted_applications.is_empty() {
        None
    } else {
        Some(select_application(&trusted_applications, app_selector)?.clone())
    };
    let application_target = if let Some(application) = &existing_application {
        context.require_application_author(application)?;
        ApplicationTarget::from(application)
    } else {
        bootstrap_application_target(&context, &discovered_applications, app_selector)?
    };

    let identifier = format!("{}@{}", application_target.identifier, release_version);
    let existing = load_exact_release(&mut context, &application_target, &release_version).await?;
    enforce_edit_guard(existing.as_ref(), args.edit, "release", &identifier)?;
    let commit = release_commit(
        &context,
        args,
        manifest.as_ref(),
        existing.as_ref(),
        tagged_commit.as_deref(),
    )?;

    let manifest_has_files = manifest.as_ref().is_some_and(|manifest| {
        manifest
            .assets
            .iter()
            .any(|asset| matches!(asset.source, ResolvedReleaseManifestSource::File(_)))
            || manifest_has_local_application_media(manifest)
    });
    let has_local_files =
        manifest_has_files || !args.files.is_empty() || !args.platform_agnostic_files.is_empty();
    if !publication.blossom_servers.is_empty() && !has_local_files {
        return Err(coded_error(
            "blossom_server_without_file",
            "publication.blossom_servers or --blossom-server requires a local release asset or application image",
        ));
    }

    let mut assets = if let Some(release) = &existing {
        require_all_assets(&mut context, release).await?
    } else {
        Vec::new()
    };
    let mut prepared_assets = Vec::new();
    if let Some(manifest) = &manifest {
        for asset in &manifest.assets {
            let prepared = match &asset.source {
                ResolvedReleaseManifestSource::Url(source) => {
                    let input = prepare_url_asset(
                        &mut context,
                        NewUrlAsset::from_manifest(
                            asset,
                            source.clone(),
                            &application_target,
                            &release_version,
                        ),
                    )
                    .await?;
                    PreparedAsset::Ready(input)
                }
                ResolvedReleaseManifestSource::File(path) => {
                    let pending = prepare_file_asset(
                        &mut context,
                        NewFileAsset::from_manifest(
                            asset,
                            path,
                            &application_target,
                            &release_version,
                        ),
                    )
                    .await?;
                    PreparedAsset::File(pending)
                }
            };
            reject_duplicate_prepared_asset(&assets, &prepared_assets, prepared.input())?;
            prepared_assets.push(prepared);
        }
    }
    for value in &args.assets {
        let (platform, url) = value.split_once('=').ok_or_else(|| {
            coded_error(
                "invalid_asset_argument",
                format!("asset {value:?} must use PLATFORM=URL"),
            )
        })?;
        if platform.trim().is_empty() || url.trim().is_empty() {
            return Err(coded_error(
                "invalid_asset_argument",
                format!("asset {value:?} must contain a platform and URL"),
            ));
        }
        let input = prepare_url_asset(
            &mut context,
            NewUrlAsset::simple(
                url,
                vec![platform.to_owned()],
                &application_target,
                &release_version,
            ),
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &prepared_assets, &input)?;
        prepared_assets.push(PreparedAsset::Ready(input));
    }
    for url in &args.platform_agnostic_assets {
        let input = prepare_url_asset(
            &mut context,
            NewUrlAsset::simple(url, Vec::new(), &application_target, &release_version),
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &prepared_assets, &input)?;
        prepared_assets.push(PreparedAsset::Ready(input));
    }
    for (path, platforms) in grouped_file_arguments(&args.files, &args.file_platforms)? {
        let pending = prepare_file_asset(
            &mut context,
            NewFileAsset::simple(
                &path,
                platforms,
                &application_target,
                &release_version,
                false,
            ),
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &prepared_assets, &pending.input)?;
        prepared_assets.push(PreparedAsset::File(pending));
    }
    for path in &args.platform_agnostic_files {
        let pending = prepare_file_asset(
            &mut context,
            NewFileAsset::simple(
                path,
                Vec::new(),
                &application_target,
                &release_version,
                true,
            ),
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &prepared_assets, &pending.input)?;
        prepared_assets.push(PreparedAsset::File(pending));
    }

    let mut reused_assets = Vec::new();
    let mut reused_asset_ids = Vec::new();
    for selector in &args.asset_events {
        let asset = load_asset_event(&mut context, selector, true).await?;
        validate_reused_asset(
            &application_target,
            &asset,
            args.accept_platform_agnostic_assets,
        )?;
        reject_duplicate_asset(&assets, &asset)?;
        reject_duplicate_asset(&reused_assets, &asset)?;
        reject_asset_against_prepared(&prepared_assets, &asset)?;
        reused_asset_ids.push(asset.raw_event.id.to_hex());
        reused_assets.push(asset);
    }

    if assets.is_empty() && prepared_assets.is_empty() && reused_assets.is_empty() {
        return Err(coded_error(
            "release_assets_required",
            "a release requires at least one asset",
        ));
    }

    let blossom_selection = resolve_blossom_server_selection(
        &mut context,
        &application_target,
        &publication.blossom_servers,
        publication.blossom_server_source,
        has_local_files,
    )
    .await?;

    let notes = release_notes(
        &context,
        args,
        manifest.as_ref(),
        existing.as_ref(),
        &release_version,
    )?;
    let channel = args
        .channel
        .clone()
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|manifest| manifest.channel.clone())
        })
        .or_else(|| existing.as_ref().map(|release| release.channel.clone()))
        .unwrap_or_else(|| "main".to_owned());
    if notes.trim().is_empty() {
        context.warnings.push(WarningJson::new(
            "release_notes_missing",
            "release notes are empty; provide --notes, --notes-file, manifest notes, or manifest release_notes",
        ));
    }
    let release_platforms = proposed_platforms(&assets, &prepared_assets, &reused_assets);
    let application_platforms = existing_application.as_ref().map_or_else(
        || release_platforms.clone(),
        |application| application.platforms.clone(),
    );
    let platform_policy = enforce_platform_policy(
        &mut context.warnings,
        &application_platforms,
        &release_platforms,
        &channel,
        publication.allow_partial_platforms,
        publication.add_application_platforms,
    )?;
    let mut prepared_application = prepare_manifest_application_metadata(
        &mut context,
        manifest.as_ref(),
        existing_application.as_ref(),
        &application_target,
        platform_policy.resulting_application_platforms.clone(),
        blossom_selection.as_ref(),
    )
    .await?;
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        existing_application.as_ref(),
        &release_version,
        existing.as_ref(),
    )
    .await?;
    if let Some(prepared) = &prepared_application {
        super::write_app::add_metadata_warnings(
            &mut context,
            &prepared.input,
            publication.strict_metadata,
        )?;
    }
    enforce_metadata_policy(&context, publication.strict_metadata)?;
    context.emit_human_warnings_before_signing(args.json);

    let signer = context
        .signer
        .as_ref()
        .context("nostr signer was not initialized")?
        .clone();
    let blossom = match blossom_selection.as_ref() {
        Some(selection) => {
            upload_prepared_files(
                prepared_application.as_mut(),
                &mut prepared_assets,
                selection,
                &signer,
                args.json,
            )
            .await?
        }
        None => BlossomPublication::empty(),
    };
    if let Some(warning) = blossom.incomplete_replication_warning() {
        context.warnings.push(warning);
        context.emit_human_warnings_before_signing(args.json);
    }
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        existing_application.as_ref(),
        &release_version,
        existing.as_ref(),
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(error, &blossom, "state_recheck", Nip82Progress::none())
    })?;

    let application_changed = match (&existing_application, &prepared_application) {
        (None, _) => true,
        (Some(existing), Some(prepared)) => !application_input_matches(existing, &prepared.input),
        (Some(_), None) => false,
    };
    let application = match (existing_application.as_ref(), prepared_application) {
        (Some(existing), Some(prepared)) if application_changed => {
            sign_application_update(existing, prepared.input, &signer)
                .await
                .map_err(|error| {
                    preserve_completed_blossom(
                        error,
                        &blossom,
                        "application_signing",
                        Nip82Progress::none(),
                    )
                })?
        }
        (Some(existing), _) => existing.clone(),
        (None, Some(prepared)) => {
            sign_bootstrap_application(prepared.input, &application_target, &signer)
                .await
                .map_err(|error| {
                    preserve_completed_blossom(
                        error,
                        &blossom,
                        "application_signing",
                        Nip82Progress::none(),
                    )
                })?
        }
        (None, None) => unreachable!("a missing application always has prepared metadata"),
    };
    let signed_application_id = application_changed.then_some(application.raw_event.id);
    let mut new_asset_event_ids = Vec::new();
    for prepared in prepared_assets {
        let input = match prepared {
            PreparedAsset::Ready(input) => input,
            PreparedAsset::File(pending) => pending.input,
        };
        let asset = sign_asset_input(input, &signer).await.map_err(|error| {
            preserve_completed_blossom(
                error,
                &blossom,
                "asset_signing",
                Nip82Progress::assets(signed_application_id.as_ref(), &new_asset_event_ids),
            )
        })?;
        new_asset_event_ids.push(asset.raw_event.id);
        reject_duplicate_asset(&assets, &asset).map_err(|error| {
            preserve_completed_blossom(
                error,
                &blossom,
                "asset_validation",
                Nip82Progress::assets(signed_application_id.as_ref(), &new_asset_event_ids),
            )
        })?;
        assets.push(asset);
    }
    assets.extend(reused_assets);
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        existing_application.as_ref(),
        &release_version,
        existing.as_ref(),
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(
            error,
            &blossom,
            "state_recheck",
            Nip82Progress::assets(signed_application_id.as_ref(), &new_asset_event_ids),
        )
    })?;
    context.require_application_author(&application)?;

    let released_at = Timestamp::from_secs(
        args.released_at
            .or_else(|| {
                existing
                    .as_ref()
                    .map(|release| release.raw_event.created_at.as_secs())
            })
            .unwrap_or_else(|| Timestamp::now().as_secs()),
    );
    let release_event = build_release_event(
        &context,
        &application,
        &release_version,
        channel,
        notes,
        commit,
        &assets,
        existing.as_ref(),
        released_at,
        &signer,
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(
            error,
            &blossom,
            "release_signing",
            Nip82Progress::assets(signed_application_id.as_ref(), &new_asset_event_ids),
        )
    })?;
    let parsed_release = SoftwareRelease::parse(&release_event).map_err(|error| {
        preserve_completed_blossom(
            error.into(),
            &blossom,
            "release_validation",
            Nip82Progress::release(signed_application_id.as_ref(), &new_asset_event_ids),
        )
    })?;

    let mut batch = vec![application.raw_event.clone()];
    batch.extend(assets.iter().map(|asset| asset.raw_event.clone()));
    batch.push(release_event);
    let relay_results = context
        .publish_batch(
            batch,
            &new_asset_event_ids,
            AssetReuseOption::ReleasePublish,
            args.json,
        )
        .await
        .map_err(|error| {
            preserve_completed_blossom(
                error,
                &blossom,
                "relay_publication",
                Nip82Progress::release(signed_application_id.as_ref(), &new_asset_event_ids),
            )
        })?;
    let authority = context.authority(&application);
    let operation = if existing.is_some() {
        "edited"
    } else {
        "created"
    };
    let application_operation = if existing_application.is_none() {
        "created"
    } else if application_changed {
        "edited"
    } else {
        "unchanged"
    };
    let result = json!({
        "operation": operation,
        "application_operation": application_operation,
        "previous_application_event_id": existing_application.as_ref().map(|application| application.raw_event.id.to_hex()),
        "application": application_json(&context, &application),
        "platform_policy": platform_policy,
        "release": release_json(&parsed_release, &assets),
        "assets": assets.iter().map(asset_json).collect::<Vec<_>>(),
        "previous_event_id": existing.as_ref().map(|release| release.raw_event.id.to_hex()),
        "newly_published_asset_ids": new_asset_event_ids.iter().map(nostr::prelude::EventId::to_hex).collect::<Vec<_>>(),
        "reused_asset_ids": reused_asset_ids,
        "publication": relay_results.json(),
        "blossom": blossom.json,
    });
    Ok(CommandOutput::new(
        "release.publish",
        &mut context,
        authority,
        result,
        format!(
            "{operation} release {} with {} asset(s)\nevent: {}",
            parsed_release.identifier,
            assets.len(),
            parsed_release.raw_event.id
        ),
    ))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn asset_add(
    args: &ReleaseAssetAddArgs,
    signer: SignerParams<'_>,
) -> Result<CommandOutput> {
    let mut context = load_context_for_write(&args.relays, args.zapstore_relay, signer).await?;
    let applications = trusted_applications_for_write(&mut context).await?;
    let releases = load_releases(&mut context, &applications, true).await?;
    let release =
        resolve_release(&releases, &applications, &args.release, args.app.as_deref())?.clone();
    let application = applications
        .iter()
        .find(|application| application.coordinate() == release.application.coordinate)
        .context("release application was not resolved")?
        .clone();
    context.require_application_author(&application)?;
    let existing_application = application.clone();
    let application_target = ApplicationTarget::from(&application);

    let mut assets = require_all_assets(&mut context, &release).await?;
    let local_apk_candidate = can_infer_local_apk_platforms(
        args.file.as_deref(),
        args.filename.as_deref(),
        args.mime.as_deref(),
    );
    if (args.url.is_some() || args.file.is_some())
        && args.platforms.is_empty()
        && !args.platform_agnostic
        && !local_apk_candidate
    {
        return Err(coded_error(
            "asset_platform_required",
            "provide at least one --platform or explicitly use --platform-agnostic",
        ));
    }
    let (mut prepared_asset, reused_asset) = if let Some(url) = &args.url {
        let proposed = asset_add_metadata(args, url.clone(), &application, &release);
        let input = prepare_url_asset(&mut context, proposed).await?;
        reject_duplicate_prepared_asset(&assets, &[], &input)?;
        (Some(PreparedAsset::Ready(input)), None)
    } else if let Some(path) = &args.file {
        let pending = prepare_file_asset(
            &mut context,
            NewFileAsset {
                source_path: path.clone(),
                metadata: asset_add_metadata(args, String::new(), &application, &release),
                platform_agnostic: args.platform_agnostic,
            },
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &[], &pending.input)?;
        (Some(PreparedAsset::File(pending)), None)
    } else {
        let selector = args.event.as_deref().context("--event is required")?;
        let asset = load_asset_event(&mut context, selector, true).await?;
        validate_reused_asset(&application_target, &asset, args.platform_agnostic)?;
        reject_duplicate_asset(&assets, &asset)?;
        (None, Some(asset))
    };
    let blossom_selection = resolve_blossom_server_selection(
        &mut context,
        &application_target,
        &args.blossom_servers,
        "explicit",
        matches!(prepared_asset, Some(PreparedAsset::File(_))),
    )
    .await?;
    let release_platforms = assets
        .iter()
        .flat_map(|asset| asset.platforms.iter())
        .chain(
            prepared_asset
                .iter()
                .flat_map(|asset| asset.input().platforms.iter()),
        )
        .chain(reused_asset.iter().flat_map(|asset| asset.platforms.iter()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let platform_policy = enforce_platform_policy(
        &mut context.warnings,
        &application.platforms,
        &release_platforms,
        &release.channel,
        args.allow_partial_platforms,
        args.add_application_platforms,
    )?;
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        Some(&application),
        &release.version,
        Some(&release),
    )
    .await?;
    enforce_metadata_policy(&context, args.strict_metadata)?;
    context.emit_human_warnings_before_signing(args.json);

    let signer = context
        .signer
        .as_ref()
        .context("nostr signer was not initialized")?
        .clone();
    let blossom = match (prepared_asset.as_mut(), blossom_selection.as_ref()) {
        (Some(prepared), Some(selection)) => {
            upload_prepared_files(
                None,
                std::slice::from_mut(prepared),
                selection,
                &signer,
                args.json,
            )
            .await?
        }
        _ => BlossomPublication::empty(),
    };
    if let Some(warning) = blossom.incomplete_replication_warning() {
        context.warnings.push(warning);
        context.emit_human_warnings_before_signing(args.json);
    }
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        Some(&existing_application),
        &release.version,
        Some(&release),
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(error, &blossom, "state_recheck", Nip82Progress::none())
    })?;
    let application = if platform_policy.application_platforms_added.is_empty() {
        application
    } else {
        sign_application_platform_update(
            &application,
            &platform_policy.resulting_application_platforms,
            &signer,
        )
        .await
        .map_err(|error| {
            preserve_completed_blossom(
                error,
                &blossom,
                "application_signing",
                Nip82Progress::none(),
            )
        })?
    };
    let signed_application_id = (!platform_policy.application_platforms_added.is_empty())
        .then_some(application.raw_event.id);
    let newly_published = prepared_asset.is_some();
    let asset = if let Some(prepared) = prepared_asset {
        let input = match prepared {
            PreparedAsset::Ready(input) => input,
            PreparedAsset::File(pending) => pending.input,
        };
        sign_asset_input(input, &signer).await.map_err(|error| {
            preserve_completed_blossom(
                error,
                &blossom,
                "asset_signing",
                Nip82Progress::application(signed_application_id.as_ref()),
            )
        })?
    } else {
        reused_asset.context("asset add requires one URL or event source")?
    };

    reject_duplicate_asset(&assets, &asset)?;
    let added_id = asset.raw_event.id;
    assets.push(asset);

    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        Some(&existing_application),
        &release.version,
        Some(&release),
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(
            error,
            &blossom,
            "state_recheck",
            Nip82Progress::assets(
                signed_application_id.as_ref(),
                std::slice::from_ref(&added_id),
            ),
        )
    })?;
    context.require_application_author(&application)?;
    let release_event = build_release_event(
        &context,
        &application,
        &release.version,
        release.channel.clone(),
        release.notes.clone(),
        release.commit.clone(),
        &assets,
        Some(&release),
        release.raw_event.created_at,
        &signer,
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(
            error,
            &blossom,
            "release_signing",
            Nip82Progress::assets(
                signed_application_id.as_ref(),
                std::slice::from_ref(&added_id),
            ),
        )
    })?;
    let parsed_release = SoftwareRelease::parse(&release_event).map_err(|error| {
        preserve_completed_blossom(
            error.into(),
            &blossom,
            "release_validation",
            Nip82Progress::release(
                signed_application_id.as_ref(),
                std::slice::from_ref(&added_id),
            ),
        )
    })?;

    let mut batch = vec![application.raw_event.clone()];
    batch.extend(assets.iter().map(|asset| asset.raw_event.clone()));
    batch.push(release_event);
    let possible_orphans = if newly_published {
        vec![added_id]
    } else {
        Vec::new()
    };
    let relay_results = context
        .publish_batch(
            batch,
            &possible_orphans,
            AssetReuseOption::AssetAdd,
            args.json,
        )
        .await
        .map_err(|error| {
            preserve_completed_blossom(
                error,
                &blossom,
                "relay_publication",
                Nip82Progress::release(
                    signed_application_id.as_ref(),
                    std::slice::from_ref(&added_id),
                ),
            )
        })?;
    let authority = context.authority(&application);
    let result = json!({
        "operation": "asset_added",
        "application_operation": if platform_policy.application_platforms_added.is_empty() { "unchanged" } else { "edited" },
        "previous_application_event_id": existing_application.raw_event.id.to_hex(),
        "application": application_json(&context, &application),
        "platform_policy": platform_policy,
        "release": release_json(&parsed_release, &assets),
        "asset": assets.last().map(asset_json),
        "previous_event_id": release.raw_event.id.to_hex(),
        "newly_published_asset_ids": if newly_published { vec![added_id.to_hex()] } else { Vec::<String>::new() },
        "reused_asset_ids": if newly_published { Vec::<String>::new() } else { vec![added_id.to_hex()] },
        "publication": relay_results.json(),
        "blossom": blossom.json,
    });
    Ok(CommandOutput::new(
        "release.asset.add",
        &mut context,
        authority,
        result,
        format!(
            "added asset {added_id} to release {}\nreplacement event: {}",
            parsed_release.identifier, parsed_release.raw_event.id
        ),
    ))
}

async fn trusted_applications_for_write(
    context: &mut ReleaseContext,
) -> Result<Vec<SoftwareApplication>> {
    let applications =
        load_applications(context, context.repo_ref.confirmed_maintainers(), true).await?;
    Ok(applications
        .into_iter()
        .filter(|application| context.application_is_trusted(application))
        .collect())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ApplicationTarget {
    author: PublicKey,
    identifier: String,
}

impl ApplicationTarget {
    fn coordinate(&self) -> Coordinate {
        Coordinate::new(SOFTWARE_APPLICATION_KIND, self.author).identifier(self.identifier.clone())
    }
}

impl From<&SoftwareApplication> for ApplicationTarget {
    fn from(application: &SoftwareApplication) -> Self {
        Self {
            author: application.raw_event.pubkey,
            identifier: application.identifier.clone(),
        }
    }
}

const MAX_APPLICATION_MEDIA_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug)]
struct PreparedApplicationMetadata {
    input: ApplicationInput,
    local_media: Vec<PendingApplicationMedia>,
}

#[derive(Clone, Copy, Debug)]
enum ApplicationMediaSlot {
    Icon,
    Image(usize),
}

#[derive(Debug)]
struct PendingApplicationMedia {
    source_path: String,
    slot: ApplicationMediaSlot,
    snapshot: FileSnapshot,
}

impl PreparedApplicationMetadata {
    fn set_media_url(&mut self, slot: ApplicationMediaSlot, url: String) {
        match slot {
            ApplicationMediaSlot::Icon => self.input.icon = Some(url),
            ApplicationMediaSlot::Image(index) => self.input.images[index] = url,
        }
    }
}

fn enforce_manifest_pubkey(
    context: &ReleaseContext,
    manifest: Option<&ResolvedReleaseManifest>,
) -> Result<()> {
    let Some(expected) = manifest.and_then(|manifest| manifest.pubkey.as_deref()) else {
        return Ok(());
    };
    let expected = PublicKey::parse(expected).map_err(|error| {
        coded_error_with_details(
            "invalid_application_pubkey",
            format!("manifest pubkey is not a valid npub or hexadecimal public key: {error}"),
            json!({ "pubkey": expected }),
        )
    })?;
    let actual = context
        .current_signer()
        .ok_or_else(|| coded_error("not_logged_in", "release publication requires login"))?;
    if expected != actual {
        return Err(coded_error_with_details(
            "application_author_mismatch",
            "the active signer does not match manifest pubkey",
            json!({
                "expected_author": expected.to_hex(),
                "actual_author": actual.to_hex(),
            }),
        ));
    }
    Ok(())
}

fn manifest_has_application_metadata(manifest: &ResolvedReleaseManifest) -> bool {
    manifest.name.is_some()
        || manifest.summary.is_some()
        || manifest.description.is_some()
        || manifest.tags.is_some()
        || manifest.license.is_some()
        || manifest.website.is_some()
        || manifest.repository.is_some()
        || manifest.icon.is_some()
        || manifest.images.is_some()
        || manifest.communities.is_some()
}

fn manifest_has_local_application_media(manifest: &ResolvedReleaseManifest) -> bool {
    manifest
        .icon
        .as_ref()
        .is_some_and(|media| matches!(media, ResolvedReleaseManifestMedia::File(_)))
        || manifest.images.as_ref().is_some_and(|images| {
            images
                .iter()
                .any(|media| matches!(media, ResolvedReleaseManifestMedia::File(_)))
        })
}

async fn prepare_manifest_application_metadata(
    context: &mut ReleaseContext,
    manifest: Option<&ResolvedReleaseManifest>,
    existing: Option<&SoftwareApplication>,
    target: &ApplicationTarget,
    platforms: Vec<String>,
    blossom_selection: Option<&BlossomServerSelection>,
) -> Result<Option<PreparedApplicationMetadata>> {
    let declared = manifest.filter(|manifest| manifest_has_application_metadata(manifest));
    if existing.is_some()
        && declared.is_none()
        && existing.is_some_and(|application| application.platforms == platforms)
    {
        return Ok(None);
    }

    let mut input = if let Some(existing) = existing {
        ApplicationInput::from(existing)
    } else {
        super::write_app::application_input_from_repository(
            context,
            &target.identifier,
            Vec::new(),
            declared.and_then(|manifest| manifest.name.as_deref()),
        )?
    };
    input.platforms = platforms;
    let mut prepared = PreparedApplicationMetadata {
        input,
        local_media: Vec::new(),
    };

    if let Some(manifest) = declared {
        if let Some(name) = &manifest.name {
            prepared.input.name.clone_from(name);
        }
        if let Some(summary) = &manifest.summary {
            prepared.input.summary = Some(summary.clone());
        }
        if let Some(description) = &manifest.description {
            prepared.input.description.clone_from(description);
        }
        if let Some(tags) = &manifest.tags {
            prepared.input.topics.clone_from(tags);
        }
        if let Some(license) = &manifest.license {
            prepared.input.license = Some(license.clone());
        }
        if let Some(website) = &manifest.website {
            prepared.input.website = Some(website.clone());
        }
        if let Some(repository) = &manifest.repository {
            prepared.input.repository = Some(repository.clone());
        }
        if let Some(communities) = &manifest.communities {
            prepared.input.communities.clone_from(communities);
        }
        if let Some(icon) = &manifest.icon {
            apply_manifest_media(context, icon, ApplicationMediaSlot::Icon, &mut prepared).await?;
        }
        if let Some(images) = &manifest.images {
            prepared.input.images = Vec::with_capacity(images.len());
            for (index, image) in images.iter().enumerate() {
                prepared.input.images.push(String::new());
                apply_manifest_media(
                    context,
                    image,
                    ApplicationMediaSlot::Image(index),
                    &mut prepared,
                )
                .await?;
            }
        }
    }

    if !prepared.local_media.is_empty() {
        let selection = blossom_selection
            .context("internal error: local application media did not select Blossom servers")?;
        let primary = selection
            .servers
            .first()
            .context("Blossom server selection is empty")?;
        let predicted = prepared
            .local_media
            .iter()
            .map(|media| {
                primary
                    .join(&media.snapshot.sha256)
                    .context("failed to construct application media Blossom URL")
                    .map(|url| (media.slot, url.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        for (slot, url) in predicted {
            prepared.set_media_url(slot, url);
        }
    }
    validate_application_input(&prepared.input)?;
    Ok(Some(prepared))
}

async fn apply_manifest_media(
    context: &mut ReleaseContext,
    media: &ResolvedReleaseManifestMedia,
    slot: ApplicationMediaSlot,
    prepared: &mut PreparedApplicationMetadata,
) -> Result<()> {
    match media {
        ResolvedReleaseManifestMedia::Url(url) => {
            prepared.set_media_url(slot, url.clone());
        }
        ResolvedReleaseManifestMedia::File(path) => {
            let source_path = tracked_application_media_path(context, path)?;
            let mut request = LocalFileRequest::new(&source_path);
            request.max_bytes = MAX_APPLICATION_MEDIA_BYTES;
            let snapshot = snapshot_local_file(request).await.with_context(|| {
                format!("failed to snapshot application media {}", path.display())
            })?;
            append_download_warnings(context, &snapshot.warnings)?;
            if !snapshot.mime_type.starts_with("image/") {
                return Err(coded_error_with_details(
                    "invalid_application_media",
                    "local application media must resolve to an image MIME type",
                    json!({
                        "path": path.display().to_string(),
                        "mime": snapshot.mime_type,
                    }),
                ));
            }
            prepared.local_media.push(PendingApplicationMedia {
                source_path: path.display().to_string(),
                slot,
                snapshot,
            });
        }
    }
    Ok(())
}

fn tracked_application_media_path(context: &ReleaseContext, declared: &Path) -> Result<PathBuf> {
    if declared.is_absolute() {
        return Err(coded_error(
            "application_media_not_tracked",
            "local application media must be a repository-relative tracked file",
        ));
    }
    let mut relative = PathBuf::new();
    for component in declared.components() {
        match component {
            Component::Normal(component) => relative.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(coded_error(
                    "application_media_not_tracked",
                    "local application media must remain within the repository",
                ));
            }
        }
    }
    if relative.as_os_str().is_empty()
        || context
            .git_repo
            .git_repo
            .index()
            .context("failed to read the Git index")?
            .get_path(&relative, 0)
            .is_none()
    {
        return Err(coded_error_with_details(
            "application_media_not_tracked",
            "local application media must be tracked by Git before publication",
            json!({ "path": declared.display().to_string() }),
        ));
    }
    let root = fs::canonicalize(context.git_repo_path()?)
        .context("failed to resolve the repository root")?;
    let source = root.join(&relative);
    let resolved = fs::canonicalize(&source)
        .with_context(|| format!("failed to resolve application media {}", declared.display()))?;
    if !resolved.starts_with(&root) {
        return Err(coded_error(
            "application_media_not_tracked",
            "local application media must not resolve outside the repository",
        ));
    }
    Ok(source)
}

fn validate_application_input(input: &ApplicationInput) -> Result<()> {
    application_event_builder(input.clone())
        .map(|_| ())
        .map_err(|error| {
            coded_error_with_details(
                "invalid_application_metadata",
                error.to_string(),
                json!({ "validation": error.issues }),
            )
        })
}

fn application_input_matches(existing: &SoftwareApplication, input: &ApplicationInput) -> bool {
    existing.identifier == input.identifier
        && existing.name == input.name
        && existing.description == input.description
        && existing.summary == input.summary
        && existing.icon == input.icon
        && existing.images == input.images
        && existing.topics == input.topics
        && existing.communities == input.communities
        && existing.website == input.website
        && existing.repository == input.repository
        && existing.repository_coordinates == input.repository_coordinates
        && existing.platforms == input.platforms
        && existing.license == input.license
        && existing.extra_tags == input.extra_tags
}

fn proposed_platforms(
    existing: &[SoftwareAsset],
    prepared: &[PreparedAsset],
    reused: &[SoftwareAsset],
) -> Vec<String> {
    existing
        .iter()
        .flat_map(|asset| asset.platforms.iter())
        .chain(
            prepared
                .iter()
                .flat_map(|asset| asset.input().platforms.iter()),
        )
        .chain(reused.iter().flat_map(|asset| asset.platforms.iter()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PlatformPolicy {
    channel: String,
    application_platforms: Vec<String>,
    release_platforms: Vec<String>,
    missing_from_release: Vec<String>,
    additional_to_application: Vec<String>,
    application_platforms_added: Vec<String>,
    resulting_application_platforms: Vec<String>,
    partial_release: bool,
}

fn enforce_platform_policy(
    warnings: &mut Vec<WarningJson>,
    application_platforms: &[String],
    release_platforms: &[String],
    channel: &str,
    allow_partial_platforms: bool,
    add_application_platforms: bool,
) -> Result<PlatformPolicy> {
    let application = application_platforms
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let release = release_platforms.iter().cloned().collect::<BTreeSet<_>>();
    let missing_from_release = application
        .difference(&release)
        .cloned()
        .collect::<Vec<_>>();
    let additional_to_application = release
        .difference(&application)
        .cloned()
        .collect::<Vec<_>>();
    let main_channel = channel == "main";

    if main_channel && !missing_from_release.is_empty() {
        return Err(coded_error_with_details(
            "release_platform_coverage_incomplete",
            "a main release must include every application platform",
            json!({
                "application_platforms": application,
                "release_platforms": release,
                "missing_from_release": missing_from_release,
            }),
        ));
    }
    if !main_channel && !missing_from_release.is_empty() && !allow_partial_platforms {
        return Err(coded_error_with_details(
            "partial_platform_confirmation_required",
            "this non-main release omits application platforms; pass --allow-partial-platforms after checking client compatibility",
            json!({
                "channel": channel,
                "application_platforms": application,
                "release_platforms": release,
                "missing_from_release": missing_from_release,
            }),
        ));
    }
    if main_channel && !additional_to_application.is_empty() && !add_application_platforms {
        return Err(coded_error_with_details(
            "application_platform_update_required",
            "this main release introduces platforms absent from the application; pass --add-application-platforms to update it first",
            json!({
                "application_platforms": application,
                "release_platforms": release,
                "additional_to_application": additional_to_application,
            }),
        ));
    }
    if !main_channel && !missing_from_release.is_empty() {
        warnings.push(
            WarningJson::new(
                "partial_platform_release",
                "the release omits application platforms; current Zapstore clients may select it without considering channel or platform",
            )
            .with_details(json!({
                "channel": channel,
                "missing_from_release": &missing_from_release,
            })),
        );
    }

    let application_platforms_added = if add_application_platforms {
        additional_to_application.clone()
    } else {
        Vec::new()
    };
    let mut resulting_application_platforms = application.clone();
    resulting_application_platforms.extend(application_platforms_added.iter().cloned());
    Ok(PlatformPolicy {
        channel: channel.to_owned(),
        application_platforms: application.into_iter().collect(),
        release_platforms: release.into_iter().collect(),
        partial_release: !missing_from_release.is_empty(),
        missing_from_release,
        additional_to_application,
        application_platforms_added,
        resulting_application_platforms: resulting_application_platforms.into_iter().collect(),
    })
}

async fn sign_bootstrap_application(
    input: ApplicationInput,
    target: &ApplicationTarget,
    signer: &std::sync::Arc<ngit::NgitSigner>,
) -> Result<SoftwareApplication> {
    let builder = application_event_builder(input).map_err(|error| {
        coded_error_with_details(
            "invalid_application_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    let unsigned = finalize_ordered_unsigned(builder, target.author, None)
        .context("failed to order initial software application")?;
    let event = sign_draft_event(
        unsigned,
        signer,
        "publish software application for release".to_owned(),
    )
    .await?;
    let application = SoftwareApplication::parse(&event)
        .context("signed software application failed validation")?;
    if application.coordinate() != target.coordinate() {
        return Err(coded_error_with_details(
            "application_author_mismatch",
            "signer returned a software application for a different coordinate",
            json!({
                "expected_application": super::support::coordinate_key(&target.coordinate()),
                "signed_application": super::support::coordinate_key(&application.coordinate()),
            }),
        ));
    }
    Ok(application)
}

async fn sign_application_platform_update(
    existing: &SoftwareApplication,
    platforms: &[String],
    signer: &std::sync::Arc<ngit::NgitSigner>,
) -> Result<SoftwareApplication> {
    let mut input = ApplicationInput::from(existing);
    input.platforms = platforms.to_vec();
    sign_application_update(existing, input, signer).await
}

async fn sign_application_update(
    existing: &SoftwareApplication,
    input: ApplicationInput,
    signer: &std::sync::Arc<ngit::NgitSigner>,
) -> Result<SoftwareApplication> {
    let builder = application_event_builder(input).map_err(|error| {
        coded_error_with_details(
            "invalid_application_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    let unsigned = finalize_ordered_unsigned(
        builder,
        existing.raw_event.pubkey,
        Some(&existing.raw_event),
    )
    .context("failed to order software application platform replacement")?;
    let event = sign_draft_event(
        unsigned,
        signer,
        "add software application platforms for release".to_owned(),
    )
    .await?;
    let application = SoftwareApplication::parse(&event)
        .context("signed software application platform replacement failed validation")?;
    if application.coordinate() != existing.coordinate() {
        return Err(coded_error(
            "application_author_mismatch",
            "signer returned a platform replacement for a different application",
        ));
    }
    Ok(application)
}

fn bootstrap_application_target(
    context: &ReleaseContext,
    discovered: &[SoftwareApplication],
    selector: Option<&str>,
) -> Result<ApplicationTarget> {
    let author = context
        .current_signer()
        .ok_or_else(|| coded_error("not_logged_in", "release publication requires login"))?;
    if !context.repo_ref.is_authorized_maintainer(&author) {
        return Err(coded_error(
            "not_repository_maintainer",
            "the active signer is not a current repository maintainer",
        ));
    }
    let identifier = if let Some(selector) = selector {
        let coordinate = Nip19Coordinate::from_bech32(selector)
            .ok()
            .map(|pointer| pointer.coordinate)
            .or_else(|| Coordinate::parse(selector).ok());
        if let Some(coordinate) = coordinate {
            if coordinate.kind != SOFTWARE_APPLICATION_KIND || coordinate.public_key != author {
                return Err(coded_error(
                    "application_not_found",
                    "a missing application coordinate can only be created for the active signer and kind 32267",
                ));
            }
            coordinate.identifier
        } else {
            selector.to_owned()
        }
    } else {
        context.repo_ref.identifier.clone()
    };
    if identifier.is_empty() {
        return Err(coded_error(
            "metadata_confirmation_required",
            "the repository identifier is empty; create the application explicitly with release app init --id",
        ));
    }
    if let Some(application) = discovered.iter().find(|application| {
        application.raw_event.pubkey == author && application.identifier == identifier
    }) {
        return Err(coded_error_with_details(
            "application_not_linked",
            format!(
                "software application {identifier:?} already exists but is not linked to this repository; link it explicitly before publishing"
            ),
            json!({ "event_id": application.raw_event.id.to_hex() }),
        ));
    }
    Ok(ApplicationTarget { author, identifier })
}

fn select_application<'a>(
    applications: &'a [SoftwareApplication],
    selector: Option<&str>,
) -> Result<&'a SoftwareApplication> {
    if let Some(selector) = selector {
        return resolve_application(applications, selector);
    }
    match applications {
        [application] => Ok(application),
        [] => Err(coded_error(
            "application_not_found",
            "no trusted application is linked to this repository; initialize or link one first",
        )),
        _ => Err(coded_error_with_details(
            "ambiguous_selector",
            "multiple applications are linked; choose one with --app",
            json!({
                "applications": applications.iter().map(|application| application.identifier.clone()).collect::<Vec<_>>()
            }),
        )),
    }
}

fn enforce_edit_guard(
    existing: Option<&SoftwareRelease>,
    edit: bool,
    entity: &str,
    identifier: &str,
) -> Result<()> {
    match (existing.is_some(), edit) {
        (true, false) => Err(coded_error(
            "release_already_exists",
            format!("{entity} {identifier:?} already exists; pass --edit to replace it"),
        )),
        (false, true) => Err(coded_error(
            "edit_target_not_found",
            format!("cannot edit missing {entity} {identifier:?}"),
        )),
        _ => Ok(()),
    }
}

#[derive(Debug)]
struct ResolvedReleaseInvocation {
    release_version: String,
    tagged_commit: Option<String>,
    manifest: Option<ResolvedReleaseManifest>,
}

fn resolve_release_invocation(
    repo: &Repo,
    args: &ReleasePublishArgs,
) -> Result<ResolvedReleaseInvocation> {
    let repository_root = repo.get_path()?;
    let has_direct_assets = !args.assets.is_empty()
        || !args.files.is_empty()
        || !args.asset_events.is_empty()
        || !args.platform_agnostic_assets.is_empty()
        || !args.platform_agnostic_files.is_empty();
    let requested = args.manifest.as_deref();
    let should_load = requested.is_some()
        || (!args.edit
            && !has_direct_assets
            && resolve_release_manifest_path(repository_root, None)?.exists());
    let loaded = should_load
        .then(|| load_release_manifest(repository_root, requested))
        .transpose()?;
    let requested_commit = args.commit.as_deref().or_else(|| {
        loaded
            .as_ref()
            .and_then(|loaded| loaded.manifest.commit.as_deref())
    });

    let (release_version, tag, tagged_commit) = match (&args.release_version, &args.tag) {
        (Some(version), None) => (version.clone(), None, None),
        (Some(version), Some(tag)) => {
            let tag_commit = resolve_exact_tag(&repo.git_repo, tag)?;
            ensure_requested_commit_matches_tag(&repo.git_repo, requested_commit, tag, tag_commit)?;
            (
                version.clone(),
                Some(tag.clone()),
                Some(tag_commit.to_string()),
            )
        }
        (None, explicit_tag) => {
            let selected_commit = match (requested_commit, explicit_tag) {
                (Some(revision), _) => resolve_repository_commit(&repo.git_repo, revision)?,
                (None, Some(tag)) => resolve_exact_tag(&repo.git_repo, tag)?,
                (None, None) => resolve_repository_commit(&repo.git_repo, "HEAD")?,
            };
            let tag = if let Some(tag) = explicit_tag {
                let tag_commit = resolve_exact_tag(&repo.git_repo, tag)?;
                if tag_commit != selected_commit {
                    return Err(coded_error_with_details(
                        "release_tag_commit_mismatch",
                        format!("Git tag {tag:?} does not identify the selected release commit"),
                        json!({
                            "tag": tag,
                            "tag_commit": tag_commit.to_string(),
                            "selected_commit": selected_commit.to_string(),
                        }),
                    ));
                }
                tag.clone()
            } else {
                exact_tag_for_commit(&repo.git_repo, selected_commit)?
            };
            let version = version_from_tag(&tag)?;
            (version, Some(tag), Some(selected_commit.to_string()))
        }
    };

    let manifest = loaded
        .map(|loaded| loaded.manifest.resolve(&release_version, tag.as_deref()))
        .transpose()?;
    Ok(ResolvedReleaseInvocation {
        release_version,
        tagged_commit,
        manifest,
    })
}

fn resolve_repository_commit(repository: &git2::Repository, revision: &str) -> Result<git2::Oid> {
    repository
        .revparse_single(revision)
        .with_context(|| format!("failed to resolve release commit {revision:?}"))?
        .peel_to_commit()
        .with_context(|| format!("release commit {revision:?} does not identify a Git commit"))
        .map(|commit| commit.id())
}

fn resolve_exact_tag(repository: &git2::Repository, tag: &str) -> Result<git2::Oid> {
    let reference_name = format!("refs/tags/{tag}");
    repository
        .find_reference(&reference_name)
        .with_context(|| format!("failed to find exact Git tag {tag:?}"))?
        .peel_to_commit()
        .with_context(|| format!("Git tag {tag:?} does not identify a commit"))
        .map(|commit| commit.id())
}

fn ensure_requested_commit_matches_tag(
    repository: &git2::Repository,
    requested_commit: Option<&str>,
    tag: &str,
    tag_commit: git2::Oid,
) -> Result<()> {
    let Some(requested_commit) = requested_commit else {
        return Ok(());
    };
    let selected_commit = resolve_repository_commit(repository, requested_commit)?;
    if selected_commit == tag_commit {
        return Ok(());
    }
    Err(coded_error_with_details(
        "release_tag_commit_mismatch",
        format!("Git tag {tag:?} does not identify the selected release commit"),
        json!({
            "tag": tag,
            "tag_commit": tag_commit.to_string(),
            "selected_commit": selected_commit.to_string(),
        }),
    ))
}

fn exact_tag_for_commit(repository: &git2::Repository, commit: git2::Oid) -> Result<String> {
    let mut tags = Vec::new();
    let references = repository
        .references_glob("refs/tags/*")
        .context("failed to enumerate Git tags")?;
    for reference in references {
        let reference = reference.context("failed to read a Git tag")?;
        let Some(name) = reference
            .name()
            .ok()
            .and_then(|name| name.strip_prefix("refs/tags/"))
            .map(str::to_owned)
        else {
            continue;
        };
        if reference
            .peel_to_commit()
            .is_ok_and(|tag_commit| tag_commit.id() == commit)
        {
            tags.push(name);
        }
    }
    tags.sort();
    match tags.as_slice() {
        [] => Err(coded_error_with_details(
            "release_tag_required",
            "the selected release commit has no exact Git tag; provide VERSION or tag the commit",
            json!({ "commit": commit.to_string() }),
        )),
        [tag] => Ok(tag.clone()),
        _ => Err(coded_error_with_details(
            "release_tag_ambiguous",
            "the selected release commit has multiple exact Git tags; select one with --tag",
            json!({ "commit": commit.to_string(), "tags": tags }),
        )),
    }
}

fn version_from_tag(tag: &str) -> Result<String> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() {
        return Err(coded_error(
            "invalid_release_tag",
            "Git tag \"v\" does not contain a release version",
        ));
    }
    Ok(version.to_owned())
}

fn effective_publication_settings(
    args: &ReleasePublishArgs,
    manifest: Option<&ResolvedReleaseManifest>,
) -> EffectivePublicationSettings {
    let manifest_publication = manifest.map(|manifest| &manifest.publication);
    let (blossom_servers, blossom_server_source) = if args.blossom_servers.is_empty() {
        let servers = manifest_publication
            .map(|publication| publication.blossom_servers.clone())
            .unwrap_or_default();
        let source = if servers.is_empty() {
            "explicit"
        } else {
            "manifest"
        };
        (servers, source)
    } else {
        (args.blossom_servers.clone(), "explicit")
    };

    let mut relays = manifest_publication
        .map(|publication| publication.relays.clone())
        .unwrap_or_default();
    for relay in &args.relays {
        if !relays.contains(relay) {
            relays.push(relay.clone());
        }
    }

    EffectivePublicationSettings {
        blossom_servers,
        blossom_server_source,
        relays,
        zapstore_relay: args.zapstore_relay
            || manifest_publication.is_some_and(|publication| publication.zapstore_relay),
        strict_metadata: args.strict_metadata
            || manifest_publication.is_some_and(|publication| publication.strict_metadata),
        allow_partial_platforms: args.allow_partial_platforms
            || manifest_publication.is_some_and(|publication| publication.allow_partial_platforms),
        add_application_platforms: args.add_application_platforms
            || manifest_publication
                .is_some_and(|publication| publication.add_application_platforms),
    }
}

fn release_notes(
    context: &ReleaseContext,
    args: &ReleasePublishArgs,
    manifest: Option<&ResolvedReleaseManifest>,
    existing: Option<&SoftwareRelease>,
    release_version: &str,
) -> Result<String> {
    if let Some(notes) = &args.notes {
        return Ok(notes.clone());
    }
    if let Some(path) = &args.notes_file {
        let path = repository_relative_path(context.git_repo_path()?, path);
        return fs::read_to_string(&path)
            .with_context(|| format!("failed to read release notes {}", path.display()));
    }
    if let Some(manifest) = manifest {
        if let Some(notes) = &manifest.notes {
            return Ok(notes.clone());
        }
        if let Some(path) = &manifest.release_notes {
            let path = repository_relative_path(context.git_repo_path()?, path);
            let changelog = fs::read_to_string(&path)
                .with_context(|| format!("failed to read release_notes {}", path.display()))?;
            return extract_keep_a_changelog_release_notes(&changelog, release_version)
                .with_context(|| {
                    format!("failed to extract release notes from {}", path.display())
                });
        }
    }
    Ok(existing
        .map(|release| release.notes.clone())
        .unwrap_or_default())
}

fn repository_relative_path(repository: &Path, path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repository.join(path)
    }
}

async fn require_all_assets(
    context: &mut ReleaseContext,
    release: &SoftwareRelease,
) -> Result<Vec<SoftwareAsset>> {
    let resolved = load_assets(context, &[release], true).await?;
    let by_id: HashMap<_, _> = resolved
        .into_iter()
        .map(|asset| (asset.raw_event.id, asset))
        .collect();
    let mut ordered = Vec::with_capacity(release.assets.len());
    for pointer in &release.assets {
        let asset = by_id.get(&pointer.event_id).ok_or_else(|| {
            coded_error_with_details(
                "release_asset_unresolved",
                format!(
                    "release references asset {} which could not be resolved and validated",
                    pointer.event_id
                ),
                json!({ "event_id": pointer.event_id.to_hex() }),
            )
        })?;
        ordered.push(asset.clone());
    }
    Ok(ordered)
}

async fn load_asset_event(
    context: &mut ReleaseContext,
    selector: &str,
    strict: bool,
) -> Result<SoftwareAsset> {
    let event_id = parse_event_id(selector).map_err(|_| {
        coded_error(
            "asset_not_found",
            format!("asset selector {selector:?} must be an event ID or nevent"),
        )
    })?;
    let events = context
        .query(vec![Filter::new().id(event_id)], strict)
        .await?;
    let event = events
        .iter()
        .find(|event| event.id == event_id)
        .ok_or_else(|| {
            coded_error(
                "asset_not_found",
                format!("software asset {selector:?} was not found"),
            )
        })?;
    SoftwareAsset::parse(event).map_err(|error| {
        coded_error_with_details(
            "invalid_asset_metadata",
            error.to_string(),
            json!({ "event_id": event.id.to_hex(), "issues": error.issues }),
        )
    })
}

async fn load_exact_release(
    context: &mut ReleaseContext,
    application: &ApplicationTarget,
    version: &str,
) -> Result<Option<SoftwareRelease>> {
    use ngit::software_release::SOFTWARE_RELEASE_KIND;

    let identifier = format!("{}@{version}", application.identifier);
    context.add_author_relays(application.author).await?;
    let events = context
        .query(
            vec![
                Filter::new()
                    .kind(SOFTWARE_RELEASE_KIND)
                    .author(application.author)
                    .identifier(&identifier),
            ],
            true,
        )
        .await?;
    let Some(event) = ngit::event_ordering::latest_event(events.iter()) else {
        return Ok(None);
    };
    let release = SoftwareRelease::parse(event).map_err(|error| {
        coded_error_with_details(
            "invalid_release_metadata",
            format!("existing software release {identifier:?} is invalid: {error}"),
            json!({
                "event_id": event.id.to_hex(),
                "validation": error.issues,
            }),
        )
    })?;
    if release.application.coordinate != application.coordinate() {
        return Err(coded_error_with_details(
            "release_author_mismatch",
            "existing release does not reference the selected application",
            json!({
                "event_id": release.raw_event.id.to_hex(),
                "application": super::support::coordinate_key(&release.application.coordinate),
            }),
        ));
    }
    Ok(Some(release))
}

async fn load_exact_application(
    context: &mut ReleaseContext,
    application: &ApplicationTarget,
) -> Result<Option<SoftwareApplication>> {
    use ngit::software_release::SOFTWARE_APPLICATION_KIND;

    let events = context
        .query(
            vec![
                Filter::new()
                    .kind(SOFTWARE_APPLICATION_KIND)
                    .author(application.author)
                    .identifier(&application.identifier),
            ],
            true,
        )
        .await?;
    let Some(event) = ngit::event_ordering::latest_event(events.iter()) else {
        return Ok(None);
    };
    SoftwareApplication::parse(event)
        .map(Some)
        .map_err(|error| {
            coded_error_with_details(
                "invalid_application_metadata",
                format!(
                    "software application {:?} became invalid: {error}",
                    application.identifier
                ),
                json!({
                    "event_id": event.id.to_hex(),
                    "validation": error.issues,
                }),
            )
        })
}

async fn ensure_release_state_unchanged(
    context: &mut ReleaseContext,
    application: &ApplicationTarget,
    expected_application: Option<&SoftwareApplication>,
    version: &str,
    expected_release: Option<&SoftwareRelease>,
) -> Result<()> {
    context.refresh_repository().await?;
    if context.current_signer() != Some(application.author)
        || !context
            .repo_ref
            .is_authorized_maintainer(&application.author)
    {
        return Err(coded_error(
            "concurrent_state_changed",
            "application publication authority changed during preflight; inspect the repository and retry",
        ));
    }
    let current_application = load_exact_application(context, application).await?;
    let expected_application_id = expected_application.map(|application| application.raw_event.id);
    let current_application_id = current_application
        .as_ref()
        .map(|application| application.raw_event.id);
    if current_application_id != expected_application_id {
        return Err(coded_error_with_details(
            "concurrent_state_changed",
            "the software application changed during preflight; inspect it and retry",
            json!({
                "expected_event_id": expected_application_id.map(|event_id| event_id.to_hex()),
                "current_event_id": current_application_id.map(|event_id| event_id.to_hex()),
            }),
        ));
    }
    if let Some(current_application) = &current_application {
        context.require_application_author(current_application)?;
    }

    let current_release = load_exact_release(context, application, version).await?;
    let expected_id = expected_release.map(|release| release.raw_event.id);
    let current_id = current_release.as_ref().map(|release| release.raw_event.id);
    if current_id != expected_id {
        return Err(coded_error_with_details(
            "concurrent_state_changed",
            "the software release changed during preflight; inspect it and retry",
            json!({
                "expected_event_id": expected_id.map(|event_id| event_id.to_hex()),
                "current_event_id": current_id.map(|event_id| event_id.to_hex()),
            }),
        ));
    }
    Ok(())
}

fn validate_reused_asset(
    application: &ApplicationTarget,
    asset: &SoftwareAsset,
    platform_agnostic_acknowledged: bool,
) -> Result<()> {
    if asset.raw_event.pubkey != application.author {
        return Err(coded_error_with_details(
            "invalid_asset_author",
            "asset author does not match the application author",
            json!({
                "asset_author": asset.raw_event.pubkey.to_hex(),
                "application_author": application.author.to_hex(),
            }),
        ));
    }
    let expected_application = application.coordinate();
    if let Some(asset_application) = asset
        .application
        .as_ref()
        .filter(|pointer| pointer.coordinate != expected_application)
    {
        return Err(coded_error_with_details(
            "invalid_asset_application",
            "asset does not reference the selected application",
            json!({
                "asset_application": super::support::coordinate_key(&asset_application.coordinate),
                "expected_application": super::support::coordinate_key(&expected_application),
            }),
        ));
    }
    if asset.platforms.is_empty() && !platform_agnostic_acknowledged {
        return Err(coded_error(
            "asset_platform_required",
            "asset has no platform metadata; explicitly acknowledge it as platform-agnostic",
        ));
    }
    Ok(())
}

fn reject_duplicate_asset(existing: &[SoftwareAsset], proposed: &SoftwareAsset) -> Result<()> {
    if existing
        .iter()
        .any(|asset| asset.raw_event.id == proposed.raw_event.id)
    {
        return Err(coded_error(
            "duplicate_asset",
            format!("asset {} is already attached", proposed.raw_event.id),
        ));
    }
    if let Some(filename) = proposed.filename.as_deref() {
        if existing
            .iter()
            .any(|asset| asset.filename.as_deref() == Some(filename))
        {
            return Err(coded_error(
                "duplicate_asset_filename",
                format!("an attached asset already uses filename {filename:?}"),
            ));
        }
    }
    Ok(())
}

fn reject_duplicate_prepared_asset(
    existing: &[SoftwareAsset],
    prepared: &[PreparedAsset],
    proposed: &AssetInput,
) -> Result<()> {
    let Some(filename) = proposed.filename.as_deref() else {
        return Ok(());
    };
    if existing
        .iter()
        .any(|asset| asset.filename.as_deref() == Some(filename))
        || prepared
            .iter()
            .any(|asset| asset.input().filename.as_deref() == Some(filename))
    {
        return Err(coded_error(
            "duplicate_asset_filename",
            format!("an attached or proposed asset already uses filename {filename:?}"),
        ));
    }
    Ok(())
}

fn reject_asset_against_prepared(
    prepared: &[PreparedAsset],
    proposed: &SoftwareAsset,
) -> Result<()> {
    let Some(filename) = proposed.filename.as_deref() else {
        return Ok(());
    };
    if prepared
        .iter()
        .any(|asset| asset.input().filename.as_deref() == Some(filename))
    {
        return Err(coded_error(
            "duplicate_asset_filename",
            format!("a proposed asset already uses filename {filename:?}"),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct NewUrlAsset {
    source: String,
    application_coordinate: Coordinate,
    identifier: String,
    version: String,
    filename: Option<String>,
    mime: Option<String>,
    platforms: Vec<String>,
    min_platform_version: Option<String>,
    target_platform_version: Option<String>,
    supported_nips: Vec<String>,
    variant: Option<String>,
    commit: Option<String>,
    min_allowed_version: Option<String>,
    version_code: Option<u64>,
    min_allowed_version_code: Option<u64>,
    apk_certificate_hashes: Vec<String>,
    original_url: Option<String>,
}

impl NewUrlAsset {
    fn simple(
        source: &str,
        platforms: Vec<String>,
        application: &ApplicationTarget,
        release_version: &str,
    ) -> Self {
        Self {
            source: source.to_owned(),
            application_coordinate: application.coordinate(),
            identifier: application.identifier.clone(),
            version: release_version.to_owned(),
            filename: None,
            mime: None,
            platforms,
            min_platform_version: None,
            target_platform_version: None,
            supported_nips: Vec::new(),
            variant: None,
            commit: None,
            min_allowed_version: None,
            version_code: None,
            min_allowed_version_code: None,
            apk_certificate_hashes: Vec::new(),
            original_url: None,
        }
    }

    fn from_manifest(
        asset: &ResolvedReleaseManifestAsset,
        source: String,
        application: &ApplicationTarget,
        release_version: &str,
    ) -> Self {
        Self {
            source,
            application_coordinate: application.coordinate(),
            identifier: asset
                .identifier
                .clone()
                .unwrap_or_else(|| application.identifier.clone()),
            version: asset
                .version
                .clone()
                .unwrap_or_else(|| release_version.to_owned()),
            filename: asset.filename.clone(),
            mime: asset.mime.clone(),
            platforms: asset.platforms.clone(),
            min_platform_version: asset.min_platform_version.clone(),
            target_platform_version: asset.target_platform_version.clone(),
            supported_nips: asset.supported_nips.clone(),
            variant: asset.variant.clone(),
            commit: asset.commit.clone(),
            min_allowed_version: asset.min_allowed_version.clone(),
            version_code: asset
                .android
                .as_ref()
                .and_then(|android| android.version_code),
            min_allowed_version_code: asset
                .android
                .as_ref()
                .and_then(|android| android.min_allowed_version_code),
            apk_certificate_hashes: asset
                .android
                .as_ref()
                .map_or_else(Vec::new, |android| android.certificate_sha256.clone()),
            original_url: asset.original_url.clone(),
        }
    }
}

fn asset_add_metadata(
    args: &ReleaseAssetAddArgs,
    source: String,
    application: &SoftwareApplication,
    release: &SoftwareRelease,
) -> NewUrlAsset {
    NewUrlAsset {
        source,
        application_coordinate: application.coordinate(),
        identifier: args
            .asset_id
            .clone()
            .unwrap_or_else(|| application.identifier.clone()),
        version: args
            .asset_version
            .clone()
            .unwrap_or_else(|| release.version.clone()),
        filename: args.filename.clone(),
        mime: args.mime.clone(),
        platforms: args.platforms.clone(),
        min_platform_version: args.min_platform_version.clone(),
        target_platform_version: args.target_platform_version.clone(),
        supported_nips: args.supported_nips.clone(),
        variant: args.variant.clone(),
        commit: args.commit.clone(),
        min_allowed_version: args.min_allowed_version.clone(),
        version_code: args.android_version_code,
        min_allowed_version_code: args.android_min_allowed_version_code,
        apk_certificate_hashes: args.android_certificate_sha256.clone(),
        original_url: args.original_url.clone(),
    }
}

#[derive(Debug)]
struct NewFileAsset {
    source_path: std::path::PathBuf,
    metadata: NewUrlAsset,
    platform_agnostic: bool,
}

fn grouped_file_arguments(
    values: &[String],
    bare_platforms: &[String],
) -> Result<Vec<(std::path::PathBuf, Vec<String>)>> {
    let mut grouped: Vec<(std::path::PathBuf, Vec<String>)> = Vec::new();
    let mut bare_path = None;
    for value in values {
        let Some((platform, path)) = value.split_once('=') else {
            if value.trim().is_empty() {
                return Err(coded_error(
                    "invalid_file_argument",
                    "--file path must not be empty",
                ));
            }
            if bare_path.replace(std::path::PathBuf::from(value)).is_some() {
                return Err(coded_error(
                    "ambiguous_file_platforms",
                    "repeatable --platform can describe only one bare --file PATH; use PLATFORM=PATH or a release manifest for multiple files",
                ));
            }
            continue;
        };
        if platform.trim().is_empty() || path.trim().is_empty() {
            return Err(coded_error(
                "invalid_file_argument",
                format!("file {value:?} must contain a platform and path"),
            ));
        }

        let path = std::path::PathBuf::from(path);
        if let Some((_, platforms)) = grouped
            .iter_mut()
            .find(|(existing_path, _)| existing_path == &path)
        {
            if !platforms.iter().any(|existing| existing == platform) {
                platforms.push(platform.to_owned());
            }
        } else {
            grouped.push((path, vec![platform.to_owned()]));
        }
    }

    if let Some(path) = bare_path {
        if !grouped.is_empty() {
            return Err(coded_error(
                "ambiguous_file_platforms",
                "do not mix a bare --file PATH with PLATFORM=PATH; use one form or a release manifest",
            ));
        }
        let mut platforms = Vec::new();
        for platform in bare_platforms {
            if platform.trim().is_empty() {
                return Err(coded_error(
                    "invalid_file_argument",
                    "--platform must not be empty",
                ));
            }
            if !platforms.contains(platform) {
                platforms.push(platform.clone());
            }
        }
        grouped.push((path, platforms));
    } else if !bare_platforms.is_empty() {
        return Err(coded_error(
            "ambiguous_file_platforms",
            "--platform requires exactly one bare --file PATH",
        ));
    }
    Ok(grouped)
}

impl NewFileAsset {
    fn simple(
        source_path: &Path,
        platforms: Vec<String>,
        application: &ApplicationTarget,
        release_version: &str,
        platform_agnostic: bool,
    ) -> Self {
        Self {
            source_path: source_path.to_path_buf(),
            metadata: NewUrlAsset::simple("", platforms, application, release_version),
            platform_agnostic,
        }
    }

    fn from_manifest(
        asset: &ResolvedReleaseManifestAsset,
        source_path: &Path,
        application: &ApplicationTarget,
        release_version: &str,
    ) -> Self {
        Self {
            source_path: source_path.to_path_buf(),
            platform_agnostic: asset.platform_agnostic,
            metadata: NewUrlAsset::from_manifest(
                asset,
                String::new(),
                application,
                release_version,
            ),
        }
    }
}

#[derive(Debug)]
struct PendingFileAsset {
    source_path: String,
    input: AssetInput,
    snapshot: FileSnapshot,
    apk_platform_inference: Option<Box<ApkInspection>>,
}

#[derive(Debug)]
enum PreparedAsset {
    Ready(AssetInput),
    File(PendingFileAsset),
}

impl PreparedAsset {
    fn input(&self) -> &AssetInput {
        match self {
            Self::Ready(input) => input,
            Self::File(pending) => &pending.input,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct BlossomServerSelection {
    source: &'static str,
    event_id: Option<String>,
    author: String,
    servers: Vec<Url>,
}

async fn prepare_file_asset(
    context: &mut ReleaseContext,
    mut proposed: NewFileAsset,
) -> Result<PendingFileAsset> {
    let source_path = repository_relative_path(context.git_repo_path()?, &proposed.source_path);
    let mut request = LocalFileRequest::new(&source_path);
    request.filename = proposed.metadata.filename.clone();
    request.mime_type = proposed.metadata.mime.clone();
    let snapshot = snapshot_local_file(request).await.with_context(|| {
        format!(
            "failed to snapshot release asset {}",
            proposed.source_path.display()
        )
    })?;
    append_download_warnings(context, &snapshot.warnings)?;

    let apk_platform_inference = infer_apk_metadata(
        context,
        &snapshot,
        &proposed.source_path,
        &mut proposed.metadata,
        proposed.platform_agnostic,
    )?;
    if proposed.metadata.platforms.is_empty()
        && apk_platform_inference.is_none()
        && !proposed.platform_agnostic
    {
        return Err(coded_error(
            "asset_platform_required",
            "local file has no platform metadata; use repeatable --platform, PLATFORM=PATH, or --platform-agnostic-file",
        ));
    }

    let input = AssetInput {
        application: Some(AddressPointer {
            coordinate: proposed.metadata.application_coordinate,
            relay_hint: context.repo_ref.relays.first().map(ToString::to_string),
        }),
        identifier: proposed.metadata.identifier,
        version: proposed.metadata.version,
        url: None,
        filename: Some(snapshot.filename.clone()),
        mime: snapshot.mime_type.clone(),
        sha256: snapshot.sha256.clone(),
        size: Some(snapshot.size),
        platforms: proposed.metadata.platforms,
        min_platform_version: proposed.metadata.min_platform_version,
        target_platform_version: proposed.metadata.target_platform_version,
        supported_nips: proposed.metadata.supported_nips,
        variant: proposed.metadata.variant,
        commit: proposed.metadata.commit,
        min_allowed_version: proposed.metadata.min_allowed_version,
        version_code: proposed.metadata.version_code,
        min_allowed_version_code: proposed.metadata.min_allowed_version_code,
        apk_certificate_hashes: proposed.metadata.apk_certificate_hashes,
        original_url: proposed.metadata.original_url,
        extra_tags: Vec::new(),
        created_at: None,
    };
    validate_asset_input(&input)?;
    Ok(PendingFileAsset {
        source_path: proposed.source_path.display().to_string(),
        input,
        snapshot,
        apk_platform_inference: apk_platform_inference.map(Box::new),
    })
}

fn infer_apk_metadata(
    context: &mut ReleaseContext,
    snapshot: &FileSnapshot,
    source_path: &Path,
    metadata: &mut NewUrlAsset,
    platform_agnostic: bool,
) -> Result<Option<ApkInspection>> {
    let source_is_apk = looks_like_apk_filename(&source_path.to_string_lossy());
    let filename_is_apk = looks_like_apk_filename(&snapshot.filename);
    let mime_is_apk = snapshot.mime_type.eq_ignore_ascii_case(APK_MIME_TYPE);
    if !source_is_apk && !filename_is_apk && !mime_is_apk {
        return Ok(None);
    }
    if !mime_is_apk {
        return Err(coded_error_with_details(
            "apk_platform_conflict",
            "the local asset looks like an APK but its resolved MIME type is not the Android package MIME type",
            json!({
                "source": source_path.display().to_string(),
                "filename": snapshot.filename,
                "resolved_mime": snapshot.mime_type,
                "expected_mime": APK_MIME_TYPE,
            }),
        ));
    }
    if platform_agnostic {
        return Err(coded_error(
            "apk_platform_conflict",
            "Android APK assets cannot be platform agnostic",
        ));
    }

    let inference = inspect_apk(snapshot).map_err(|error| {
        coded_error_with_details(
            "invalid_apk",
            format!(
                "cannot inspect Android metadata in {}: {error:#}",
                snapshot.filename
            ),
            json!({
                "source": source_path.display().to_string(),
                "filename": snapshot.filename,
            }),
        )
    })?;
    merge_apk_platforms(metadata, &inference.platforms)?;
    apply_apk_identity_metadata(metadata, &inference)?;
    if !inference.platforms.unknown_abis.is_empty() {
        context.warnings.push(WarningJson::new(
            "apk_unknown_abi",
            format!(
                "the APK contains unrecognized native ABI directories: {}",
                inference.platforms.unknown_abis.join(", ")
            ),
        ));
    }
    Ok(Some(inference))
}

fn merge_apk_platforms(metadata: &mut NewUrlAsset, inference: &ApkPlatformInference) -> Result<()> {
    let derived = inference
        .derived_platforms
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let conflicting = metadata
        .platforms
        .iter()
        .filter(|platform| {
            if inference.native_libraries_present {
                !derived.contains(platform.as_str())
            } else {
                !platform.starts_with("android-")
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    if !conflicting.is_empty() {
        return Err(coded_error_with_details(
            "apk_platform_conflict",
            "declared platforms contradict the platforms supported by the APK",
            json!({
                "declared_platforms": metadata.platforms,
                "derived_platforms": inference.derived_platforms,
                "conflicting_platforms": conflicting,
                "native_libraries_present": inference.native_libraries_present,
            }),
        ));
    }

    metadata
        .platforms
        .extend(inference.derived_platforms.iter().cloned());
    metadata.platforms.sort();
    metadata.platforms.dedup();
    Ok(())
}

fn apply_apk_identity_metadata(
    metadata: &mut NewUrlAsset,
    inference: &ApkInspection,
) -> Result<()> {
    assert_apk_string_metadata("identifier", &metadata.identifier, &inference.package)?;
    assert_apk_string_metadata("version", &metadata.version, &inference.version_name)?;
    assert_optional_apk_string_metadata(
        "min_platform_version",
        metadata.min_platform_version.as_deref(),
        &inference.min_sdk_version,
    )?;
    assert_optional_apk_string_metadata(
        "target_platform_version",
        metadata.target_platform_version.as_deref(),
        &inference.target_sdk_version,
    )?;
    if metadata
        .version_code
        .is_some_and(|declared| declared != inference.version_code)
    {
        return Err(apk_metadata_conflict(
            "android.version_code",
            metadata.version_code,
            inference.version_code,
        ));
    }
    let mut declared_certificate_hashes = metadata.apk_certificate_hashes.clone();
    declared_certificate_hashes.sort();
    if !declared_certificate_hashes.is_empty()
        && declared_certificate_hashes != inference.certificate_sha256
    {
        return Err(apk_metadata_conflict(
            "android.certificate_sha256",
            &metadata.apk_certificate_hashes,
            &inference.certificate_sha256,
        ));
    }

    metadata.identifier.clone_from(&inference.package);
    metadata.version.clone_from(&inference.version_name);
    metadata.min_platform_version = Some(inference.min_sdk_version.clone());
    metadata.target_platform_version = Some(inference.target_sdk_version.clone());
    metadata.version_code = Some(inference.version_code);
    metadata
        .apk_certificate_hashes
        .clone_from(&inference.certificate_sha256);
    Ok(())
}

fn assert_apk_string_metadata(field: &str, declared: &str, extracted: &str) -> Result<()> {
    if declared == extracted {
        return Ok(());
    }
    Err(apk_metadata_conflict(field, declared, extracted))
}

fn assert_optional_apk_string_metadata(
    field: &str,
    declared: Option<&str>,
    extracted: &str,
) -> Result<()> {
    let Some(declared) = declared else {
        return Ok(());
    };
    assert_apk_string_metadata(field, declared, extracted)
}

fn apk_metadata_conflict(
    field: &str,
    declared: impl Serialize,
    extracted: impl Serialize,
) -> anyhow::Error {
    coded_error_with_details(
        "apk_metadata_conflict",
        format!("declared {field} contradicts the value extracted from the APK"),
        json!({
            "field": field,
            "declared": declared,
            "extracted": extracted,
        }),
    )
}

fn can_infer_local_apk_platforms(
    file: Option<&Path>,
    filename: Option<&str>,
    mime: Option<&str>,
) -> bool {
    file.is_some_and(|path| {
        looks_like_apk_filename(&path.to_string_lossy())
            || filename.is_some_and(looks_like_apk_filename)
            || mime.is_some_and(|mime| mime.eq_ignore_ascii_case(APK_MIME_TYPE))
    })
}

fn looks_like_apk_filename(value: &str) -> bool {
    value.to_ascii_lowercase().ends_with(".apk")
}

async fn resolve_blossom_server_selection(
    context: &mut ReleaseContext,
    application: &ApplicationTarget,
    explicit_servers: &[String],
    explicit_source: &'static str,
    required: bool,
) -> Result<Option<BlossomServerSelection>> {
    if !required {
        if !explicit_servers.is_empty() {
            return Err(coded_error(
                "blossom_server_without_file",
                "--blossom-server requires a local file",
            ));
        }
        return Ok(None);
    }

    let author = application.author;
    if !explicit_servers.is_empty() {
        let mut servers = Vec::new();
        for value in explicit_servers {
            let server = canonicalize_blossom_server_root(value).map_err(|error| {
                coded_error_with_details(
                    "invalid_blossom_server",
                    error.to_string(),
                    json!({ "server": value }),
                )
            })?;
            if !servers.contains(&server) {
                servers.push(server);
            }
        }
        return Ok(Some(BlossomServerSelection {
            source: explicit_source,
            event_id: None,
            author: author.to_hex(),
            servers,
        }));
    }

    context.add_author_relays(author).await?;
    let events = context
        .query_with_required_discovery_route(vec![blossom_server_list_filter(author)])
        .await?;
    let BlossomServerList {
        event_id, servers, ..
    } = blossom_server_list_from_events(author, &events).map_err(|error| {
        coded_error_with_details(
            "blossom_servers_not_found",
            format!("{error}; provide --blossom-server to override discovery"),
            json!({ "author": author.to_hex() }),
        )
    })?;
    Ok(Some(BlossomServerSelection {
        source: "kind_10063",
        event_id: Some(event_id.to_hex()),
        author: author.to_hex(),
        servers,
    }))
}

#[derive(Debug)]
struct BlossomPublication {
    json: Value,
    outcomes: Vec<Vec<BlossomServerOutcome>>,
    possible_orphan_blobs: Vec<PossibleOrphanBlob>,
}

impl BlossomPublication {
    fn empty() -> Self {
        Self {
            json: json!({
                "server_selection": null,
                "uploads": [],
            }),
            outcomes: Vec::new(),
            possible_orphan_blobs: Vec::new(),
        }
    }

    fn has_uploads(&self) -> bool {
        !self.outcomes.is_empty()
    }

    fn incomplete_replication_warning(&self) -> Option<WarningJson> {
        blossom_replication_warning(self.outcomes.iter().map(Vec::as_slice))
    }
}

#[derive(Clone, Copy)]
struct Nip82Progress<'a> {
    signed_application_id: Option<&'a EventId>,
    signed_asset_ids: &'a [EventId],
    release_event_signed: bool,
    publication_complete: bool,
}

impl<'a> Nip82Progress<'a> {
    fn none() -> Self {
        Self {
            signed_application_id: None,
            signed_asset_ids: &[],
            release_event_signed: false,
            publication_complete: false,
        }
    }

    fn application(signed_application_id: Option<&'a EventId>) -> Self {
        Self {
            signed_application_id,
            ..Self::none()
        }
    }

    fn assets(signed_application_id: Option<&'a EventId>, signed_asset_ids: &'a [EventId]) -> Self {
        Nip82Progress {
            signed_application_id,
            signed_asset_ids,
            release_event_signed: false,
            publication_complete: false,
        }
    }

    fn release(
        signed_application_id: Option<&'a EventId>,
        signed_asset_ids: &'a [EventId],
    ) -> Self {
        Nip82Progress {
            signed_application_id,
            signed_asset_ids,
            release_event_signed: true,
            publication_complete: false,
        }
    }
}

async fn upload_prepared_files(
    prepared_application: Option<&mut PreparedApplicationMetadata>,
    prepared_assets: &mut [PreparedAsset],
    selection: &BlossomServerSelection,
    signer: &std::sync::Arc<ngit::NgitSigner>,
    json_output: bool,
) -> Result<BlossomPublication> {
    let snapshots = prepared_upload_snapshots(prepared_application.as_deref(), prepared_assets)?;

    let progress = BlossomUploadProgress::new(json_output)?;
    let batch = upload_release_snapshot_batch_to_servers_with_progress(
        &selection.servers,
        &snapshots,
        signer,
        DEFAULT_UPLOAD_CONCURRENCY,
        progress,
    )
    .await;
    let batch = match batch {
        Ok(batch) => batch,
        Err(error) => {
            drop(snapshots);
            return Err(blossom_batch_failure(
                prepared_application.as_deref(),
                prepared_assets,
                selection,
                error,
            ));
        }
    };

    let (confirmed_by_hash, possible_orphan_blobs) = confirmed_batch_uploads(&snapshots, &batch)?;
    drop(snapshots);

    let mut uploads = Vec::new();
    let mut outcomes = Vec::new();
    if let Some(prepared) = prepared_application {
        for index in 0..prepared.local_media.len() {
            let (slot, url, upload_json, server_outcomes) = {
                let pending = &prepared.local_media[index];
                let upload = confirmed_by_hash
                    .get(&pending.snapshot.sha256)
                    .context("prepared application media was not in the Blossom batch")?;
                (
                    pending.slot,
                    upload.primary.url.to_string(),
                    application_media_upload_json(pending, upload),
                    upload.servers.clone(),
                )
            };
            uploads.push(upload_json);
            outcomes.push(server_outcomes);
            prepared.set_media_url(slot, url);
        }
        if let Err(error) = validate_application_input(&prepared.input) {
            let publication = BlossomPublication {
                json: blossom_json(selection, &uploads),
                outcomes,
                possible_orphan_blobs,
            };
            return Err(preserve_completed_blossom(
                error,
                &publication,
                "application_metadata",
                Nip82Progress::none(),
            ));
        }
    }
    for prepared in prepared_assets {
        let PreparedAsset::File(pending) = prepared else {
            continue;
        };
        let upload = confirmed_by_hash
            .get(&pending.snapshot.sha256)
            .context("prepared release asset was not in the Blossom batch")?;
        pending.input.url = Some(upload.primary.url.to_string());
        uploads.push(blossom_upload_json(pending, upload));
        outcomes.push(upload.servers.clone());
        if let Err(error) = validate_asset_input(&pending.input) {
            let publication = BlossomPublication {
                json: blossom_json(selection, &uploads),
                outcomes,
                possible_orphan_blobs,
            };
            return Err(preserve_completed_blossom(
                error,
                &publication,
                "asset_metadata",
                Nip82Progress::none(),
            ));
        }
    }
    Ok(BlossomPublication {
        json: blossom_json(selection, &uploads),
        outcomes,
        possible_orphan_blobs,
    })
}

fn prepared_upload_snapshots<'a>(
    prepared_application: Option<&'a PreparedApplicationMetadata>,
    prepared_assets: &'a [PreparedAsset],
) -> Result<Vec<&'a FileSnapshot>> {
    let mut seen_hashes = HashMap::new();
    let mut snapshots = Vec::new();
    if let Some(prepared) = prepared_application {
        for pending in &prepared.local_media {
            include_batch_snapshot(&pending.snapshot, &mut seen_hashes, &mut snapshots)?;
        }
    }
    for prepared in prepared_assets {
        let PreparedAsset::File(pending) = prepared else {
            continue;
        };
        include_batch_snapshot(&pending.snapshot, &mut seen_hashes, &mut snapshots)?;
    }
    Ok(snapshots)
}

fn confirmed_batch_uploads(
    snapshots: &[&FileSnapshot],
    batch: &BatchUploadResult,
) -> Result<(HashMap<String, MultiServerUpload>, Vec<PossibleOrphanBlob>)> {
    let batch_by_hash = batch
        .blobs
        .iter()
        .map(|blob| (blob.sha256.as_str(), blob))
        .collect::<HashMap<_, _>>();
    let mut confirmed_by_hash = HashMap::new();
    let mut possible_orphan_blobs = Vec::new();
    for snapshot in snapshots {
        let outcome = batch_by_hash
            .get(snapshot.sha256.as_str())
            .context("Blossom batch omitted a prepared release file")?;
        let upload = multi_server_upload_from_batch_outcome(snapshot, outcome)
            .map_err(anyhow::Error::new)?;
        possible_orphan_blobs.extend(possible_orphans_from_upload(snapshot, &upload));
        confirmed_by_hash.insert(snapshot.sha256.clone(), upload);
    }
    Ok((confirmed_by_hash, possible_orphan_blobs))
}

fn include_batch_snapshot<'a>(
    snapshot: &'a FileSnapshot,
    seen_hashes: &mut HashMap<&'a str, &'a str>,
    snapshots: &mut Vec<&'a FileSnapshot>,
) -> Result<()> {
    if let Some(previous_mime) = seen_hashes.get(snapshot.sha256.as_str()) {
        if *previous_mime != snapshot.mime_type {
            return Err(coded_error_with_details(
                "conflicting_blossom_metadata",
                "local files with the same SHA-256 must use the same MIME type",
                json!({
                    "sha256": snapshot.sha256,
                    "first_mime": previous_mime,
                    "conflicting_mime": snapshot.mime_type,
                }),
            ));
        }
        return Ok(());
    }
    seen_hashes.insert(snapshot.sha256.as_str(), snapshot.mime_type.as_str());
    snapshots.push(snapshot);
    Ok(())
}

fn blossom_batch_failure(
    prepared_application: Option<&PreparedApplicationMetadata>,
    prepared_assets: &[PreparedAsset],
    selection: &BlossomServerSelection,
    error: BatchUploadError,
) -> anyhow::Error {
    let BatchUploadError {
        message,
        blobs,
        possible_orphan_blobs,
    } = error;
    let by_hash = blobs
        .iter()
        .map(|blob| (blob.sha256.as_str(), blob))
        .collect::<HashMap<_, _>>();
    let mut uploads = Vec::new();
    if let Some(prepared) = prepared_application {
        for pending in &prepared.local_media {
            let servers = by_hash
                .get(pending.snapshot.sha256.as_str())
                .map_or(&[][..], |blob| blob.servers.as_slice());
            uploads.push(failed_application_media_upload_json(pending, servers));
        }
    }
    for prepared in prepared_assets {
        let PreparedAsset::File(pending) = prepared else {
            continue;
        };
        let servers = by_hash
            .get(pending.snapshot.sha256.as_str())
            .map_or(&[][..], |blob| blob.servers.as_slice());
        uploads.push(failed_blossom_upload_json(pending, servers));
    }
    let outcomes = blobs
        .iter()
        .map(|blob| blob.servers.clone())
        .collect::<Vec<_>>();
    let outcome_labels = blobs
        .iter()
        .map(|blob| {
            let filename = uploads
                .iter()
                .find(|upload| upload["sha256"].as_str() == Some(blob.sha256.as_str()))
                .and_then(|upload| upload["filename"].as_str())
                .unwrap_or("unnamed blob")
                .to_owned();
            BlossomBlobLabel {
                filename,
                sha256: blob.sha256.clone(),
            }
        })
        .collect::<Vec<_>>();
    let (stage, server) = failed_blossom_operation(&outcomes);
    let human_message = blossom_failure_message(
        &message,
        &outcomes,
        Some(&outcome_labels),
        &possible_orphan_blobs,
        Nip82Progress::none(),
        BLOSSOM_RETRY_RECOVERY,
    );
    coded_error_with_details(
        "blossom_publication_failed",
        human_message,
        json!({
            "stage": stage,
            "server": server,
            "blossom": blossom_json(selection, &uploads),
            "possible_orphan_blobs": possible_orphan_blobs,
            "release_events_signed": false,
            "release_events_published": false,
            "recovery": BLOSSOM_RETRY_RECOVERY,
        }),
    )
}

fn blossom_json(selection: &BlossomServerSelection, uploads: &[Value]) -> Value {
    json!({
        "server_selection": selection,
        "uploads": uploads,
    })
}

fn blossom_upload_json(pending: &PendingFileAsset, upload: &MultiServerUpload) -> Value {
    json!({
        "source": pending.source_path,
        "filename": pending.snapshot.filename,
        "sha256": pending.snapshot.sha256,
        "size": pending.snapshot.size.to_string(),
        "mime": pending.snapshot.mime_type,
        "apk_platform_inference": pending.apk_platform_inference,
        "primary_url": upload.primary.url,
        "servers": upload.servers.iter().map(blossom_server_outcome_json).collect::<Vec<_>>(),
    })
}

fn application_media_upload_json(
    pending: &PendingApplicationMedia,
    upload: &MultiServerUpload,
) -> Value {
    json!({
        "entity": "application_media",
        "field": application_media_field(pending.slot),
        "source": pending.source_path,
        "filename": pending.snapshot.filename,
        "sha256": pending.snapshot.sha256,
        "size": pending.snapshot.size.to_string(),
        "mime": pending.snapshot.mime_type,
        "primary_url": upload.primary.url,
        "servers": upload.servers.iter().map(blossom_server_outcome_json).collect::<Vec<_>>(),
    })
}

fn failed_application_media_upload_json(
    pending: &PendingApplicationMedia,
    servers: &[BlossomServerOutcome],
) -> Value {
    json!({
        "entity": "application_media",
        "field": application_media_field(pending.slot),
        "source": pending.source_path,
        "filename": pending.snapshot.filename,
        "sha256": pending.snapshot.sha256,
        "size": pending.snapshot.size.to_string(),
        "mime": pending.snapshot.mime_type,
        "primary_url": Value::Null,
        "servers": servers.iter().map(blossom_server_outcome_json).collect::<Vec<_>>(),
    })
}

fn application_media_field(slot: ApplicationMediaSlot) -> String {
    match slot {
        ApplicationMediaSlot::Icon => "icon".to_owned(),
        ApplicationMediaSlot::Image(index) => format!("images[{index}]"),
    }
}

fn failed_blossom_upload_json(
    pending: &PendingFileAsset,
    servers: &[BlossomServerOutcome],
) -> Value {
    json!({
        "source": pending.source_path,
        "filename": pending.snapshot.filename,
        "sha256": pending.snapshot.sha256,
        "size": pending.snapshot.size.to_string(),
        "mime": pending.snapshot.mime_type,
        "apk_platform_inference": pending.apk_platform_inference,
        "primary_url": servers.first().and_then(|outcome| outcome.descriptor.as_ref()).map(|descriptor| descriptor.url.as_str()),
        "servers": servers.iter().map(blossom_server_outcome_json).collect::<Vec<_>>(),
    })
}

fn blossom_server_outcome_json(outcome: &BlossomServerOutcome) -> Value {
    json!({
        "server": outcome.server.as_str(),
        "operation": outcome.operation,
        "status": outcome.status,
        "url": outcome.descriptor.as_ref().map(|descriptor| descriptor.url.as_str()),
        "message": outcome.message.as_deref(),
    })
}

fn failed_blossom_operation(outcomes: &[Vec<BlossomServerOutcome>]) -> (Value, Value) {
    outcomes
        .iter()
        .flatten()
        .find(|outcome| {
            matches!(
                outcome.status,
                BlossomServerStatus::Failed | BlossomServerStatus::Unknown
            )
        })
        .map_or((Value::Null, Value::Null), |outcome| {
            (json!(outcome.operation), json!(outcome.server))
        })
}

fn blossom_status_label(status: BlossomServerStatus) -> &'static str {
    match status {
        BlossomServerStatus::Stored => "stored",
        BlossomServerStatus::AlreadyPresent => "already_present",
        BlossomServerStatus::Failed => "failed",
        BlossomServerStatus::Unknown => "unknown",
        BlossomServerStatus::NotAttempted => "not_attempted",
    }
}

fn possible_orphans_from_upload(
    snapshot: &FileSnapshot,
    upload: &MultiServerUpload,
) -> Vec<PossibleOrphanBlob> {
    upload
        .servers
        .iter()
        .filter(|outcome| outcome.status == BlossomServerStatus::Stored)
        .map(|outcome| PossibleOrphanBlob {
            server: outcome.server.clone(),
            sha256: snapshot.sha256.clone(),
            url: outcome
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.url.clone()),
        })
        .collect()
}

struct BlossomBlobLabel {
    filename: String,
    sha256: String,
}

fn blossom_failure_message(
    message: &str,
    outcomes: &[Vec<BlossomServerOutcome>],
    labels: Option<&[BlossomBlobLabel]>,
    possible_orphan_blobs: &[PossibleOrphanBlob],
    progress: Nip82Progress<'_>,
    recovery: &str,
) -> String {
    let server_outcomes = labels.map_or_else(
        || {
            outcomes
                .iter()
                .flatten()
                .map(format_blossom_server_outcome)
                .collect::<Vec<_>>()
                .join("; ")
        },
        |labels| {
            outcomes
                .iter()
                .enumerate()
                .map(|(index, servers)| {
                    let label = labels.get(index);
                    let filename = label.map_or("unnamed blob", |label| label.filename.as_str());
                    let sha256 = label.map_or("unknown hash", |label| label.sha256.as_str());
                    let confirmed = servers
                        .iter()
                        .filter(|outcome| {
                            matches!(
                                outcome.status,
                                BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent
                            )
                        })
                        .count();
                    let availability = if confirmed == 0 {
                        "NO CONFIRMED COPY".to_owned()
                    } else {
                        format!("{confirmed}/{} confirmed", servers.len())
                    };
                    let placements = servers
                        .iter()
                        .map(|outcome| format!("    {}", format_blossom_server_outcome(outcome)))
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("  {filename} ({sha256}): {availability}\n{placements}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
    );
    let possible_orphans = if possible_orphan_blobs.is_empty() {
        "none".to_owned()
    } else {
        possible_orphan_blobs
            .iter()
            .map(|orphan| {
                let filename = labels
                    .and_then(|labels| labels.iter().find(|label| label.sha256 == orphan.sha256))
                    .map(|label| format!("{}: ", label.filename))
                    .unwrap_or_default();
                orphan.url.as_ref().map_or_else(
                    || {
                        format!(
                            "{filename}{} hash {} (URL unknown)",
                            orphan.server, orphan.sha256
                        )
                    },
                    |url| format!("{filename}{url} hash {}", orphan.sha256),
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ")
    };
    let signed_asset_ids = if progress.signed_asset_ids.is_empty() {
        "none".to_owned()
    } else {
        progress
            .signed_asset_ids
            .iter()
            .map(EventId::to_hex)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let signed_application_id = progress
        .signed_application_id
        .map_or_else(|| "none".to_owned(), EventId::to_hex);
    format!(
        "{message}\nserver outcomes:\n{server_outcomes}\npossible orphan blobs:\n  {possible_orphans}\nsigned application ID: {signed_application_id}\nsigned asset IDs: {signed_asset_ids}\nrelease event signed: {}\npublication complete: {}\nrecovery: {recovery}",
        progress.release_event_signed, progress.publication_complete
    )
}

fn format_blossom_server_outcome(outcome: &BlossomServerOutcome) -> String {
    let operation = match outcome.operation {
        BlossomServerOperation::Upload => "upload",
        BlossomServerOperation::Mirror => "mirror",
    };
    let status = blossom_status_label(outcome.status);
    outcome.message.as_ref().map_or_else(
        || format!("{operation} {}: {status}", outcome.server),
        |message| format!("{operation} {}: {status} ({message})", outcome.server),
    )
}

fn preserve_completed_blossom(
    error: anyhow::Error,
    blossom: &BlossomPublication,
    stage: &'static str,
    progress: Nip82Progress<'_>,
) -> anyhow::Error {
    if !blossom.has_uploads() {
        return error;
    }
    let (code, message, details) = match error.downcast::<ReleaseError>() {
        Ok(error) => (error.code, error.message, error.details),
        Err(error) => (
            "operation_failed_after_blossom",
            format!("{error:#}"),
            json!({}),
        ),
    };
    let mut details = match details {
        Value::Object(details) => details,
        details => serde_json::Map::from_iter([("cause".to_owned(), details)]),
    };
    details.insert("stage".to_owned(), json!(stage));
    details.insert("blossom".to_owned(), blossom.json.clone());
    details.insert(
        "possible_orphan_blobs".to_owned(),
        json!(blossom.possible_orphan_blobs),
    );
    details.insert(
        "signed_application_id".to_owned(),
        progress
            .signed_application_id
            .map_or(Value::Null, |event_id| json!(event_id.to_hex())),
    );
    details.insert(
        "signed_asset_ids".to_owned(),
        json!(
            progress
                .signed_asset_ids
                .iter()
                .map(EventId::to_hex)
                .collect::<Vec<_>>()
        ),
    );
    details.insert(
        "release_event_signed".to_owned(),
        json!(progress.release_event_signed),
    );
    details.insert(
        "publication_complete".to_owned(),
        json!(progress.publication_complete),
    );
    details.insert(
        "blossom_recovery".to_owned(),
        json!(BLOSSOM_DOWNSTREAM_RECOVERY),
    );
    let message = blossom_failure_message(
        &message,
        &blossom.outcomes,
        None,
        &blossom.possible_orphan_blobs,
        progress,
        BLOSSOM_DOWNSTREAM_RECOVERY,
    );
    coded_error_with_details(code, message, Value::Object(details))
}

async fn prepare_url_asset(
    context: &mut ReleaseContext,
    proposed: NewUrlAsset,
) -> Result<AssetInput> {
    let mut request = UrlAssetRequest::new(&proposed.source);
    request.filename = proposed.filename.clone();
    request.mime_type = proposed.mime.clone();
    let downloaded = download_url_asset(request).await.with_context(|| {
        format!(
            "failed to acquire release asset {}",
            redacted_url(&proposed.source)
        )
    })?;
    append_download_warnings(context, &downloaded.warnings)?;
    let input = AssetInput {
        application: Some(AddressPointer {
            coordinate: proposed.application_coordinate,
            relay_hint: context.repo_ref.relays.first().map(ToString::to_string),
        }),
        identifier: proposed.identifier,
        version: proposed.version,
        url: Some(downloaded.source_url),
        filename: Some(downloaded.filename),
        mime: downloaded.mime_type,
        sha256: downloaded.sha256,
        size: Some(downloaded.size),
        platforms: proposed.platforms,
        min_platform_version: proposed.min_platform_version,
        target_platform_version: proposed.target_platform_version,
        supported_nips: proposed.supported_nips,
        variant: proposed.variant,
        commit: proposed.commit,
        min_allowed_version: proposed.min_allowed_version,
        version_code: proposed.version_code,
        min_allowed_version_code: proposed.min_allowed_version_code,
        apk_certificate_hashes: proposed.apk_certificate_hashes,
        original_url: proposed.original_url,
        extra_tags: Vec::new(),
        created_at: None,
    };
    validate_asset_input(&input)?;
    Ok(input)
}

fn append_download_warnings(
    context: &mut ReleaseContext,
    warnings: &[ngit::release_download::DownloadWarning],
) -> Result<()> {
    for warning in warnings {
        let code = serde_json::to_value(warning.code)?
            .as_str()
            .unwrap_or("asset_download_warning")
            .to_owned();
        context
            .warnings
            .push(WarningJson::new(code, warning.message.clone()));
    }
    Ok(())
}

fn validate_asset_input(input: &AssetInput) -> Result<()> {
    if input.mime.eq_ignore_ascii_case(APK_MIME_TYPE) && input.platforms.is_empty() {
        return Err(coded_error_with_details(
            "apk_platform_conflict",
            "Android APK assets require platform metadata; only local APKs can infer it",
            json!({
                "mime": input.mime,
                "platforms": input.platforms,
            }),
        ));
    }
    asset_event_builder(input.clone()).map_err(|error| {
        coded_error_with_details(
            "invalid_asset_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    Ok(())
}

async fn sign_asset_input(
    input: AssetInput,
    signer: &std::sync::Arc<ngit::NgitSigner>,
) -> Result<SoftwareAsset> {
    let builder = asset_event_builder(input).map_err(|error| {
        coded_error_with_details(
            "invalid_asset_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    let event = sign_event(builder, signer, "software release asset".to_owned()).await?;
    Ok(SoftwareAsset::parse(&event)?)
}

#[allow(clippy::too_many_arguments)]
async fn build_release_event(
    context: &ReleaseContext,
    application: &SoftwareApplication,
    version: &str,
    channel: String,
    notes: String,
    commit: Option<String>,
    assets: &[SoftwareAsset],
    existing: Option<&SoftwareRelease>,
    released_at: Timestamp,
    signer: &std::sync::Arc<ngit::NgitSigner>,
) -> Result<Event> {
    let relay_hint = context.repo_ref.relays.first().map(ToString::to_string);
    let application_pointer = existing.map_or_else(
        || AddressPointer {
            coordinate: application.coordinate(),
            relay_hint: relay_hint.clone(),
        },
        |release| release.application.clone(),
    );
    let existing_hints: HashMap<_, _> = existing
        .into_iter()
        .flat_map(|release| release.assets.iter())
        .map(|asset| (asset.event_id, asset.relay_hint.clone()))
        .collect();
    let asset_pointers = assets
        .iter()
        .map(|asset| ReleaseAssetInput {
            event_id: asset.raw_event.id,
            author: asset.raw_event.pubkey,
            relay_hint: existing_hints
                .get(&asset.raw_event.id)
                .cloned()
                .flatten()
                .or_else(|| relay_hint.clone()),
            platforms: asset.platforms.clone(),
        })
        .collect();
    let builder = release_event_builder(ReleaseInput {
        application: application_pointer,
        version: version.to_owned(),
        channel,
        notes,
        assets: asset_pointers,
        commit,
        extra_tags: existing.map_or_else(Vec::new, |release| release.extra_tags.clone()),
        released_at,
    })
    .map_err(|error| {
        coded_error_with_details(
            "invalid_release_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    if let Some(existing) = existing {
        let unsigned = finalize_fixed_timestamp_ordered_unsigned(
            builder,
            application.raw_event.pubkey,
            Some(&existing.raw_event),
            released_at,
        )
        .map_err(|error| {
            coded_error_with_details(
                "replacement_ordering_exhausted",
                format!("failed to order the release replacement: {error}"),
                json!({
                    "previous_event_id": existing.raw_event.id.to_hex(),
                    "released_at": released_at.as_secs(),
                }),
            )
        })?;
        sign_draft_event(unsigned, signer, "software release replacement".to_owned()).await
    } else {
        sign_event(builder, signer, "software release".to_owned()).await
    }
}

fn release_commit(
    context: &ReleaseContext,
    args: &ReleasePublishArgs,
    manifest: Option<&ResolvedReleaseManifest>,
    existing: Option<&SoftwareRelease>,
    tagged_commit: Option<&str>,
) -> Result<Option<String>> {
    let requested = args
        .commit
        .as_deref()
        .or_else(|| manifest.and_then(|manifest| manifest.commit.as_deref()));
    if let Some(tagged_commit) = tagged_commit {
        return Ok(Some(tagged_commit.to_owned()));
    }
    if let Some(revision) = requested {
        return resolve_git_commit(context, revision).map(Some);
    }
    if let Some(existing) = existing {
        return Ok(existing.commit.clone());
    }
    resolve_git_commit(context, "HEAD").map(Some)
}

fn resolve_git_commit(context: &ReleaseContext, revision: &str) -> Result<String> {
    context
        .git_repo
        .git_repo
        .revparse_single(revision)
        .with_context(|| format!("failed to resolve release commit {revision:?}"))?
        .peel_to_commit()
        .with_context(|| format!("release commit {revision:?} does not identify a Git commit"))
        .map(|commit| commit.id().to_string())
}

fn enforce_metadata_policy(context: &ReleaseContext, strict: bool) -> Result<()> {
    let metadata_warnings = context
        .warnings
        .iter()
        .filter(|warning| is_metadata_warning(&warning.code))
        .collect::<Vec<_>>();
    if strict && !metadata_warnings.is_empty() {
        return Err(coded_error_with_details(
            "metadata_confirmation_required",
            "metadata warnings must be resolved before publishing with --strict-metadata",
            json!({ "warnings": metadata_warnings }),
        ));
    }
    Ok(())
}

fn is_metadata_warning(code: &str) -> bool {
    matches!(
        code,
        "application_metadata_incomplete"
            | "release_notes_missing"
            | "filename_sanitized"
            | "filename_hint_ignored"
            | "filename_fallback"
            | "mime_parameters_ignored"
            | "invalid_mime_hint"
            | "mime_conflict"
            | "generic_mime"
            | "apk_unknown_abi"
    )
}

fn redacted_url(value: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(value) else {
        return "<invalid URL>".to_owned();
    };
    if url.query().is_some() {
        url.set_query(Some("REDACTED"));
    }
    url.set_fragment(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use git2::{Oid, Repository, Signature};
    use tempfile::TempDir;

    use super::*;

    fn values(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn error_code(error: &anyhow::Error) -> &'static str {
        error
            .downcast_ref::<super::super::support::ReleaseError>()
            .expect("platform policy returned an uncoded error")
            .code
    }

    fn publish_args(values: &[&str]) -> ReleasePublishArgs {
        let cli = crate::cli::Cli::try_parse_from(
            ["ngit", "release", "publish"]
                .into_iter()
                .chain(values.iter().copied()),
        )
        .expect("valid release publish arguments");
        let Some(crate::cli::Commands::Release(release)) = cli.command else {
            panic!("release command was not parsed");
        };
        let crate::cli::ReleaseCommands::Publish(args) = release.release_command else {
            panic!("release publish command was not parsed");
        };
        args
    }

    fn repository_with_commit() -> (TempDir, Repo, Oid) {
        let directory = tempfile::tempdir().unwrap();
        let repository = Repository::init(directory.path()).unwrap();
        let commit = {
            let mut index = repository.index().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repository.find_tree(tree_id).unwrap();
            let signature = Signature::now("ngit test", "ngit@example.com").unwrap();
            repository
                .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                .unwrap()
        };
        (
            directory,
            Repo {
                git_repo: repository,
            },
            commit,
        )
    }

    fn add_lightweight_tag(repository: &Repository, tag: &str, commit: Oid) {
        let object = repository.find_object(commit, None).unwrap();
        repository.tag_lightweight(tag, &object, false).unwrap();
    }

    #[test]
    fn publish_without_version_uses_the_single_exact_tag() {
        let (_directory, repo, commit) = repository_with_commit();
        add_lightweight_tag(&repo.git_repo, "v1.2.3", commit);

        let invocation = resolve_release_invocation(&repo, &publish_args(&[])).unwrap();

        assert_eq!(invocation.release_version, "1.2.3");
        assert_eq!(invocation.tagged_commit, Some(commit.to_string()));
    }

    #[test]
    fn explicit_publish_version_remains_available_without_a_tag() {
        let (_directory, repo, _commit) = repository_with_commit();

        let invocation =
            resolve_release_invocation(&repo, &publish_args(&["custom-version"])).unwrap();

        assert_eq!(invocation.release_version, "custom-version");
        assert!(invocation.tagged_commit.is_none());
    }

    #[test]
    fn automatic_version_requires_one_unambiguous_exact_tag() {
        let (_directory, repo, commit) = repository_with_commit();
        let missing = resolve_release_invocation(&repo, &publish_args(&[])).unwrap_err();
        assert_eq!(error_code(&missing), "release_tag_required");

        add_lightweight_tag(&repo.git_repo, "v1.2.3", commit);
        add_lightweight_tag(&repo.git_repo, "stable", commit);
        let ambiguous = resolve_release_invocation(&repo, &publish_args(&[])).unwrap_err();
        assert_eq!(error_code(&ambiguous), "release_tag_ambiguous");

        let selected =
            resolve_release_invocation(&repo, &publish_args(&["--tag", "v1.2.3"])).unwrap();
        assert_eq!(selected.release_version, "1.2.3");
    }

    #[test]
    fn manifest_publication_settings_are_ci_defaults_with_cli_precedence() {
        let manifest = ngit::release_manifest::parse_release_manifest(
            r#"
schema: 1
publication:
  blossom_servers:
    - https://manifest-primary.example.com
    - https://manifest-mirror.example.com
  relays:
    - wss://manifest.example.com
  zapstore_relay: true
  allow_partial_platforms: true
assets:
  - source: https://downloads.example.com/app
    platform_agnostic: true
"#,
        )
        .unwrap()
        .resolve("1.0.0", None)
        .unwrap();
        let args = publish_args(&[
            "1.0.0",
            "--blossom-server",
            "https://cli.example.com",
            "--relay",
            "wss://manifest.example.com",
            "--relay",
            "wss://cli.example.com",
            "--strict-metadata",
            "--add-application-platforms",
        ]);

        assert_eq!(
            effective_publication_settings(&args, Some(&manifest)),
            EffectivePublicationSettings {
                blossom_servers: values(&["https://cli.example.com"]),
                blossom_server_source: "explicit",
                relays: values(&["wss://manifest.example.com", "wss://cli.example.com"]),
                zapstore_relay: true,
                strict_metadata: true,
                allow_partial_platforms: true,
                add_application_platforms: true,
            }
        );
    }

    #[test]
    fn manifest_blossom_servers_are_reported_as_manifest_selected() {
        let manifest = ngit::release_manifest::parse_release_manifest(
            r#"
schema: 1
publication:
  blossom_servers: [https://manifest.example.com]
assets:
  - file: dist/app
    platforms: [linux-x86_64]
"#,
        )
        .unwrap()
        .resolve("1.0.0", None)
        .unwrap();
        let settings = effective_publication_settings(&publish_args(&["1.0.0"]), Some(&manifest));

        assert_eq!(
            settings.blossom_servers,
            values(&["https://manifest.example.com"])
        );
        assert_eq!(settings.blossom_server_source, "manifest");
    }

    #[test]
    fn strict_metadata_ignores_transport_and_relay_warnings() {
        assert!(is_metadata_warning("release_notes_missing"));
        assert!(is_metadata_warning("mime_conflict"));
        assert!(!is_metadata_warning("redirected"));
        assert!(!is_metadata_warning("non_public_host"));
        assert!(!is_metadata_warning("relay_discovery_incomplete"));
    }

    #[test]
    fn only_local_apks_are_candidates_for_platform_inference() {
        assert!(can_infer_local_apk_platforms(
            Some(Path::new("artifact.bin")),
            Some("application.apk"),
            None,
        ));
        assert!(can_infer_local_apk_platforms(
            Some(Path::new("artifact.bin")),
            None,
            Some(APK_MIME_TYPE),
        ));
        assert!(!can_infer_local_apk_platforms(
            None,
            Some("application.apk"),
            Some(APK_MIME_TYPE),
        ));
    }

    #[test]
    fn apk_identity_is_extracted_and_declarations_are_only_assertions() {
        let application = ApplicationTarget {
            author: nostr::prelude::Keys::generate().public_key(),
            identifier: "dev.ngit.fixture".to_owned(),
        };
        let mut metadata = NewUrlAsset::simple("", Vec::new(), &application, "9.9.9");
        let inspection = ApkInspection {
            platforms: ApkPlatformInference {
                derived_platforms: values(&["android-arm64-v8a"]),
                native_libraries_present: true,
                unknown_abis: Vec::new(),
            },
            package: "dev.ngit.fixture".to_owned(),
            version_name: "1.2.3".to_owned(),
            version_code: 10_203,
            min_sdk_version: "24".to_owned(),
            target_sdk_version: "35".to_owned(),
            certificate_sha256: values(&["abcdef"]),
        };

        let error = apply_apk_identity_metadata(&mut metadata, &inspection).unwrap_err();
        let release_error = error
            .downcast_ref::<super::super::support::ReleaseError>()
            .expect("APK conflict should be a coded release error");
        assert_eq!(release_error.code, "apk_metadata_conflict");
        assert_eq!(release_error.details["field"], "version");
        assert_eq!(release_error.details["declared"], "9.9.9");
        assert_eq!(release_error.details["extracted"], "1.2.3");

        metadata.version = inspection.version_name.clone();
        apply_apk_identity_metadata(&mut metadata, &inspection).unwrap();
        assert_eq!(metadata.version_code, Some(10_203));
        assert_eq!(metadata.min_platform_version.as_deref(), Some("24"));
        assert_eq!(metadata.target_platform_version.as_deref(), Some("35"));
        assert_eq!(metadata.apk_certificate_hashes, ["abcdef"]);
    }

    #[test]
    fn bare_file_accepts_repeatable_platforms() {
        let grouped = grouped_file_arguments(
            &values(&["dist/app"]),
            &values(&["linux-x86_64", "linux-aarch64", "linux-x86_64"]),
        )
        .unwrap();
        assert_eq!(
            grouped,
            vec![(
                std::path::PathBuf::from("dist/app"),
                values(&["linux-x86_64", "linux-aarch64"]),
            )]
        );
    }

    #[test]
    fn repeated_shorthand_path_becomes_one_multiplatform_file() {
        let grouped = grouped_file_arguments(
            &values(&["linux-x86_64=dist/app", "linux-aarch64=dist/app"]),
            &[],
        )
        .unwrap();
        assert_eq!(
            grouped,
            vec![(
                std::path::PathBuf::from("dist/app"),
                values(&["linux-x86_64", "linux-aarch64"]),
            )]
        );
    }

    #[test]
    fn platforms_reject_ambiguous_file_forms() {
        for (files, platforms) in [
            (values(&["dist/a", "dist/b"]), values(&["linux-x86_64"])),
            (
                values(&["dist/a", "linux-aarch64=dist/b"]),
                values(&["linux-x86_64"]),
            ),
            (values(&["linux-x86_64=dist/a"]), values(&["linux-aarch64"])),
        ] {
            let error = grouped_file_arguments(&files, &platforms).unwrap_err();
            assert_eq!(error_code(&error), "ambiguous_file_platforms");
        }
    }

    #[test]
    fn main_releases_must_cover_the_application_platforms() {
        let mut warnings = Vec::new();
        let error = enforce_platform_policy(
            &mut warnings,
            &values(&["linux-x86_64", "windows-x86_64"]),
            &values(&["linux-x86_64"]),
            "main",
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), "release_platform_coverage_incomplete");
        assert!(warnings.is_empty());
    }

    #[test]
    fn main_release_extras_require_the_additive_application_edit() {
        let mut warnings = Vec::new();
        let application = values(&["linux-x86_64"]);
        let release = values(&["linux-aarch64", "linux-x86_64"]);
        let error =
            enforce_platform_policy(&mut warnings, &application, &release, "main", false, false)
                .unwrap_err();
        assert_eq!(error_code(&error), "application_platform_update_required");

        let policy =
            enforce_platform_policy(&mut warnings, &application, &release, "main", false, true)
                .unwrap();
        assert_eq!(
            policy.application_platforms_added,
            values(&["linux-aarch64"])
        );
        assert_eq!(policy.resulting_application_platforms, release);
    }

    #[test]
    fn non_main_subsets_are_allowed_only_with_an_explicit_warning() {
        let application = values(&["linux-x86_64", "windows-x86_64"]);
        let release = values(&["linux-x86_64"]);
        let mut warnings = Vec::new();
        let error =
            enforce_platform_policy(&mut warnings, &application, &release, "beta", false, false)
                .unwrap_err();
        assert_eq!(error_code(&error), "partial_platform_confirmation_required");

        let policy =
            enforce_platform_policy(&mut warnings, &application, &release, "beta", true, false)
                .unwrap();
        assert!(policy.partial_release);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "partial_platform_release");
    }

    #[test]
    fn downstream_failures_retain_truthful_blossom_progress_for_humans_and_json() -> Result<()> {
        let server = Url::parse("https://blossom.example/")?;
        let orphan_url = Url::parse(
            "https://blossom.example/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.zip",
        )?;
        let signed_application = EventId::from_hex(&"22".repeat(32))?;
        let signed_asset = EventId::from_hex(&"11".repeat(32))?;
        let publication = BlossomPublication {
            json: json!({ "server_selection": {}, "uploads": [{}] }),
            outcomes: vec![vec![BlossomServerOutcome {
                server: server.clone(),
                operation: BlossomServerOperation::Upload,
                status: BlossomServerStatus::Stored,
                descriptor: None,
                message: None,
                presence_check_only: false,
            }]],
            possible_orphan_blobs: vec![PossibleOrphanBlob {
                server,
                sha256: "aa".repeat(32),
                url: Some(orphan_url.clone()),
            }],
        };

        let error = preserve_completed_blossom(
            coded_error(
                "release_signing_failed",
                "remote signer refused the release",
            ),
            &publication,
            "release_signing",
            Nip82Progress::assets(
                Some(&signed_application),
                std::slice::from_ref(&signed_asset),
            ),
        );
        let error = error
            .downcast::<ReleaseError>()
            .expect("coded release error");

        assert_eq!(error.code, "release_signing_failed");
        assert_eq!(
            error.details["signed_application_id"],
            signed_application.to_hex()
        );
        assert_eq!(error.details["signed_asset_ids"][0], signed_asset.to_hex());
        assert_eq!(error.details["release_event_signed"], false);
        assert_eq!(error.details["publication_complete"], false);
        assert!(error.message.contains(orphan_url.as_str()));
        assert!(error.message.contains(&signed_application.to_hex()));
        assert!(error.message.contains(&signed_asset.to_hex()));
        assert!(
            error
                .message
                .contains("upload https://blossom.example/: stored")
        );
        assert!(error.message.contains("recovery:"));
        Ok(())
    }

    #[test]
    fn blossom_failure_message_includes_server_failure_detail() -> Result<()> {
        let server = Url::parse("https://blossom.example/")?;
        let message = blossom_failure_message(
            "one or more Blossom uploads failed",
            &[vec![BlossomServerOutcome {
                server,
                operation: BlossomServerOperation::Upload,
                status: BlossomServerStatus::Failed,
                descriptor: None,
                message: Some("HTTP 413 Payload Too Large: quota exceeded".to_owned()),
                presence_check_only: false,
            }]],
            None,
            &[],
            Nip82Progress::none(),
            BLOSSOM_RETRY_RECOVERY,
        );

        assert!(message.contains(
            "upload https://blossom.example/: failed (HTTP 413 Payload Too Large: quota exceeded)"
        ));
        Ok(())
    }

    #[test]
    fn blossom_failure_message_names_the_blob_without_a_confirmed_copy() -> Result<()> {
        let failed = Url::parse("https://failed.example/")?;
        let available = Url::parse("https://available.example/")?;
        let labels = [
            BlossomBlobLabel {
                filename: "ngit-grasp.tar.gz".to_owned(),
                sha256: "aa11".to_owned(),
            },
            BlossomBlobLabel {
                filename: "SHA256SUMS".to_owned(),
                sha256: "bb22".to_owned(),
            },
        ];
        let message = blossom_failure_message(
            "1/2 Blossom blobs were not confirmed on any selected server",
            &[
                vec![BlossomServerOutcome {
                    server: failed,
                    operation: BlossomServerOperation::Upload,
                    status: BlossomServerStatus::Unknown,
                    descriptor: None,
                    message: Some("operation timed out".to_owned()),
                    presence_check_only: false,
                }],
                vec![BlossomServerOutcome {
                    server: available,
                    operation: BlossomServerOperation::Upload,
                    status: BlossomServerStatus::AlreadyPresent,
                    descriptor: None,
                    message: None,
                    presence_check_only: false,
                }],
            ],
            Some(&labels),
            &[],
            Nip82Progress::none(),
            BLOSSOM_RETRY_RECOVERY,
        );

        assert!(message.contains("ngit-grasp.tar.gz (aa11): NO CONFIRMED COPY"));
        assert!(message.contains("SHA256SUMS (bb22): 1/1 confirmed"));
        assert!(message.contains("    upload https://failed.example/: unknown"));
        Ok(())
    }

    #[test]
    fn incomplete_blossom_replication_is_an_actionable_warning() -> Result<()> {
        let available = Url::parse("https://available.example/")?;
        let unavailable = Url::parse("https://unavailable.example/")?;
        let publication = BlossomPublication {
            json: json!({}),
            outcomes: vec![vec![
                BlossomServerOutcome {
                    server: available,
                    operation: BlossomServerOperation::Upload,
                    status: BlossomServerStatus::Stored,
                    descriptor: None,
                    message: None,
                    presence_check_only: false,
                },
                BlossomServerOutcome {
                    server: unavailable.clone(),
                    operation: BlossomServerOperation::Upload,
                    status: BlossomServerStatus::Unknown,
                    descriptor: None,
                    message: Some("connection timed out".to_owned()),
                    presence_check_only: false,
                },
            ]],
            possible_orphan_blobs: Vec::new(),
        };

        let warning = publication
            .incomplete_replication_warning()
            .context("partial placement should produce a warning")?;
        assert_eq!(warning.code, "blossom_replication_incomplete");
        assert!(warning.message.contains("1/1 blobs are available"));
        assert!(warning.message.contains("1/1 copies available"));
        assert!(warning.message.contains("1 uploaded now"));
        assert!(warning.message.contains("0/1 copies available"));
        assert!(warning.message.contains("1 uncertain"));
        assert!(warning.message.contains(unavailable.as_str()));
        assert_eq!(warning.details["confirmed"], 1);
        assert_eq!(warning.details["placements"], 2);
        assert_eq!(warning.details["servers"][0], unavailable.as_str());
        assert_eq!(warning.details["blobs"]["available"], 1);
        assert_eq!(warning.details["copies_by_server"][0]["uploaded_now"], 1);
        Ok(())
    }
}
