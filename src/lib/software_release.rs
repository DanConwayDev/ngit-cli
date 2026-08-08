//! NIP-82 software application, release, and asset events.
//!
//! This module deliberately models NIP-82 independently from the release CLI.
//! Parsers are strict: callers can use [`validate_application`] to retain and
//! display the raw event alongside structured issues, and only construct a
//! typed value after the event passes validation.

use std::{collections::BTreeSet, error::Error, fmt};

use nostr::prelude::{Event, EventBuilder, Kind, RelayUrl, Tag, Timestamp, Url, nip01::Coordinate};
use serde::Serialize;

pub const SOFTWARE_APPLICATION_KIND: Kind = Kind::Custom(32_267);
pub const SOFTWARE_RELEASE_KIND: Kind = Kind::Custom(30_063);
pub const SOFTWARE_ASSET_KIND: Kind = Kind::Custom(3_063);
const GIT_REPOSITORY_KIND: Kind = Kind::Custom(30_617);

/// Stable categories suitable for human diagnostics and JSON output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    IncorrectKind,
    MissingTag,
    DuplicateTag,
    MalformedTag,
    EmptyValue,
    InvalidValue,
    InvalidCoordinate,
    InvalidRepositoryCoordinate,
    InvalidApplicationCoordinate,
    ApplicationAuthorMismatch,
    IdentifierMismatch,
    ReleaseIdentifierMismatch,
    InvalidEventId,
    DuplicateAsset,
    InvalidUrl,
    InvalidRelayHint,
    InvalidMime,
    InvalidSha256,
    InvalidInteger,
    NonEmptyAssetContent,
    MissingAndroidMetadata,
    DuplicatePlatform,
    PlatformUnionMismatch,
    MissingReferencedAsset,
    UnreferencedAsset,
    InvalidAssetAuthor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationIssue {
    pub code: ValidationCode,
    pub field: Option<String>,
    pub message: String,
}

impl ValidationIssue {
    fn new(code: ValidationCode, field: impl Into<Option<&'static str>>, message: String) -> Self {
        Self {
            code,
            field: field.into().map(str::to_owned),
            message,
        }
    }

