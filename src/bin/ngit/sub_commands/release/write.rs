use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use ngit::{
    apk::{APK_MIME_TYPE, ApkPlatformInference, inspect_apk_platforms},
    blossom::{
        BlossomServerList, BlossomServerOperation, BlossomServerOutcome, BlossomServerStatus,
        FileSnapshot, LocalFileRequest, MultiServerUpload, MultiServerUploadError,
        PossibleOrphanBlob, blossom_server_list_filter, blossom_server_list_from_events,
        canonicalize_blossom_server_root, confirm_snapshot_on_servers, snapshot_local_file,
    },
    client::{sign_draft_event, sign_event},
    event_ordering::{finalize_fixed_timestamp_ordered_unsigned, finalize_ordered_unsigned},
    git::{Repo, RepoActions},
    release_download::{UrlAssetRequest, download_url_asset},
    release_manifest::{
        ResolvedReleaseManifest, ResolvedReleaseManifestAsset, ResolvedReleaseManifestSource,
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
    load_releases, release_json, resolve_application, resolve_release,
};
use crate::{
    cli::{
        ReleaseAppInitArgs, ReleaseAppLinkArgs, ReleaseAssetAddArgs, ReleasePublishArgs,
        SignerParams,
    },
    sub_commands::id_resolver::parse_event_id,
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
    let manifest = resolve_manifest(repo.get_path()?, args)?;
    let publication = effective_publication_settings(args, manifest.as_ref());
    let mut context =
        ReleaseContext::load_for_write(&publication.relays, publication.zapstore_relay, signer)
            .await?;
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

    let identifier = format!("{}@{}", application_target.identifier, args.release_version);
    let existing =
        load_exact_release(&mut context, &application_target, &args.release_version).await?;
    enforce_edit_guard(existing.as_ref(), args.edit, "release", &identifier)?;
    let commit = release_commit(&context, args, manifest.as_ref(), existing.as_ref())?;

    let manifest_has_files = manifest.as_ref().is_some_and(|manifest| {
        manifest
            .assets
            .iter()
            .any(|asset| matches!(asset.source, ResolvedReleaseManifestSource::File(_)))
    });
    let has_local_files =
        manifest_has_files || !args.files.is_empty() || !args.platform_agnostic_files.is_empty();
    if !publication.blossom_servers.is_empty() && !has_local_files {
        return Err(coded_error(
            "blossom_server_without_file",
            "publication.blossom_servers or --blossom-server requires a local file from --file, --platform-agnostic-file, or the release manifest",
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
                            &args.release_version,
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
                            &args.release_version,
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
                &args.release_version,
            ),
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &prepared_assets, &input)?;
        prepared_assets.push(PreparedAsset::Ready(input));
    }
    for url in &args.platform_agnostic_assets {
        let input = prepare_url_asset(
            &mut context,
            NewUrlAsset::simple(url, Vec::new(), &application_target, &args.release_version),
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
                &args.release_version,
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
                &args.release_version,
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
        prepared_assets
            .iter()
            .any(|asset| matches!(asset, PreparedAsset::File(_))),
    )
    .await?;

    let notes = release_notes(&context, args, manifest.as_ref(), existing.as_ref())?;
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
            "release notes are empty; provide --notes, --notes-file, or manifest notes",
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
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        existing_application.as_ref(),
        &args.release_version,
        existing.as_ref(),
    )
    .await?;
    if existing_application.is_none() {
        let input =
            bootstrap_application_input(&context, &application_target, release_platforms.clone())?;
        super::write_app::add_metadata_warnings(&mut context, &input, publication.strict_metadata)?;
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
            upload_prepared_file_assets(&mut prepared_assets, selection, &signer).await?
        }
        None => BlossomPublication::empty(),
    };
    ensure_release_state_unchanged(
        &mut context,
        &application_target,
        existing_application.as_ref(),
        &args.release_version,
        existing.as_ref(),
    )
    .await
    .map_err(|error| {
        preserve_completed_blossom(error, &blossom, "state_recheck", Nip82Progress::none())
    })?;

    let application = if let Some(application) = existing_application.as_ref() {
        if platform_policy.application_platforms_added.is_empty() {
            application.clone()
        } else {
            sign_application_platform_update(
                application,
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
        }
    } else {
        sign_bootstrap_application(
            bootstrap_application_input(&context, &application_target, release_platforms.clone())?,
            &application_target,
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
    let signed_application_id = (existing_application.is_none()
        || !platform_policy.application_platforms_added.is_empty())
    .then_some(application.raw_event.id);
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
        &args.release_version,
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
        &args.release_version,
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
    } else if platform_policy.application_platforms_added.is_empty() {
        "unchanged"
    } else {
        "edited"
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
    let mut context =
        ReleaseContext::load_for_write(&args.relays, args.zapstore_relay, signer).await?;
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
            upload_prepared_file_assets(std::slice::from_mut(prepared), selection, &signer).await?
        }
        _ => BlossomPublication::empty(),
    };
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

fn bootstrap_application_input(
    context: &ReleaseContext,
    target: &ApplicationTarget,
    platforms: Vec<String>,
) -> Result<ApplicationInput> {
    super::write_app::application_input_from_repository(context, &target.identifier, platforms)
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

fn resolve_manifest(
    repository_root: &Path,
    args: &ReleasePublishArgs,
) -> Result<Option<ResolvedReleaseManifest>> {
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
    if !should_load {
        return Ok(None);
    }
    let loaded = load_release_manifest(repository_root, requested)?;
    Ok(Some(
        loaded
            .manifest
            .resolve(&args.release_version, args.tag.as_deref())?,
    ))
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
) -> Result<String> {
    if let Some(notes) = &args.notes {
        return Ok(notes.clone());
    }
    if let Some(path) = &args.notes_file {
        let path = repository_relative_path(context.git_repo_path()?, path);
        return fs::read_to_string(&path)
            .with_context(|| format!("failed to read release notes {}", path.display()));
    }
    Ok(manifest
        .and_then(|manifest| manifest.notes.clone())
        .or_else(|| existing.map(|release| release.notes.clone()))
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
    apk_platform_inference: Option<Box<ApkPlatformInference>>,
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
    proposed: NewFileAsset,
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

    let mut platforms = proposed.metadata.platforms;
    let apk_platform_inference = infer_apk_platforms(
        context,
        &snapshot,
        &proposed.source_path,
        &mut platforms,
        proposed.platform_agnostic,
    )?;
    if platforms.is_empty() && apk_platform_inference.is_none() && !proposed.platform_agnostic {
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
        platforms,
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

fn infer_apk_platforms(
    context: &mut ReleaseContext,
    snapshot: &FileSnapshot,
    source_path: &Path,
    platforms: &mut Vec<String>,
    platform_agnostic: bool,
) -> Result<Option<ApkPlatformInference>> {
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

    let inference = inspect_apk_platforms(snapshot).map_err(|error| {
        coded_error_with_details(
            "invalid_apk",
            format!(
                "cannot infer Android platforms from {}: {error:#}",
                snapshot.filename
            ),
            json!({
                "source": source_path.display().to_string(),
                "filename": snapshot.filename,
            }),
        )
    })?;
    let derived = inference
        .derived_platforms
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let conflicting = platforms
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
                "declared_platforms": platforms,
                "derived_platforms": inference.derived_platforms,
                "conflicting_platforms": conflicting,
                "native_libraries_present": inference.native_libraries_present,
            }),
        ));
    }

    platforms.extend(inference.derived_platforms.iter().cloned());
    platforms.sort();
    platforms.dedup();
    if !inference.unknown_abis.is_empty() {
        context.warnings.push(WarningJson::new(
            "apk_unknown_abi",
            format!(
                "the APK contains unrecognized native ABI directories: {}",
                inference.unknown_abis.join(", ")
            ),
        ));
    }
    Ok(Some(inference))
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

async fn upload_prepared_file_assets(
    prepared_assets: &mut [PreparedAsset],
    selection: &BlossomServerSelection,
    signer: &std::sync::Arc<ngit::NgitSigner>,
) -> Result<BlossomPublication> {
    let mut uploads = Vec::new();
    let mut outcomes = Vec::new();
    let mut possible_orphan_blobs = Vec::new();
    for prepared in prepared_assets {
        let PreparedAsset::File(pending) = prepared else {
            continue;
        };
        let upload = match confirm_snapshot_on_servers(
            &selection.servers,
            &pending.snapshot,
            signer,
        )
        .await
        {
            Ok(upload) => upload,
            Err(error) => {
                let (stage, server) = failed_blossom_operation(&error);
                possible_orphan_blobs.extend(error.possible_orphan_blobs.iter().cloned());
                uploads.push(failed_blossom_upload_json(pending, &error));
                outcomes.push(error.servers.clone());
                let message = blossom_failure_message(
                    &error.message,
                    &outcomes,
                    &possible_orphan_blobs,
                    Nip82Progress::none(),
                    BLOSSOM_RETRY_RECOVERY,
                );
                return Err(coded_error_with_details(
                    "blossom_publication_failed",
                    message,
                    json!({
                        "stage": stage,
                        "server": server,
                        "blossom": blossom_json(selection, &uploads),
                        "possible_orphan_blobs": possible_orphan_blobs,
                        "release_events_signed": false,
                        "release_events_published": false,
                        "recovery": BLOSSOM_RETRY_RECOVERY,
                    }),
                ));
            }
        };
        pending.input.url = Some(upload.primary.url.to_string());
        uploads.push(blossom_upload_json(pending, &upload));
        outcomes.push(upload.servers.clone());
        possible_orphan_blobs.extend(possible_orphans_from_upload(&pending.snapshot, &upload));
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

fn failed_blossom_upload_json(pending: &PendingFileAsset, error: &MultiServerUploadError) -> Value {
    json!({
        "source": pending.source_path,
        "filename": pending.snapshot.filename,
        "sha256": pending.snapshot.sha256,
        "size": pending.snapshot.size.to_string(),
        "mime": pending.snapshot.mime_type,
        "apk_platform_inference": pending.apk_platform_inference,
        "primary_url": error.servers.first().and_then(|outcome| outcome.descriptor.as_ref()).map(|descriptor| descriptor.url.as_str()),
        "servers": error.servers.iter().map(blossom_server_outcome_json).collect::<Vec<_>>(),
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

fn failed_blossom_operation(error: &MultiServerUploadError) -> (Value, Value) {
    error
        .servers
        .iter()
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

fn blossom_failure_message(
    message: &str,
    outcomes: &[Vec<BlossomServerOutcome>],
    possible_orphan_blobs: &[PossibleOrphanBlob],
    progress: Nip82Progress<'_>,
    recovery: &str,
) -> String {
    let server_outcomes = outcomes
        .iter()
        .flatten()
        .map(|outcome| {
            let operation = match outcome.operation {
                BlossomServerOperation::Upload => "upload",
                BlossomServerOperation::Mirror => "mirror",
            };
            let status = match outcome.status {
                BlossomServerStatus::Stored => "stored",
                BlossomServerStatus::AlreadyPresent => "already_present",
                BlossomServerStatus::Failed => "failed",
                BlossomServerStatus::Unknown => "unknown",
                BlossomServerStatus::NotAttempted => "not_attempted",
            };
            format!("{operation} {}: {status}", outcome.server)
        })
        .collect::<Vec<_>>()
        .join("; ");
    let possible_orphans = if possible_orphan_blobs.is_empty() {
        "none".to_owned()
    } else {
        possible_orphan_blobs
            .iter()
            .map(|orphan| {
                orphan.url.as_ref().map_or_else(
                    || format!("{} hash {} (URL unknown)", orphan.server, orphan.sha256),
                    |url| format!("{url} hash {}", orphan.sha256),
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
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
        "{message}\nserver outcomes: {server_outcomes}\npossible orphan blobs: {possible_orphans}\nsigned application ID: {signed_application_id}\nsigned asset IDs: {signed_asset_ids}\nrelease event signed: {}\npublication complete: {}\nrecovery: {recovery}",
        progress.release_event_signed, progress.publication_complete
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
) -> Result<Option<String>> {
    let requested = args
        .commit
        .as_deref()
        .or_else(|| manifest.and_then(|manifest| manifest.commit.as_deref()));
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
}
