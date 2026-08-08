use std::{collections::BTreeSet, fmt::Write as _};

use anyhow::{Context, Result};
use ngit::software_release::SoftwareApplication;
use nostr::prelude::{Coordinate, FromBech32, PublicKey, ToBech32, nip19::Nip19Coordinate};
use serde_json::json;

use super::support::{
    AuthorityJson, CommandOutput, LoginMode, ReleaseContext, application_json, load_applications,
    resolve_application,
};
use crate::cli::{Cli, ReleaseAppListArgs, ReleaseAppViewArgs};

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
