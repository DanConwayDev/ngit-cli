use std::{collections::BTreeSet, fmt::Write as _};

use anyhow::{Context, Result};
use ngit::software_release::{SoftwareApplication, SoftwareRelease};
use nostr::prelude::{Coordinate, Filter, FromBech32, PublicKey, ToBech32, nip19::Nip19Coordinate};
use serde_json::{Value, json};

use super::support::{
    AuthorityJson, CommandOutput, LoginMode, ReleaseContext, application_for_release,
    application_json, coded_error, load_applications, load_releases,
    load_trusted_linked_applications, release_json, resolve_application, resolve_release,
};
use crate::{
    cli::{Cli, ReleaseAppListArgs, ReleaseAppViewArgs, ReleaseListArgs, ReleaseViewArgs},
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
    let mut context =
        ReleaseContext::load(cli, args.offline, &args.relays, LoginMode::Optional).await?;
    let applications = load_trusted_linked_applications(&mut context).await?;
    let releases = load_releases(&mut context, &applications).await?;
    let (release, is_latest, latest_event_id) = resolve_release_for_read(
        &mut context,
        &applications,
        &releases,
        &args.release,
        args.app.as_deref(),
    )
    .await?;
    let application = application_for_release(&applications, &release)?;
    let authority = context.authority(application);
    let mut release_value = release_json(&release);
    release_value["is_latest"] = json!(is_latest);
    release_value["latest_event_id"] = json!(latest_event_id);
    let result = json!({
        "application": application_json(&context, application),
        "release": release_value,
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
        "release.view",
        &mut context,
        authority,
        result,
        human,
    ))
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

fn release_summary_json(release: &SoftwareRelease) -> Value {
    let mut value = release_json(release);
    value["asset_count"] = json!(release.assets.len());
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
