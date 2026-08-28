use std::collections::HashSet;

use anyhow::{Context, Result};
use ngit::{
    client::{send_events, sign_draft_event},
    event_ordering::{finalize_ordered_unsigned, latest_event},
    software_release::{
        ApplicationInput, SOFTWARE_APPLICATION_KIND, SoftwareApplication, application_event_builder,
    },
};
use nostr::prelude::{Coordinate, Filter, FromBech32, PublicKey, ToBech32, nip19::Nip19Coordinate};
use serde_json::{Value, json};

use super::support::{
    CommandOutput, LoginMode, ReleaseContext, WarningJson, application_json, coded_error,
    coded_error_with_details, coordinate_key, load_applications, resolve_application,
};
use crate::cli::{ReleaseAppInitArgs, ReleaseAppLinkArgs, SignerParams};

#[allow(clippy::too_many_lines)]
pub(super) async fn app_init(
    args: &ReleaseAppInitArgs,
    signer: SignerParams<'_>,
) -> Result<CommandOutput> {
    let mut context =
        ReleaseContext::load(false, &args.relays, LoginMode::Required, signer).await?;
    let signer_public_key = require_current_maintainer(&context)?;
    let identifier = args
        .id
        .clone()
        .unwrap_or_else(|| context.repo_ref.identifier.clone());
    if identifier.is_empty() {
        return Err(coded_error(
            "metadata_confirmation_required",
            "application identifier is empty; provide --id",
        ));
    }

    let existing =
        load_exact_application(&mut context, signer_public_key, &identifier, true).await?;
    match (&existing, args.edit) {
        (Some(application), false) => {
            return Err(coded_error_with_details(
                "application_already_exists",
                format!(
                    "software application {identifier:?} already exists; pass --edit to replace it"
                ),
                json!({ "event_id": application.raw_event.id.to_hex() }),
            ));
        }
        (None, true) => {
            return Err(coded_error(
                "edit_target_not_found",
                format!(
                    "software application {identifier:?} does not exist; --edit never creates an application"
                ),
            ));
        }
        _ => {}
    }

    if let Some(application) = &existing {
        context.require_owner_maintainer(application)?;
    }

    context.refresh_repository().await?;
    require_current_maintainer(&context)?;
    let refreshed =
        load_exact_application(&mut context, signer_public_key, &identifier, true).await?;
    ensure_same_application_revision(existing.as_ref(), refreshed.as_ref())?;
    if let Some(application) = &refreshed {
        context.require_owner_maintainer(application)?;
    }
    let existing = refreshed;

    let input = application_input(&context, args, &identifier, existing.as_ref())?;
    add_metadata_warnings(&mut context, &input, args.strict_metadata)?;

    let builder = application_event_builder(input).map_err(|error| {
        coded_error_with_details(
            "invalid_application_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    let unsigned = finalize_ordered_unsigned(
        builder,
        signer_public_key,
        existing.as_ref().map(|application| &application.raw_event),
    )
    .context("failed to order software application replacement")?;
    context.emit_human_warnings_before_signing(args.json);
    let signer = context
        .signer
        .clone()
        .context("application publication requires a signer")?;
    let event = sign_draft_event(
        unsigned,
        &signer,
        "publish software application".to_string(),
    )
    .await?;
    require_signed_author(&event, signer_public_key)?;
    let application = SoftwareApplication::parse(&event)
        .context("signed software application failed validation")?;
    let relay_results = publish_application(&context, event.clone(), args.json).await?;

    let operation = if existing.is_some() {
        "edited"
    } else {
        "created"
    };
    let previous_event_id = existing
        .as_ref()
        .map(|application| application.raw_event.id.to_hex());
    let authority = context.authority(&application);
    let result = mutation_json(
        &context,
        operation,
        previous_event_id.as_deref(),
        &application,
        &relay_results,
    );
    let human = format!(
        "{operation} software application {} ({})\nevent: {}",
        application.name,
        application.identifier,
        event.id.to_bech32()?
    );

    Ok(CommandOutput::new(
        "release.app.init",
        &mut context,
        authority,
        result,
        human,
    ))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn app_link(
    args: &ReleaseAppLinkArgs,
    signer: SignerParams<'_>,
) -> Result<CommandOutput> {
    let mut context =
        ReleaseContext::load(false, &args.relays, LoginMode::Required, signer).await?;
    let signer_public_key = require_current_maintainer(&context)?;

    let authors = selector_author(&args.app).map_or_else(
        || context.repo_ref.maintainers.clone(),
        |author| vec![author],
    );
    for author in &authors {
        context.add_author_relays(*author).await?;
    }
    let applications = load_applications(&mut context, authors, true).await?;
    let existing = resolve_application(&applications, &args.app)?.clone();

    context.require_owner_maintainer(&existing)?;
    if context.application_is_fully_linked(&existing) {
        return Err(coded_error_with_details(
            "application_already_linked",
            format!(
                "software application {:?} is already linked to every current repository coordinate",
                existing.identifier
            ),
            json!({ "event_id": existing.raw_event.id.to_hex() }),
        ));
    }

    context.refresh_repository().await?;
    require_current_maintainer(&context)?;
    let refreshed = load_exact_application(
        &mut context,
        existing.raw_event.pubkey,
        &existing.identifier,
        true,
    )
    .await?
    .ok_or_else(|| {
        coded_error(
            "concurrent_state_changed",
            "the software application disappeared during link preflight",
        )
    })?;
    ensure_same_application_revision(Some(&existing), Some(&refreshed))?;
    context.require_owner_maintainer(&refreshed)?;
    if context.application_is_fully_linked(&refreshed) {
        return Err(coded_error(
            "application_already_linked",
            "the software application became fully linked during preflight",
        ));
    }
    let existing = refreshed;

    let input = ApplicationInput {
        identifier: existing.identifier.clone(),
        name: existing.name.clone(),
        description: existing.description.clone(),
        summary: existing.summary.clone(),
        icon: existing.icon.clone(),
        images: dedup(existing.images.clone()),
        topics: dedup(existing.topics.clone()),
        website: existing.website.clone(),
        repository: existing.repository.clone(),
        repository_coordinates: merged_repository_coordinates(&context, Some(&existing)),
        platforms: dedup(existing.platforms.clone()),
        license: existing.license.clone(),
        extra_tags: existing.extra_tags.clone(),
        created_at: None,
    };
    let builder = application_event_builder(input).map_err(|error| {
        coded_error_with_details(
            "invalid_application_metadata",
            error.to_string(),
            json!({ "validation": error.issues }),
        )
    })?;
    let unsigned = finalize_ordered_unsigned(builder, signer_public_key, Some(&existing.raw_event))
        .context("failed to order software application link replacement")?;
    context.emit_human_warnings_before_signing(args.json);
    let signer = context
        .signer
        .clone()
        .context("application publication requires a signer")?;
    let event =
        sign_draft_event(unsigned, &signer, "link software application".to_string()).await?;
    require_signed_author(&event, signer_public_key)?;
    let application = SoftwareApplication::parse(&event)
        .context("signed software application failed validation")?;
    let relay_results = publish_application(&context, event.clone(), args.json).await?;

    let authority = context.authority(&application);
    let result = mutation_json(
        &context,
        "edited",
        Some(&existing.raw_event.id.to_hex()),
        &application,
        &relay_results,
    );
    let human = format!(
        "linked software application {} ({}) to {} repository coordinate{}\nevent: {}",
        application.name,
        application.identifier,
        context.repo_coordinate_keys().len(),
        if context.repo_coordinate_keys().len() == 1 {
            ""
        } else {
            "s"
        },
        event.id.to_bech32()?
    );

    Ok(CommandOutput::new(
        "release.app.link",
        &mut context,
        authority,
        result,
        human,
    ))
}

fn application_input(
    context: &ReleaseContext,
    args: &ReleaseAppInitArgs,
    identifier: &str,
    existing: Option<&SoftwareApplication>,
) -> Result<ApplicationInput> {
    let description_argument = if let Some(path) = &args.description_file {
        Some(std::fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read application description from {}",
                path.display()
            )
        })?)
    } else {
        args.description.clone()
    };
    let canonical_repository = context
        .repo_ref
        .to_nostr_git_url(&Some(&context.git_repo))
        .to_string();

    let name = args
        .name
        .clone()
        .or_else(|| existing.map(|application| application.name.clone()))
        .unwrap_or_else(|| context.repo_ref.name.clone());
    if name.is_empty() {
        return Err(coded_error(
            "metadata_confirmation_required",
            "application name is required; provide --name",
        ));
    }

    Ok(ApplicationInput {
        identifier: identifier.to_string(),
        name,
        description: patch_string(
            description_argument,
            args.clear_description,
            existing.map(|application| application.description.clone()),
            context.repo_ref.description.clone(),
        ),
        summary: patch_optional(
            args.summary.clone(),
            args.clear_summary,
            existing.and_then(|application| application.summary.clone()),
            None,
        ),
        icon: patch_optional(
            args.icon.clone(),
            args.clear_icon,
            existing.and_then(|application| application.icon.clone()),
            None,
        ),
        images: patch_values(
            &args.images,
            args.clear_images,
            existing.map(|application| application.images.clone()),
            Vec::new(),
        ),
        topics: patch_values(
            &args.topics,
            args.clear_topics,
            existing.map(|application| application.topics.clone()),
            context.repo_ref.hashtags.clone(),
        ),
        website: patch_optional(
            args.website.clone(),
            args.clear_website,
            existing.and_then(|application| application.website.clone()),
            context.repo_ref.web.first().cloned(),
        ),
        repository: patch_optional(
            args.repository.clone(),
            args.clear_repository,
            existing.and_then(|application| application.repository.clone()),
            Some(canonical_repository),
        ),
        repository_coordinates: merged_repository_coordinates(context, existing),
        platforms: dedup(
            patch_values(
                &args.platforms,
                args.clear_platforms,
                existing.map(|application| application.platforms.clone()),
                Vec::new(),
            )
            .into_iter()
            .map(|platform| platform.trim().to_string())
            .collect(),
        ),
        license: patch_optional(
            args.license.clone(),
            args.clear_license,
            existing.and_then(|application| application.license.clone()),
            None,
        ),
        extra_tags: existing.map_or_else(Vec::new, |application| application.extra_tags.clone()),
        created_at: None,
    })
}

