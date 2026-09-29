use anyhow::{Context, Result, bail};
use ngit::{
    cli_interactor::{Interactor, InteractorPrompt, PromptInputParms},
    client::{Params, send_events, sign_event},
    content_tags::{dedup_tags, tags_from_content},
    repo_lookup::RepositoryLookup,
};
use nostr::prelude::{EventBuilder, Kind, Tag, ToBech32, nip01::Nip01Tag, nip19::Nip19Event};

use crate::{
    cli::SignerParams,
    client::{Client, Connect, warn_if_invited_as_maintainer},
    git::{Repo, RepoActions},
    login,
    repo_ref::get_resolved_repo_coordinate_for_publishing,
    sub_commands::repository_fetch::fetching_with_account,
};

struct IssueDraft {
    subject: String,
    body: String,
    labels: Vec<String>,
}

fn compose_issue(
    subject: Option<String>,
    body: Option<String>,
    labels: Vec<String>,
    interactive: bool,
    mut input: impl FnMut(PromptInputParms) -> Result<String>,
) -> Result<IssueDraft> {
    let subject = match subject {
        Some(subject) => subject,
        None if interactive => input(
            PromptInputParms::default()
                .with_prompt("issue subject")
                .with_flag_name("--subject"),
        )?,
        None => bail!("provide `--subject` to create an issue, or use `--interactive` to enter it"),
    };
    if subject.trim().is_empty() {
        bail!(
            "the issue subject cannot be empty; provide `--subject` or enter a subject at the prompt"
        );
    }
    let body = match body {
        Some(body) => body,
        None if interactive => input(
            PromptInputParms::default()
                .with_prompt("issue body (Nostr links create mentions)")
                .with_flag_name("--body")
                .optional(),
        )?,
        None => String::new(),
    };
    let labels = if interactive && labels.is_empty() {
        input(
            PromptInputParms::default()
                .with_prompt("labels (comma-separated, optional)")
                .with_flag_name("--label")
                .optional(),
        )?
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect()
    } else {
        labels
    };
    Ok(IssueDraft {
        subject,
        body,
        labels,
    })
}

pub async fn launch(
    title: Option<String>,
    body: Option<String>,
    labels: Vec<String>,
    interactive: bool,
    signer: SignerParams<'_>,
) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));
    let selected_repo = get_resolved_repo_coordinate_for_publishing(&git_repo, &mut client).await?;
    let mut lookup = RepositoryLookup::from_resolved(&git_repo, &selected_repo)?;
    let mut repo_coordinates = selected_repo.coordinate;

    fetching_with_account(
        &git_repo,
        git_repo_path,
        &mut client,
        &mut repo_coordinates,
        &mut lookup,
        signer,
    )
    .await?;

    let repo_ref = lookup.load(git_repo_path).await?;
    warn_if_invited_as_maintainer(git_repo_path, &repo_ref).await;

    let interactor = Interactor::default();
    let IssueDraft {
        subject: title,
        body,
        labels,
    } = compose_issue(title, body, labels, interactive, |p| interactor.input(p))?;

    // Login
    let (event_signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer.info,
        signer.password,
        Some(&client),
        true,
    )
    .await?;

    // Build NIP-34 GitIssue event (kind 1621)
    // Tags:
    //   - `a` coordinate tags for each maintainer's repo announcement
    //   - `subject` — issue title
    //   - `t` — hashtag labels
    //   - `alt` — human-readable summary
    let mut tags: Vec<Tag> = vec![];

    // Repo coordinate tags (one per maintainer)
    for coord in repo_ref.coordinates() {
        tags.push(Tag::from(Nip01Tag::Coordinate {
            coordinate: coord.coordinate.clone(),
            relay_hint: coord.relays.first().cloned(),
        }));
    }

    // Subject (title)
    tags.push(Tag::parse(vec!["subject".to_string(), title.clone()])?);

    // Hashtag labels
    for label in &labels {
        tags.push(Tag::hashtag(label));
    }

    // Alt text
    tags.push(Tag::parse(["alt", &format!("git issue: {title}")])?);

    // Maintainer p-tags (so they get notified)
    for pk in &repo_ref.maintainers {
        tags.push(Tag::public_key(*pk));
    }

    // NIP-21 mention tags: q tags for cited events/addresses, p tags for cited
    // pubkeys
    tags.extend(tags_from_content(&body, Some(git_repo_path)).await?);
    let tags = dedup_tags(tags);

    let issue_event = sign_event(
        EventBuilder::new(Kind::GitIssue, body).tags(tags),
        &event_signer,
        "create issue".to_string(),
    )
    .await?;

    let event_id = issue_event.id;

    let mut client = client;
    client.set_signer(event_signer).await;

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

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "created",
            "entity": "issue",
            "id": event_bech32,
            "subject": title,
            "labels": labels,
            "url": format!("https://gitworkshop.dev/{event_bech32}"),
        }));
    }

    println!("issue created: {event_id}");
    let dim = console::Style::new().color256(247);
    println!(
        "{}",
        dim.apply_to(format!(
            "view in gitworkshop.dev: https://gitworkshop.dev/{event_bech32}",
        ))
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_creation_collects_missing_fields() -> Result<()> {
        let mut answers = [
            "a subject",
            "body with nostr:note1example",
            "bug, help wanted, ",
        ]
        .into_iter();
        let mut fields = Vec::new();
        let draft = compose_issue(None, None, vec![], true, |params| {
            fields.push(params.flag_name.unwrap());
            Ok(answers.next().unwrap().into())
        })?;
        assert_eq!(fields, ["--subject", "--body", "--label"]);
        assert_eq!(draft.subject, "a subject");
        assert_eq!(draft.body, "body with nostr:note1example");
        assert_eq!(draft.labels, ["bug", "help wanted"]);
        Ok(())
    }

    #[test]
    fn supplied_values_are_preserved_and_subject_alone_still_prompts() -> Result<()> {
        for interactive in [false, true] {
            let draft = compose_issue(
                Some("subject".into()),
                Some(String::new()),
                vec!["bug".into()],
                interactive,
                |_| panic!("explicit values must not prompt"),
            )?;
            assert_eq!(draft.subject, "subject");
            assert!(draft.body.is_empty());
            assert_eq!(draft.labels, ["bug"]);
        }
        let mut fields = Vec::new();
        let draft = compose_issue(Some("subject".into()), None, vec![], true, |params| {
            fields.push(params.flag_name.unwrap());
            Ok("entered".into())
        })?;
        assert_eq!(fields, ["--body", "--label"]);
        assert_eq!(draft.body, "entered");
        Ok(())
    }

    #[test]
    fn noninteractive_creation_never_prompts() -> Result<()> {
        for subject in [None, Some("  ".into())] {
            let error = compose_issue(subject, None, vec![], false, |_| panic!("must not prompt"))
                .err()
                .unwrap();
            assert!(error.to_string().contains("--subject"));
        }
        let draft = compose_issue(Some("subject".into()), None, vec![], false, |_| {
            panic!("must not prompt")
        })?;
        assert!(draft.body.is_empty());
        assert!(draft.labels.is_empty());
        Ok(())
    }

    #[test]
    fn cancellation_and_blank_subject_do_not_produce_a_draft() {
        let mut calls = 0;
        let result = compose_issue(Some("subject".into()), None, vec![], true, |_| {
            calls += 1;
            bail!("cancelled")
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        let mut calls = 0;
        let result = compose_issue(None, None, vec![], true, |_| {
            calls += 1;
            Ok("  ".into())
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}
