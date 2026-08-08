use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
};

use anyhow::{Context, Result};
use ngit::{
    client::get_events_from_local_cache,
    release_download::{UrlAssetRequest, download_url_asset},
    software_release::{SOFTWARE_ASSET_KIND, SoftwareApplication, SoftwareAsset, SoftwareRelease},
};
use nostr::prelude::{
    Coordinate, Event, EventId, Filter, FromBech32, PublicKey, ToBech32,
    nip19::{Nip19Coordinate, Nip19Event},
};
use serde_json::{Value, json};

use super::support::{
    AuthorityJson, CommandOutput, LoginMode, ReleaseContext, application_for_release,
    application_json, asset_json, coded_error, load_applications, load_assets, load_releases,
    load_trusted_linked_applications, release_json, resolve_application, resolve_asset,
    resolve_release,
};
use crate::{
    cli::{
        Cli, ReleaseAppListArgs, ReleaseAppViewArgs, ReleaseAssetListArgs, ReleaseAssetViewArgs,
        ReleaseListArgs, ReleaseViewArgs,
    },
    sub_commands::id_resolver::parse_event_id,
};

pub(super) async fn app_list(cli: &Cli, args: &ReleaseAppListArgs) -> Result<CommandOutput> {
    let login_mode = if args.mine {
        LoginMode::Required
    } else {
        LoginMode::Optional
    };
    let mut context = ReleaseContext::load(cli, args.offline, &args.relays, login_mode).await?;
    let authors = if args.mine {
        vec![
            context
                .current_signer()
                .context("active signer was not resolved")?,
        ]
    } else if let Some(author) = &args.author {
        vec![PublicKey::parse(author).context("invalid --author public key")?]
    } else {
        context.repo_ref.maintainers.clone()
    };
    for author in &authors {
        context.add_author_relays(*author).await?;
    }
    let mut applications = load_applications(&mut context, authors).await?;
    if !args.mine && args.author.is_none() {
        applications.retain(|application| context.application_is_trusted(application));
    }
    if args.linked {
        applications.retain(|application| context.application_is_linked(application));
    } else if args.unlinked {
        applications.retain(|application| !context.application_is_linked(application));
    }

    let result = json!({
        "applications": applications
            .iter()
            .map(|application| application_json(&context, application))
            .collect::<Vec<_>>(),
        "offline": args.offline,
    });
    let human = if applications.is_empty() {
        "no software applications found".to_string()
    } else {
        applications
            .iter()
            .map(|application| {
                let authority = context.authority(application);
                format!(
                    "{}\t{}\t{}\t{}\t{}",
                    application.identifier,
                    application.name,
                    short_pubkey(application.raw_event.pubkey),
                    if context.application_is_linked(application) {
                        "linked"
                    } else {
                        "unlinked"
                    },
                    if authority.can_publish {
                        "can publish"
                    } else {
                        authority.blocker.as_deref().unwrap_or("read only")
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let authority = AuthorityJson::unknown(context.current_signer());
    Ok(CommandOutput::new(
        "release.app.list",
        &mut context,
        authority,
        result,
        human,
    ))
}

pub(super) async fn app_view(cli: &Cli, args: &ReleaseAppViewArgs) -> Result<CommandOutput> {
    let mut context =
        ReleaseContext::load(cli, args.offline, &args.relays, LoginMode::Optional).await?;
    let mut authors: BTreeSet<PublicKey> = context.repo_ref.maintainers.iter().copied().collect();
    if let Some(signer) = context.current_signer() {
        authors.insert(signer);
    }
    if let Some(author) = explicit_application_author(&args.app) {
        authors.insert(author);
    }
    for author in &authors {
        context.add_author_relays(*author).await?;
    }
    let applications = load_applications(&mut context, authors.into_iter().collect()).await?;
    let application = resolve_application(&applications, &args.app)?;
    let authority = context.authority(application);
    let result = json!({ "application": application_json(&context, application) });
    let mut human = format_application(application, &context);
    if !authority.can_publish {
        let _ = write!(
            human,
            "\n\npublication: {}",
            authority.blocker.as_deref().unwrap_or("read only")
        );
        if authority.blocker.as_deref() == Some("application_author_mismatch") {
            let _ = write!(
                human,
                "\nonly application author {} can publish releases",
                short_pubkey(application.raw_event.pubkey)
            );
        }
    }
    Ok(CommandOutput::new(
        "release.app.view",
        &mut context,
        authority,
        result,
        human,
    ))
}

pub(super) async fn release_list(cli: &Cli, args: &ReleaseListArgs) -> Result<CommandOutput> {
    let mut context =
        ReleaseContext::load(cli, args.offline, &args.relays, LoginMode::Optional).await?;
    let mut applications = load_trusted_linked_applications(&mut context).await?;
    if let Some(selector) = &args.app {
        let selected = resolve_application(&applications, selector)?.coordinate();
        applications.retain(|application| application.coordinate() == selected);
    }
    if let Some(author) = &args.author {
        let author = PublicKey::parse(author).context("invalid --author public key")?;
        if !context.repo_ref.maintainers.contains(&author) {
            return Err(coded_error(
                "not_repository_maintainer",
                "--author is not a current repository maintainer",
            ));
        }
        applications.retain(|application| application.raw_event.pubkey == author);
    }
    let mut releases = load_releases(&mut context, &applications).await?;
    if let Some(channel) = &args.channel {
        releases.retain(|release| release.channel == *channel);
    }
    if !args.platforms.is_empty() {
        releases.retain(|release| {
            args.platforms
                .iter()
                .any(|platform| release.platforms.contains(platform))
        });
    }
    if let Some(limit) = args.limit {
        releases.truncate(limit);
    }
    let result = json!({
        "releases": releases
            .iter()
            .map(|release| {
                let mut value = release_summary_json(release);
                if let Ok(application) = application_for_release(&applications, release) {
                    value["authority"] = json!(context.authority(application));
                }
                value
            })
            .collect::<Vec<_>>(),
        "offline": args.offline,
    });
    let human = if releases.is_empty() {
        "no software releases found".to_string()
    } else {
        releases
            .iter()
            .map(|release| {
                let (author, publication) = application_for_release(&applications, release)
                    .map_or_else(
                        |_| ("unknown".to_owned(), "read only".to_owned()),
                        |application| {
                            let authority = context.authority(application);
                            (
                                short_pubkey(application.raw_event.pubkey),
                                if authority.can_publish {
                                    "can publish".to_owned()
                                } else {
                                    authority.blocker.unwrap_or_else(|| "read only".to_owned())
                                },
                            )
                        },
                    );
                format!(
                    "{}\t{}\t{}\t{}\t{} asset{}\t{}\t{}\t{}",
                    release.application_identifier,
                    release.version,
                    release.channel,
                    release.raw_event.created_at.as_secs(),
                    release.assets.len(),
                    if release.assets.len() == 1 { "" } else { "s" },
                    release.platforms.join(","),
                    author,
                    publication,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let authority = AuthorityJson::unknown(context.current_signer());
    Ok(CommandOutput::new(
        "release.list",
        &mut context,
        authority,
        result,
        human,
    ))
}

pub(super) async fn release_view(cli: &Cli, args: &ReleaseViewArgs) -> Result<CommandOutput> {
    view_release(
        cli,
        &args.release,
        args.app.as_deref(),
        args.verify,
        args.offline,
        &args.relays,
        "release.view",
    )
    .await
}

pub(super) async fn asset_list(cli: &Cli, args: &ReleaseAssetListArgs) -> Result<CommandOutput> {
    let mut output = view_release(
        cli,
        &args.release,
        args.app.as_deref(),
        false,
        args.offline,
        &args.relays,
        "release.asset.list",
    )
    .await?;
    let assets = output
        .result
        .get("assets")
        .cloned()
        .unwrap_or_else(|| json!([]));
    output.result = json!({ "assets": assets });
    output.human = assets_human(&output.result["assets"]);
    Ok(output)
}

pub(super) async fn asset_view(cli: &Cli, args: &ReleaseAssetViewArgs) -> Result<CommandOutput> {
    let mut context =
        ReleaseContext::load(cli, args.offline, &args.relays, LoginMode::Optional).await?;
    let (asset, authority, release_value, application_value) =
        if let Some(release_selector) = &args.release {
            let applications = load_trusted_linked_applications(&mut context).await?;
            let releases = load_releases(&mut context, &applications).await?;
            let (release, is_latest, latest_event_id) = resolve_release_for_read(
                &mut context,
                &applications,
                &releases,
                release_selector,
                args.app.as_deref(),
            )
            .await?;
            let assets = load_assets(&mut context, &[&release]).await?;
            let asset = resolve_asset(&assets, &args.asset)?.clone();
            let application = application_for_release(&applications, &release)?;
            let mut release_value = release_json(&release, &assets);
            release_value["is_latest"] = json!(is_latest);
            release_value["latest_event_id"] = json!(latest_event_id);
            (
                asset,
                context.authority(application),
                Some(release_value),
                Some(application_json(&context, application)),
            )
        } else {
            let event_id = parse_event_id(&args.asset).map_err(|_| {
                coded_error(
                    "asset_not_found",
                    "asset view without --release requires an event ID or nevent",
                )
            })?;
            let events = context
                .query(vec![Filter::new().kind(SOFTWARE_ASSET_KIND).id(event_id)])
                .await?;
            let event = events
                .iter()
                .find(|event| event.id == event_id)
                .ok_or_else(|| coded_error("asset_not_found", "software asset was not found"))?;
            let asset = SoftwareAsset::parse(event)
                .map_err(|error| coded_error("invalid_asset_metadata", error.to_string()))?;
            (
                asset,
                AuthorityJson::unknown(context.current_signer()),
                None,
                None,
            )
        };

    let mut value = asset_json(&asset);
    if args.verify {
        value["verification"] = verify_asset(&asset).await?;
    }
    let result = json!({
        "application": application_value,
        "release": release_value,
        "asset": value,
    });
    let human = format_asset(&asset, args.verify);
    Ok(CommandOutput::new(
        "release.asset.view",
        &mut context,
        authority,
        result,
        human,
    ))
}

#[allow(clippy::too_many_lines)]
async fn view_release(
    cli: &Cli,
    selector: &str,
    app_selector: Option<&str>,
    verify: bool,
    offline: bool,
    relays: &[String],
    command: &'static str,
) -> Result<CommandOutput> {
    let mut context = ReleaseContext::load(cli, offline, relays, LoginMode::Optional).await?;
    let applications = load_trusted_linked_applications(&mut context).await?;
    let releases = load_releases(&mut context, &applications).await?;
    let (release, is_latest, latest_event_id) = resolve_release_for_read(
        &mut context,
        &applications,
        &releases,
        selector,
        app_selector,
    )
    .await?;
    let assets = load_assets(&mut context, &[&release]).await?;
    let application = application_for_release(&applications, &release)?;
    let authority = context.authority(application);
    let resolved_ids: BTreeSet<_> = assets.iter().map(|asset| asset.raw_event.id).collect();
    let raw_asset_events = get_events_from_local_cache(
        context.git_repo_path()?,
        vec![Filter::new().ids(release.assets.iter().map(|asset| asset.event_id))],
    )
    .await?;
    let raw_assets_by_id: HashMap<_, _> = raw_asset_events
        .into_iter()
        .map(|event| (event.id, event))
        .collect();
    let unresolved_asset_ids: Vec<String> = release
        .assets
        .iter()
        .filter(|pointer| !resolved_ids.contains(&pointer.event_id))
        .map(|pointer| pointer.event_id.to_hex())
        .collect();

    let mut asset_values = Vec::new();
    for pointer in &release.assets {
        if let Some(asset) = assets
            .iter()
            .find(|asset| asset.raw_event.id == pointer.event_id)
        {
            let mut value = asset_json(asset);
            if asset.raw_event.pubkey != release.raw_event.pubkey {
                value["resolution"] = json!("invalid");
                value["validation"] = json!([{
                    "code": "invalid_asset_author",
                    "message": "asset author does not match the release author",
                    "details": {
                        "asset_author": asset.raw_event.pubkey.to_hex(),
                        "release_author": release.raw_event.pubkey.to_hex(),
                    }
                }]);
            }
            if verify {
                value["verification"] = verify_asset(asset).await?;
            }
            asset_values.push(value);
        } else {
            asset_values.push(unresolved_asset_json(
                pointer.event_id,
                raw_assets_by_id.get(&pointer.event_id),
            ));
        }
    }
    let mut release_value = release_json(&release, &assets);
    release_value["is_latest"] = json!(is_latest);
    release_value["latest_event_id"] = json!(latest_event_id);
    let result = json!({
        "application": application_json(&context, application),
        "release": release_value,
        "assets": asset_values,
        "unresolved_asset_ids": unresolved_asset_ids,
    });
    let mut human = format!(
        "{} {} ({})\nreleased: {}\nauthor: {}\nplatforms: {}\n\n{}",
        release.application_identifier,
        release.version,
        release.channel,
        release.raw_event.created_at.as_secs(),
        short_pubkey(release.raw_event.pubkey),
        release.platforms.join(", "),
        release.notes
    );
    human.push_str("\n\nassets:\n");
    human.push_str(
        &assets
            .iter()
            .map(|asset| format!("  {}", format_asset(asset, verify)))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if !unresolved_asset_ids.is_empty() {
        let _ = write!(human, "\n  missing: {}", unresolved_asset_ids.join(", "));
    }
    if !authority.can_publish {
        let _ = write!(
            human,
            "\n\npublication: {}",
            authority.blocker.as_deref().unwrap_or("read only")
        );
        if authority.blocker.as_deref() == Some("application_author_mismatch") {
            let _ = write!(
                human,
                "\nonly application author {} can publish this release",
                short_pubkey(application.raw_event.pubkey)
            );
        }
    }
    Ok(CommandOutput::new(
        command,
        &mut context,
        authority,
        result,
        human,
    ))
}

fn unresolved_asset_json(event_id: EventId, event: Option<&Event>) -> Value {
    let (resolution, author, author_npub, validation, raw_event) = event.map_or_else(
        || {
            (
                "missing",
                None,
                None,
                json!([{
                    "code": "missing_referenced_asset",
                    "message": "referenced asset was not resolved",
                    "details": {},
                }]),
                Value::Null,
            )
        },
        |event| {
            let validation = SoftwareAsset::parse(event)
                .err()
                .map_or_else(Vec::new, |error| error.issues);
            (
                "invalid",
                Some(event.pubkey.to_hex()),
                event.pubkey.to_bech32().ok(),
                json!(validation),
                json!(event),
            )
        },
    );
    json!({
        "event_id": event_id.to_hex(),
        "event_id_bech32": event.map_or_else(
            || Nip19Event::new(event_id).to_bech32().ok(),
            |event| Nip19Event::new(event.id)
                .author(event.pubkey)
                .kind(event.kind)
                .to_bech32()
                .ok(),
        ),
        "author": author,
        "author_npub": author_npub,
        "identifier": null,
        "version": null,
        "url": null,
        "filename": null,
        "mime": null,
        "sha256": null,
        "size": null,
        "platforms": [],
        "min_platform_version": null,
        "target_platform_version": null,
        "supported_nips": [],
        "variant": null,
        "commit": null,
        "min_allowed_version": null,
        "android": {
            "version_code": null,
            "min_allowed_version_code": null,
            "certificate_sha256": [],
        },
        "original_url": null,
        "resolution": resolution,
        "verification": null,
        "validation": validation,
        "raw_event": raw_event,
    })
}

async fn resolve_release_for_read(
    context: &mut ReleaseContext,
    applications: &[SoftwareApplication],
    latest_releases: &[SoftwareRelease],
    selector: &str,
    app_selector: Option<&str>,
) -> Result<(SoftwareRelease, bool, Option<String>)> {
    let Ok(event_id) = parse_event_id(selector) else {
        let release = resolve_release(latest_releases, applications, selector, app_selector)?;
        return Ok((release.clone(), true, Some(release.raw_event.id.to_hex())));
    };

    let events = context.query(vec![Filter::new().id(event_id)]).await?;
    let event = events
        .iter()
        .find(|event| event.id == event_id)
        .ok_or_else(|| coded_error("release_not_found", "software release event was not found"))?;
    let release = SoftwareRelease::parse(event)
        .map_err(|error| coded_error("invalid_release_metadata", error.to_string()))?;
    if !applications
        .iter()
        .any(|application| application.coordinate() == release.application.coordinate)
    {
        return Err(coded_error(
            "release_author_mismatch",
            "release event does not belong to a trusted application linked to this repository",
        ));
    }
    let latest = latest_releases
        .iter()
        .find(|candidate| candidate.coordinate() == release.coordinate());
    let is_latest = latest.is_some_and(|candidate| candidate.raw_event.id == release.raw_event.id);
    Ok((
        release,
        is_latest,
        latest.map(|candidate| candidate.raw_event.id.to_hex()),
    ))
}

async fn verify_asset(asset: &SoftwareAsset) -> Result<Value> {
    let url = asset
        .url
        .as_ref()
        .ok_or_else(|| coded_error("asset_integrity_mismatch", "asset has no URL to verify"))?;
    let mut request = UrlAssetRequest::new(url);
    request.filename.clone_from(&asset.filename);
    request.mime_type = Some(asset.mime.clone());
    let downloaded = download_url_asset(request).await?;
    if !downloaded.sha256.eq_ignore_ascii_case(&asset.sha256)
        || asset.size.is_some_and(|size| size != downloaded.size)
    {
        return Err(coded_error(
            "asset_integrity_mismatch",
            format!(
                "asset {} does not match its published hash or size",
                asset.raw_event.id
            ),
        ));
    }
    Ok(json!({
        "ok": true,
        "observed_final_url": downloaded.final_url,
        "observed_sha256": downloaded.sha256,
        "observed_size": downloaded.size.to_string(),
        "warnings": downloaded.warnings,
    }))
}

fn release_summary_json(release: &SoftwareRelease) -> Value {
    let mut value = release_json(release, &[]);
    value["asset_count"] = json!(release.assets.len());
    value["derived_platforms"] = Value::Null;
    value["validation"] = json!([]);
    value
}

fn explicit_application_author(selector: &str) -> Option<PublicKey> {
    Nip19Coordinate::from_bech32(selector)
        .ok()
        .map(|pointer| pointer.coordinate.public_key)
        .or_else(|| {
            Coordinate::parse(selector)
                .ok()
                .map(|coordinate| coordinate.public_key)
        })
}

fn short_pubkey(public_key: PublicKey) -> String {
    let value = public_key
        .to_bech32()
        .unwrap_or_else(|_| public_key.to_hex());
    value.chars().take(20).collect()
}

fn format_application(application: &SoftwareApplication, context: &ReleaseContext) -> String {
    format!(
        "{} ({})\nauthor: {}\nlinked: {}\ntrusted: {}\nsummary: {}\nwebsite: {}\nrepository: {}\nlicense: {}\nplatforms: {}\n\n{}",
        application.name,
        application.identifier,
        short_pubkey(application.raw_event.pubkey),
        context.application_is_linked(application),
        context.application_is_trusted(application),
        application.summary.as_deref().unwrap_or("-"),
        application.website.as_deref().unwrap_or("-"),
        application.repository.as_deref().unwrap_or("-"),
        application.license.as_deref().unwrap_or("-"),
        application.platforms.join(", "),
        application.description,
    )
}

fn format_asset(asset: &SoftwareAsset, verified: bool) -> String {
    format!(
        "{}\n    id: {}\n    url: {}\n    mime: {}\n    size: {}\n    sha256: {}\n    platforms: {}{}",
        asset.filename.as_deref().unwrap_or("<unnamed>"),
        asset.raw_event.id,
        asset.url.as_deref().unwrap_or("-"),
        asset.mime,
        asset
            .size
            .map_or_else(|| "-".to_string(), |size| size.to_string()),
        asset.sha256,
        asset.platforms.join(", "),
        if verified { "\n    verified: yes" } else { "" },
    )
}

fn assets_human(assets: &Value) -> String {
    assets.as_array().map_or_else(
        || "no assets".to_string(),
        |assets| {
            assets
                .iter()
                .map(|asset| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        asset["event_id"].as_str().unwrap_or("-"),
                        asset["filename"].as_str().unwrap_or("-"),
                        asset["mime"].as_str().unwrap_or("-"),
                        asset["resolution"].as_str().unwrap_or("missing"),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
    )
}