fn patch_string(
    supplied: Option<String>,
    clear: bool,
    existing: Option<String>,
    create_default: String,
) -> String {
    if clear {
        String::new()
    } else {
        supplied.or(existing).unwrap_or(create_default)
    }
}

fn patch_optional<T>(
    supplied: Option<T>,
    clear: bool,
    existing: Option<T>,
    create_default: Option<T>,
) -> Option<T> {
    if clear {
        None
    } else {
        supplied.or(existing).or(create_default)
    }
}

fn patch_values<T: Clone + Eq + std::hash::Hash>(
    supplied: &[T],
    clear: bool,
    existing: Option<Vec<T>>,
    create_default: Vec<T>,
) -> Vec<T> {
    if clear {
        Vec::new()
    } else if !supplied.is_empty() {
        dedup(supplied.to_vec())
    } else {
        dedup(existing.unwrap_or(create_default))
    }
}

fn dedup<T: Clone + Eq + std::hash::Hash>(values: Vec<T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn merged_repository_coordinates(
    context: &ReleaseContext,
    existing: Option<&SoftwareApplication>,
) -> Vec<ngit::software_release::AddressPointer> {
    let current = context.ordered_repo_coordinates();
    let mut coordinates = Vec::with_capacity(
        current.len() + existing.map_or(0, |application| application.repository_coordinates.len()),
    );
    let mut seen = HashSet::new();
    for pointer in current {
        let key = coordinate_key(&pointer.coordinate);
        let pointer = existing
            .and_then(|application| {
                application
                    .repository_coordinates
                    .iter()
                    .find(|existing| coordinate_key(&existing.coordinate) == key)
            })
            .cloned()
            .unwrap_or(pointer);
        if seen.insert(key) {
            coordinates.push(pointer);
        }
    }
    if let Some(existing) = existing {
        for pointer in &existing.repository_coordinates {
            if seen.insert(coordinate_key(&pointer.coordinate)) {
                coordinates.push(pointer.clone());
            }
        }
    }
    coordinates
}

pub(super) fn application_input_from_repository(
    context: &ReleaseContext,
    identifier: &str,
    platforms: Vec<String>,
) -> Result<ApplicationInput> {
    if context.repo_ref.name.is_empty() {
        return Err(coded_error(
            "metadata_confirmation_required",
            "application name is required; create the application explicitly with release app init --name",
        ));
    }
    let canonical_repository = context
        .repo_ref
        .to_nostr_git_url(&Some(&context.git_repo))
        .to_string();
    Ok(ApplicationInput {
        identifier: identifier.to_owned(),
        name: context.repo_ref.name.clone(),
        description: context.repo_ref.description.clone(),
        summary: None,
        icon: None,
        images: Vec::new(),
        topics: dedup(context.repo_ref.hashtags.clone()),
        website: context.repo_ref.web.first().cloned(),
        repository: Some(canonical_repository),
        repository_coordinates: context.ordered_repo_coordinates(),
        platforms: dedup(platforms),
        license: None,
        extra_tags: Vec::new(),
        created_at: None,
    })
}

pub(super) fn add_metadata_warnings(
    context: &mut ReleaseContext,
    input: &ApplicationInput,
    strict: bool,
) -> Result<()> {
    let mut missing = Vec::new();
    if input.summary.is_none() {
        missing.push("summary");
    }
    if input.icon.is_none() {
        missing.push("icon");
    }
    if input.website.is_none() {
        missing.push("website");
    }
    if input.repository.is_none() {
        missing.push("repository");
    }
    if input.license.is_none() {
        missing.push("license");
    }
    if input.platforms.is_empty() {
        missing.push("platforms");
    }
    if missing.is_empty() {
        return Ok(());
    }

    let message = format!(
        "application metadata is missing recommended field{}: {}",
        if missing.len() == 1 { "" } else { "s" },
        missing.join(", ")
    );
    if strict {
        return Err(coded_error_with_details(
            "metadata_confirmation_required",
            message,
            json!({ "missing_fields": missing }),
        ));
    }
    context.warnings.push(
        WarningJson::new("application_metadata_incomplete", message)
            .with_details(json!({ "missing_fields": missing })),
    );
    Ok(())
}

fn require_current_maintainer(context: &ReleaseContext) -> Result<PublicKey> {
    let signer = context
        .current_signer()
        .ok_or_else(|| coded_error("not_logged_in", "application publication requires login"))?;
    if !context.repo_ref.maintainers.contains(&signer) {
        return Err(coded_error_with_details(
            "not_repository_maintainer",
            format!(
                "current signer {} is not a maintainer of this repository",
                signer.to_bech32()?
            ),
            json!({ "current_signer": signer.to_hex() }),
        ));
    }
    Ok(signer)
}

fn require_signed_author(event: &nostr::prelude::Event, expected: PublicKey) -> Result<()> {
    if event.pubkey == expected {
        return Ok(());
    }
    Err(coded_error_with_details(
        "application_author_mismatch",
        "signer returned a software application for a different author",
        json!({
            "expected_author": expected.to_hex(),
            "event_author": event.pubkey.to_hex(),
        }),
    ))
}

async fn load_exact_application(
    context: &mut ReleaseContext,
    author: PublicKey,
    identifier: &str,
    strict: bool,
) -> Result<Option<SoftwareApplication>> {
    context.add_author_relays(author).await?;
    let filter = Filter::new()
        .kind(SOFTWARE_APPLICATION_KIND)
        .author(author)
        .identifier(identifier);
    let events = context.query(vec![filter], strict).await?;
    let Some(event) = latest_event(events.iter()) else {
        return Ok(None);
    };
    SoftwareApplication::parse(event)
        .map(Some)
        .map_err(|error| {
            coded_error_with_details(
                "invalid_application_metadata",
                format!("existing software application {identifier:?} is invalid: {error}"),
                json!({
                    "event_id": event.id.to_hex(),
                    "validation": error.issues,
                }),
            )
        })
}

fn selector_author(selector: &str) -> Option<PublicKey> {
    Nip19Coordinate::from_bech32(selector)
        .ok()
        .map(|pointer| pointer.coordinate.public_key)
        .or_else(|| {
            Coordinate::parse(selector)
                .ok()
                .map(|coordinate| coordinate.public_key)
        })
}

fn ensure_same_application_revision(
    expected: Option<&SoftwareApplication>,
    current: Option<&SoftwareApplication>,
) -> Result<()> {
    let expected_id = expected.map(|application| application.raw_event.id);
    let current_id = current.map(|application| application.raw_event.id);
    if expected_id == current_id {
        return Ok(());
    }
    Err(coded_error_with_details(
        "concurrent_state_changed",
        "the software application changed during preflight; inspect it and retry",
        json!({
            "expected_event_id": expected_id.map(|event_id| event_id.to_hex()),
            "current_event_id": current_id.map(|event_id| event_id.to_hex()),
        }),
    ))
}

async fn publish_application(
    context: &ReleaseContext,
    event: nostr::prelude::Event,
    json_output: bool,
) -> Result<Vec<(String, bool)>> {
    let (write_relays, repo_relays) = context.publication_relays();
    let results = send_events(
        &context.client,
        Some(context.git_repo_path()?),
        vec![event],
        write_relays,
        repo_relays,
        !json_output,
        json_output,
    )
    .await
    .context("failed to publish software application")?;
    if !results.iter().any(|(_, accepted)| *accepted) {
        return Err(coded_error_with_details(
            "publication_failed",
            "no relay accepted the software application",
            json!({
                "relays": results
                    .iter()
                    .map(|(url, accepted)| relay_json(url, *accepted))
                    .collect::<Vec<_>>()
            }),
        ));
    }
    Ok(results)
}

fn mutation_json(
    context: &ReleaseContext,
    operation: &str,
    previous_event_id: Option<&str>,
    application: &SoftwareApplication,
    relay_results: &[(String, bool)],
) -> Value {
    json!({
        "operation": operation,
        "previous_event_id": previous_event_id,
        "coordinate": coordinate_key(&application.coordinate()),
        "event_id": application.raw_event.id.to_hex(),
        "application": application_json(context, application),
        "events": [{
            "entity": "application",
            "event_id": application.raw_event.id.to_hex(),
            "relays": relay_results
                .iter()
                .map(|(url, accepted)| relay_json(url, *accepted))
                .collect::<Vec<_>>(),
        }],
        "orphan_asset_ids": Vec::<String>::new(),
    })
}

fn relay_json(url: &str, accepted: bool) -> Value {
    json!({
        "url": url,
        "status": if accepted { "accepted" } else { "rejected" },
        "message": null,
    })
}

#[cfg(test)]
mod tests {
    use super::{dedup, patch_optional, patch_string, patch_values};

    #[test]
    fn omitted_edit_values_preserve_existing_metadata() {
        assert_eq!(
            patch_string(None, false, Some("old".to_string()), "default".to_string()),
            "old"
        );
        assert_eq!(
            patch_optional(None, false, Some("old"), Some("default")),
            Some("old")
        );
        assert_eq!(
            patch_values(
                &[],
                false,
                Some(vec!["old".to_string()]),
                vec!["default".to_string()],
            ),
            vec!["old".to_string()]
        );
    }

    #[test]
    fn supplied_and_clear_edit_values_are_explicit() {
        assert_eq!(
            patch_string(
                Some("new".to_string()),
                false,
                Some("old".to_string()),
                String::new(),
            ),
            "new"
        );
        assert_eq!(patch_optional(Some("new"), true, Some("old"), None), None);
        assert!(patch_values(&["new"], true, Some(vec!["old"]), vec![]).is_empty());
    }

    #[test]
    fn managed_repeated_values_are_canonicalized() {
        assert_eq!(dedup(vec!["one", "two", "one"]), vec!["one", "two"]);
    }
}