    fn field(code: ValidationCode, field: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, Some(field), message.into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftwareEventType {
    Application,
    Release,
    Asset,
}

impl fmt::Display for SoftwareEventType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Application => formatter.write_str("software application"),
            Self::Release => formatter.write_str("software release"),
            Self::Asset => formatter.write_str("software asset"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    pub event_type: SoftwareEventType,
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid {} event: {}",
            self.event_type,
            self.issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

impl Error for ValidationError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AddressPointer {
    pub coordinate: Coordinate,
    pub relay_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SoftwareApplication {
    pub raw_event: Event,
    pub identifier: String,
    pub name: String,
    pub description: String,
    pub summary: Option<String>,
    pub icon: Option<String>,
    pub images: Vec<String>,
    pub topics: Vec<String>,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub repository_coordinates: Vec<AddressPointer>,
    pub platforms: Vec<String>,
    pub license: Option<String>,
    pub extra_tags: Vec<Tag>,
}

impl SoftwareApplication {
    pub fn parse(event: &Event) -> Result<Self, ValidationError> {
        let issues = validate_application(event);
        if !issues.is_empty() {
            return Err(ValidationError {
                event_type: SoftwareEventType::Application,
                issues,
            });
        }

        Ok(Self {
            raw_event: event.clone(),
            identifier: required_value(event, "d"),
            name: required_value(event, "name"),
            description: event.content.clone(),
            summary: optional_value(event, "summary"),
            icon: optional_value(event, "icon"),
            images: repeated_values(event, "image"),
            topics: repeated_values(event, "t"),
            website: optional_value(event, "url"),
            repository: optional_value(event, "repository"),
            repository_coordinates: address_pointers(event, "a"),
            platforms: unique_values(event, "f"),
            license: optional_value(event, "license"),
            extra_tags: extra_tags(event, is_application_tag),
        })
    }

    pub fn coordinate(&self) -> Coordinate {
        Coordinate::new(SOFTWARE_APPLICATION_KIND, self.raw_event.pubkey)
            .identifier(self.identifier.clone())
    }
}

pub fn validate_application(event: &Event) -> Vec<ValidationIssue> {
    let mut issues = validate_kind(event, SOFTWARE_APPLICATION_KIND);
    validate_single_tag(event, "d", true, &mut issues);
    validate_single_tag(event, "name", true, &mut issues);
    for field in ["summary", "icon", "url", "repository", "license"] {
        validate_single_tag(event, field, false, &mut issues);
    }
    for field in ["image", "t", "f"] {
        validate_repeated_tag(event, field, false, &mut issues);
    }
    validate_addresses(event, "a", GIT_REPOSITORY_KIND, false, &mut issues);
    validate_url_tag(event, "icon", &mut issues);
    validate_url_tag(event, "image", &mut issues);
    validate_url_tag(event, "url", &mut issues);
    validate_duplicate_values(event, "f", &mut issues);
    issues
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationInput {
    pub identifier: String,
    pub name: String,
    pub description: String,
    pub summary: Option<String>,
    pub icon: Option<String>,
    pub images: Vec<String>,
    pub topics: Vec<String>,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub repository_coordinates: Vec<AddressPointer>,
    pub platforms: Vec<String>,
    pub license: Option<String>,
    pub extra_tags: Vec<Tag>,
    pub created_at: Option<Timestamp>,
}

pub fn application_event_builder(input: ApplicationInput) -> Result<EventBuilder, ValidationError> {
    let mut issues = Vec::new();
    validate_input_required("d", &input.identifier, &mut issues);
    validate_input_required("name", &input.name, &mut issues);
    for (field, value) in [
        ("summary", input.summary.as_deref()),
        ("icon", input.icon.as_deref()),
        ("url", input.website.as_deref()),
        ("repository", input.repository.as_deref()),
        ("license", input.license.as_deref()),
    ] {
        validate_optional_nonempty(field, value, &mut issues);
    }
    validate_input_values("image", &input.images, &mut issues);
    validate_input_values("t", &input.topics, &mut issues);
    validate_optional_url("icon", input.icon.as_deref(), &mut issues);
    validate_optional_url("url", input.website.as_deref(), &mut issues);
    for image in &input.images {
        validate_optional_url("image", Some(image), &mut issues);
    }
    for address in &input.repository_coordinates {
        validate_input_address(address, GIT_REPOSITORY_KIND, "a", &mut issues);
    }
    reject_duplicate_strings("f", &input.platforms, &mut issues);
    if !issues.is_empty() {
        return Err(ValidationError {
            event_type: SoftwareEventType::Application,
            issues,
        });
    }

    let mut tags = vec![tag(["d", &input.identifier]), tag(["name", &input.name])];
    push_optional(&mut tags, "summary", input.summary);
    push_optional(&mut tags, "icon", input.icon);
    push_repeated(&mut tags, "image", input.images);
    push_repeated(&mut tags, "t", input.topics);
    push_optional(&mut tags, "url", input.website);
    push_optional(&mut tags, "repository", input.repository);
    for address in input.repository_coordinates {
        tags.push(address_tag("a", address));
    }
    push_repeated(&mut tags, "f", sorted_unique(input.platforms));
    push_optional(&mut tags, "license", input.license);
    tags.extend(
        input
            .extra_tags
            .into_iter()
            .filter(|tag| !is_application_tag(tag.kind())),
    );

    let mut builder = EventBuilder::new(SOFTWARE_APPLICATION_KIND, input.description).tags(tags);
    if let Some(created_at) = input.created_at {
        builder = builder.custom_created_at(created_at);
    }
    Ok(builder)
}

fn validate_kind(event: &Event, expected: Kind) -> Vec<ValidationIssue> {
    if event.kind == expected {
        Vec::new()
    } else {
        vec![ValidationIssue::new(
            ValidationCode::IncorrectKind,
            None,
            format!("expected event kind {expected}, found {}", event.kind),
        )]
    }
}

fn validate_single_tag(
    event: &Event,
    name: &'static str,
    required: bool,
    issues: &mut Vec<ValidationIssue>,
) {
    let tags: Vec<&Tag> = event.tags.iter().filter(|tag| tag.kind() == name).collect();
    if required && tags.is_empty() {
        issues.push(ValidationIssue::field(
            ValidationCode::MissingTag,
            name,
            format!("missing required {name} tag"),
        ));
        return;
    }
    if tags.len() > 1 {
        issues.push(ValidationIssue::field(
            ValidationCode::DuplicateTag,
            name,
            format!("{name} must occur at most once"),
        ));
    }
    for tag in tags {
        validate_tag_shape(tag, name, false, issues);
    }
}

fn validate_repeated_tag(
    event: &Event,
    name: &'static str,
    required: bool,
    issues: &mut Vec<ValidationIssue>,
) {
    let tags: Vec<&Tag> = event.tags.iter().filter(|tag| tag.kind() == name).collect();
    if required && tags.is_empty() {
        issues.push(ValidationIssue::field(
            ValidationCode::MissingTag,
            name,
            format!("missing required {name} tag"),
        ));
    }
    for tag in tags {
        validate_tag_shape(tag, name, false, issues);
    }
}

fn validate_addresses(
    event: &Event,
    name: &'static str,
    kind: Kind,
    exactly_one: bool,
    issues: &mut Vec<ValidationIssue>,
) {
    let tags: Vec<&Tag> = event.tags.iter().filter(|tag| tag.kind() == name).collect();
    if exactly_one && tags.is_empty() {
        issues.push(ValidationIssue::field(
            ValidationCode::MissingTag,
            name,
            format!("missing required {name} tag"),
        ));
    }
    if exactly_one && tags.len() > 1 {
        issues.push(ValidationIssue::field(
            ValidationCode::DuplicateTag,
            name,
            format!("{name} must occur exactly once"),
        ));
    }
    for tag in tags {
        validate_tag_shape(tag, name, true, issues);
        let Some(value) = tag.as_slice().get(1) else {
            continue;
        };
        match Coordinate::parse(value) {
            Ok(coordinate) if coordinate.kind == kind && !coordinate.identifier.is_empty() => {}
            Ok(coordinate) => issues.push(ValidationIssue::field(
                if kind == GIT_REPOSITORY_KIND {
                    ValidationCode::InvalidRepositoryCoordinate
                } else {
                    ValidationCode::InvalidApplicationCoordinate
                },
                name,
                format!("expected kind {kind} address, found {coordinate}"),
            )),
            Err(_) => issues.push(ValidationIssue::field(
                ValidationCode::InvalidCoordinate,
                name,
                format!("invalid coordinate {value:?}"),
            )),
        }
        validate_relay_hint(tag.as_slice().get(2).map(String::as_str), name, issues);
    }
}

fn validate_tag_shape(
    tag: &Tag,
    name: &'static str,
    allows_hint: bool,
    issues: &mut Vec<ValidationIssue>,
) {
    let valid_len = if allows_hint {
        (2..=3).contains(&tag.len())
    } else {
        tag.len() == 2
    };
    if !valid_len {
        issues.push(ValidationIssue::field(
            ValidationCode::MalformedTag,
            name,
            format!("malformed {name} tag"),
        ));
    }
    if tag.as_slice().get(1).is_some_and(String::is_empty) {
        issues.push(ValidationIssue::field(
            ValidationCode::EmptyValue,
            name,
            format!("{name} tag value must not be empty"),
        ));
    } else if let Some(value) = tag.as_slice().get(1) {
        validate_clean_value(name, value, issues);
    }
}

fn validate_duplicate_values(event: &Event, name: &'static str, issues: &mut Vec<ValidationIssue>) {
    let mut seen = BTreeSet::new();
    for value in values(event, name) {
        if !seen.insert(value) {
            issues.push(ValidationIssue::field(
                if name == "e" {
                    ValidationCode::DuplicateAsset
                } else {
                    ValidationCode::DuplicatePlatform
                },
                name,
                format!("duplicate {name} value {value:?}"),
            ));
        }
    }
}

fn validate_url_tag(event: &Event, name: &'static str, issues: &mut Vec<ValidationIssue>) {
    for value in values(event, name) {
        validate_optional_url(name, Some(value), issues);
    }
}

fn validate_optional_url(
    name: &'static str,
    value: Option<&str>,
    issues: &mut Vec<ValidationIssue>,
) {
    if let Some(value) = value {
        if Url::parse(value).is_err() {
            issues.push(ValidationIssue::field(
                ValidationCode::InvalidUrl,
                name,
                format!("invalid URL {value:?}"),
            ));
        }
    }
}

fn validate_relay_hint(
    value: Option<&str>,
    field: &'static str,
    issues: &mut Vec<ValidationIssue>,
) {
    if let Some(value) = value {
        if RelayUrl::parse(value).is_err() {
            issues.push(ValidationIssue::field(
                ValidationCode::InvalidRelayHint,
                field,
                format!("invalid relay hint {value:?}"),
            ));
        }
    }
}

fn validate_input_address(
    address: &AddressPointer,
    kind: Kind,
    field: &'static str,
    issues: &mut Vec<ValidationIssue>,
) {
    if address.coordinate.kind != kind || address.coordinate.identifier.is_empty() {
        issues.push(ValidationIssue::field(
            if kind == GIT_REPOSITORY_KIND {
                ValidationCode::InvalidRepositoryCoordinate
            } else {
                ValidationCode::InvalidApplicationCoordinate
            },
            field,
            format!("expected non-empty kind {kind} address"),
        ));
    }
    validate_relay_hint(address.relay_hint.as_deref(), field, issues);
}

fn validate_input_required(field: &'static str, value: &str, issues: &mut Vec<ValidationIssue>) {
    if value.is_empty() {
        issues.push(ValidationIssue::field(
            ValidationCode::EmptyValue,
            field,
            format!("{field} must not be empty"),
        ));
    } else {
        validate_clean_value(field, value, issues);
    }
}

fn validate_optional_nonempty(
    field: &'static str,
    value: Option<&str>,
    issues: &mut Vec<ValidationIssue>,
) {
    if let Some(value) = value {
        if value.is_empty() {
            issues.push(ValidationIssue::field(
                ValidationCode::EmptyValue,
                field,
                format!("{field} must not be empty when supplied"),
            ));
        } else {
            validate_clean_value(field, value, issues);
        }
    }
}

fn validate_input_values(
    field: &'static str,
    values: &[String],
    issues: &mut Vec<ValidationIssue>,
) {
    for value in values {
        if value.is_empty() {
            issues.push(ValidationIssue::field(
                ValidationCode::EmptyValue,
                field,
                format!("{field} must not contain an empty value"),
            ));
        } else {
            validate_clean_value(field, value, issues);
        }
    }
}

fn reject_duplicate_strings(
    field: &'static str,
    values: &[String],
    issues: &mut Vec<ValidationIssue>,
) {
    let mut seen = BTreeSet::new();
    for value in values {
        if value.is_empty() {
            issues.push(ValidationIssue::field(
                ValidationCode::EmptyValue,
                field,
                format!("{field} must not contain an empty value"),
            ));
        } else if !seen.insert(value) {
            issues.push(ValidationIssue::field(
                ValidationCode::DuplicatePlatform,
                field,
                format!("duplicate {field} value {value:?}"),
            ));
        } else {
            validate_clean_value(field, value, issues);
        }
    }
}

fn validate_clean_value(field: &'static str, value: &str, issues: &mut Vec<ValidationIssue>) {
    if value.trim() != value || value.chars().any(char::is_control) {
        issues.push(ValidationIssue::field(
            ValidationCode::InvalidValue,
            field,
            format!("{field} must not contain surrounding whitespace or control characters"),
        ));
    }
}

fn first_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    values(event, name).next()
}

fn values<'a>(event: &'a Event, name: &str) -> impl Iterator<Item = &'a str> {
    let name = name.to_string();
    event
        .tags
        .iter()
        .filter(move |tag| tag.kind() == name)
        .filter_map(Tag::content)
}

fn required_value(event: &Event, name: &str) -> String {
    first_value(event, name)
        .expect("validated event has required tag")
        .to_string()
}

fn optional_value(event: &Event, name: &str) -> Option<String> {
    first_value(event, name).map(str::to_owned)
}

fn repeated_values(event: &Event, name: &str) -> Vec<String> {
    values(event, name).map(str::to_owned).collect()
}

fn unique_values(event: &Event, name: &str) -> Vec<String> {
    sorted_unique(repeated_values(event, name))
}

fn address_pointers(event: &Event, name: &str) -> Vec<AddressPointer> {
    event
        .tags
        .iter()
        .filter(|tag| tag.kind() == name)
        .filter_map(|tag| {
            Some(AddressPointer {
                coordinate: Coordinate::parse(tag.content()?).ok()?,
                relay_hint: tag.as_slice().get(2).cloned(),
            })
        })
        .collect()
}

fn extra_tags(event: &Event, known: impl Fn(&str) -> bool) -> Vec<Tag> {
    event
        .tags
        .iter()
        .filter(|tag| !known(tag.kind()))
        .cloned()
        .collect()
}

fn sorted_unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn tag<I, S>(fields: I) -> Tag
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Tag::parse(fields).expect("NIP-82 tags always have a name")
}

fn address_tag(name: &str, address: AddressPointer) -> Tag {
    let mut fields = vec![name.to_string(), address.coordinate.to_string()];
    if let Some(hint) = address.relay_hint {
        fields.push(hint);
    }
    tag(fields)
}

fn push_optional(tags: &mut Vec<Tag>, name: &str, value: Option<String>) {
    if let Some(value) = value {
        tags.push(tag([name.to_string(), value]));
    }
}

fn push_repeated(tags: &mut Vec<Tag>, name: &str, values: Vec<String>) {
    tags.extend(
        values
            .into_iter()
            .map(|value| tag([name.to_string(), value])),
    );
}

fn is_application_tag(name: &str) -> bool {
    matches!(
        name,
        "d" | "name"
            | "summary"
            | "icon"
            | "image"
            | "t"
            | "url"
            | "repository"
            | "a"
            | "f"
            | "license"
    )
}

#[cfg(test)]
mod tests {
    use nostr::prelude::{FinalizeEvent, Keys};

