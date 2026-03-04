use anyhow::{Context, Result, bail};
use ngit::client::{Params, send_events, sign_event};
use nostr::{EventBuilder, Tag, TagStandard, ToBech32, nips::nip19::Nip19Event};
use nostr_sdk::Kind;

use crate::{
    client::{Client, Connect, fetching_with_report, get_repo_ref_from_cache},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates_when_remote_unknown,
};

pub async fn launch(
    title: Option<String>,
    body: Option<String>,
    labels: Vec<String>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    // Resolve title — required
    let title = match title {
        Some(t) if !t.trim().is_empty() => t,
        _ => bail!("--title is required to create an issue"),
    };

    // Body defaults to empty string if not provided
    let body = body.unwrap_or_default();

    // Login
    let (signer, user_ref, _) =
        login::login_or_signup(&Some(&git_repo), &None, &None, Some(&client), true).await?;

    // Build NIP-34 GitIssue event (kind 1621)
    // Tags:
    //   - `a` coordinate tags for each maintainer's repo announcement
    //   - `subject` — issue title
    //   - `t` — hashtag labels
    //   - `alt` — human-readable summary
    let mut tags: Vec<Tag> = vec![];

    // Repo coordinate tags (one per maintainer)
    for coord in repo_ref.coordinates() {
        tags.push(Tag::from_standardized(TagStandard::Coordinate {
            coordinate: coord.coordinate.clone(),
            relay_url: coord.relays.first().cloned(),
            uppercase: false,
        }));
    }

    // Subject (title)
    tags.push(Tag::parse(vec!["subject".to_string(), title.clone()])?);

    // Hashtag labels
    for label in &labels {
        tags.push(Tag::hashtag(label));
    }

    // Alt text
    tags.push(Tag::custom(
        nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
        vec![format!("git issue: {title}")],
    ));

    // Maintainer p-tags (so they get notified)
    for pk in &repo_ref.maintainers {
        tags.push(Tag::public_key(*pk));
    }

    let issue_event = sign_event(
        EventBuilder::new(Kind::GitIssue, body).tags(tags),
        &signer,
        "create issue".to_string(),
    )
    .await?;

    let event_id = issue_event.id;

    let mut client = client;
    client.set_signer(signer).await;

    send_events(
        &client,
        Some(git_repo_path),
        vec![issue_event],
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        true,
        false,
    )
    .await?;

    let event_bech32 = if let Some(relay) = repo_ref.relays.first() {
        Nip19Event {
            event_id,
            relays: vec![relay.clone()],
            author: None,
            kind: None,
        }
        .to_bech32()?
    } else {
        event_id.to_bech32()?
    };

    println!("issue created: {event_id}");
    let dim = console::Style::new().color256(247);
    println!(
        "{}",
        dim.apply_to(format!(
            "view in gitworkshop.dev: https://gitworkshop.dev/{}",
            &event_bech32,
        ))
    );
    Ok(())
}
