use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use ngit::{
    client::{sign_draft_event, sign_event},
    event_ordering::{finalize_fixed_timestamp_ordered_unsigned, finalize_ordered_unsigned},
    release_download::{UrlAssetRequest, download_url_asset},
    release_manifest::{
        ResolvedReleaseManifest, ResolvedReleaseManifestAsset, load_release_manifest,
        resolve_release_manifest_path,
    },
    software_release::{
        AddressPointer, ApplicationInput, AssetInput, ReleaseAssetInput, ReleaseInput,
        SOFTWARE_APPLICATION_KIND, SoftwareApplication, SoftwareAsset, SoftwareRelease,
        application_event_builder, asset_event_builder, release_event_builder,
    },
};
use nostr::prelude::{
    Coordinate, Event, Filter, FromBech32, PublicKey, Timestamp, nip19::Nip19Coordinate,
};
use serde::Serialize;
use serde_json::json;

use super::support::{
    AssetReuseOption, CommandOutput, LoginMode, ReleaseContext, WarningJson, application_json,
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
    let mut context =
        ReleaseContext::load(false, &args.relays, LoginMode::Required, signer).await?;
    let manifest = resolve_manifest(&context, args)?;
    let app_selector = args.app.as_deref().or_else(|| {
        manifest
            .as_ref()
            .and_then(|manifest| manifest.application.as_deref())
    });
    let maintainers = context.repo_ref.maintainers.clone();
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

    let mut assets = if let Some(release) = &existing {
        require_all_assets(&mut context, release).await?
    } else {
        Vec::new()
    };
    let mut prepared_assets = Vec::new();
    if let Some(manifest) = &manifest {
        for asset in &manifest.assets {
            let input = prepare_url_asset(
                &mut context,
                NewUrlAsset::from_manifest(asset, &application_target, &args.release_version),
            )
            .await?;
            reject_duplicate_prepared_asset(&assets, &prepared_assets, &input)?;
            prepared_assets.push(input);
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
        prepared_assets.push(input);
    }
    for url in &args.platform_agnostic_assets {
        let input = prepare_url_asset(
            &mut context,
            NewUrlAsset::simple(url, Vec::new(), &application_target, &args.release_version),
        )
        .await?;
        reject_duplicate_prepared_asset(&assets, &prepared_assets, &input)?;
        prepared_assets.push(input);
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
        args.allow_partial_platforms,
        args.add_application_platforms,
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
        super::write_app::add_metadata_warnings(&mut context, &input, args.strict_metadata)?;
    }
    enforce_metadata_policy(&context, args.strict_metadata)?;
    context.emit_human_warnings_before_signing(args.json);

    let signer = context
        .signer
        .as_ref()
        .context("nostr signer was not initialized")?
        .clone();
    let application = if let Some(application) = existing_application.as_ref() {
        if platform_policy.application_platforms_added.is_empty() {
            application.clone()
        } else {
            sign_application_platform_update(
                application,
                &platform_policy.resulting_application_platforms,
                &signer,
            )
            .await?
        }
    } else {
        sign_bootstrap_application(
            bootstrap_application_input(&context, &application_target, release_platforms.clone())?,
            &application_target,
            &signer,
        )
        .await?
    };
    let mut new_asset_event_ids = Vec::new();
    for input in prepared_assets {
        let asset = sign_asset_input(input, &signer).await?;
        reject_duplicate_asset(&assets, &asset)?;
        new_asset_event_ids.push(asset.raw_event.id);
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
    .await?;
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
    .await?;
    let parsed_release = SoftwareRelease::parse(&release_event)?;

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
        .await?;
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
        ReleaseContext::load(false, &args.relays, LoginMode::Required, signer).await?;
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
    let (prepared_asset, reused_asset) = if let Some(url) = &args.url {
        if args.platforms.is_empty() && !args.platform_agnostic {
            return Err(coded_error(
                "asset_platform_required",
                "provide at least one --platform or explicitly use --platform-agnostic",
            ));
        }
        let proposed = NewUrlAsset {
            source: url.clone(),
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
        };
        let input = prepare_url_asset(&mut context, proposed).await?;
        reject_duplicate_prepared_asset(&assets, &[], &input)?;
        (Some(input), None)
    } else {
        let selector = args.event.as_deref().context("--event is required")?;
        let asset = load_asset_event(&mut context, selector, true).await?;
        validate_reused_asset(&application_target, &asset, args.platform_agnostic)?;
        reject_duplicate_asset(&assets, &asset)?;
        (None, Some(asset))
    };
    let release_platforms = assets
        .iter()
        .flat_map(|asset| asset.platforms.iter())
        .chain(
            prepared_asset
                .iter()
                .flat_map(|asset| asset.platforms.iter()),
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
    let application = if platform_policy.application_platforms_added.is_empty() {
        application
    } else {
        sign_application_platform_update(
            &application,
            &platform_policy.resulting_application_platforms,
            &signer,
        )
        .await?
    };
    let newly_published = prepared_asset.is_some();
    let asset = if let Some(input) = prepared_asset {
        sign_asset_input(input, &signer).await?
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
    .await?;
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
    .await?;
    let parsed_release = SoftwareRelease::parse(&release_event)?;

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
        .await?;
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
        load_applications(context, context.repo_ref.maintainers.clone(), true).await?;
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
    prepared: &[AssetInput],
    reused: &[SoftwareAsset],
) -> Vec<String> {
    existing
        .iter()
        .flat_map(|asset| asset.platforms.iter())
        .chain(prepared.iter().flat_map(|asset| asset.platforms.iter()))
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
    if !context.repo_ref.maintainers.contains(&author) {
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
    context: &ReleaseContext,
    args: &ReleasePublishArgs,
) -> Result<Option<ResolvedReleaseManifest>> {
    let has_direct_assets = !args.assets.is_empty()
        || !args.asset_events.is_empty()
        || !args.platform_agnostic_assets.is_empty();
    let requested = args.manifest.as_deref();
    let should_load = requested.is_some()
        || (!args.edit
            && !has_direct_assets
            && resolve_release_manifest_path(context.git_repo_path()?, None)?.exists());
    if !should_load {
        return Ok(None);
    }
    let loaded = load_release_manifest(context.git_repo_path()?, requested)?;
    Ok(Some(
        loaded
            .manifest
            .resolve(&args.release_version, args.tag.as_deref())?,
    ))
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
        || !context.repo_ref.maintainers.contains(&application.author)
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
    prepared: &[AssetInput],
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
            .any(|asset| asset.filename.as_deref() == Some(filename))
    {
        return Err(coded_error(
            "duplicate_asset_filename",
            format!("an attached or proposed asset already uses filename {filename:?}"),
        ));
    }
    Ok(())
}

fn reject_asset_against_prepared(prepared: &[AssetInput], proposed: &SoftwareAsset) -> Result<()> {
    let Some(filename) = proposed.filename.as_deref() else {
        return Ok(());
    };
    if prepared
        .iter()
        .any(|asset| asset.filename.as_deref() == Some(filename))
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
        application: &ApplicationTarget,
        release_version: &str,
    ) -> Self {
        Self {
            source: asset.source.clone(),
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
    for warning in &downloaded.warnings {
        let code = serde_json::to_value(warning.code)?
            .as_str()
            .unwrap_or("asset_download_warning")
            .to_owned();
        context
            .warnings
            .push(WarningJson::new(code, warning.message.clone()));
    }
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
    asset_event_builder(input.clone()).map_err(|error| {
        coded_error_with_details(
            "invalid_asset_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    Ok(input)
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
    use super::{enforce_platform_policy, is_metadata_warning};

    fn values(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn error_code(error: &anyhow::Error) -> &'static str {
        error
            .downcast_ref::<super::super::support::ReleaseError>()
            .expect("platform policy returned an uncoded error")
            .code
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
}