    use super::*;

    const SECRET_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    fn keys() -> Keys {
        Keys::parse(SECRET_KEY).unwrap()
    }

    #[test]
    fn application_round_trips_all_fields_and_foreign_tags() {
        let repository = AddressPointer {
            coordinate: Coordinate::new(GIT_REPOSITORY_KIND, keys().public_key())
                .identifier("ngit"),
            relay_hint: Some("wss://relay.example.com".to_string()),
        };
        let foreign = tag(["future", "value", "extension"]);
        let event = application_event_builder(ApplicationInput {
            identifier: "org.ngit.cli".to_string(),
            name: "ngit".to_string(),
            description: "Nostr-native git collaboration".to_string(),
            summary: Some("git over nostr".to_string()),
            icon: Some("https://example.com/icon.png".to_string()),
            images: vec!["https://example.com/screenshot.png".to_string()],
            topics: vec!["git".to_string(), "nostr".to_string()],
            website: Some("https://ngit.dev".to_string()),
            repository: Some("nostr://dan@example.com/ngit".to_string()),
            repository_coordinates: vec![repository],
            platforms: vec!["linux-x86_64".to_string()],
            license: Some("MIT".to_string()),
            extra_tags: vec![foreign.clone(), tag(["name", "smuggled name"])],
            ..Default::default()
        })
        .unwrap()
        .finalize(&keys())
        .unwrap();

        let parsed = SoftwareApplication::parse(&event).unwrap();
        assert_eq!(parsed.identifier, "org.ngit.cli");
        assert_eq!(parsed.name, "ngit");
        assert_eq!(parsed.repository_coordinates.len(), 1);
        assert_eq!(parsed.extra_tags, vec![foreign]);
        assert_eq!(values(&event, "name").collect::<Vec<_>>(), vec!["ngit"]);
    }
}
