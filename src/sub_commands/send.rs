use std::{str::FromStr, time::Duration};

use anyhow::{bail, Context, Result};
use console::Style;
use futures::future::join_all;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use nostr::{
    nips::{nip01::Coordinate, nip19::Nip19},
    EventBuilder, FromBech32, Marker, Tag, TagKind, UncheckedUrl,
};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;

use super::list::tag_value;
#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    cli_interactor::{
        Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms, PromptMultiChoiceParms,
    },
    client::Connect,
    git::{Repo, RepoActions},
    login,
    repo_ref::{self, RepoRef, REPO_REF_KIND},
    Cli,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[arg(default_value = "")]
    /// commits to send as proposal; like in `git format-patch` eg. HEAD~2
    pub(crate) since_or_range: String,
    #[clap(long)]
    /// nevent or event id of an existing proposal for which this is a new
    /// version
    pub(crate) in_reply_to: Option<String>,
    /// don't prompt for a cover letter
    #[arg(long, action)]
    pub(crate) no_cover_letter: bool,
    /// optional cover letter title
    #[clap(short, long)]
    pub(crate) title: Option<String>,
    #[clap(short, long)]
    /// optional cover letter description
    pub(crate) description: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;

    if let Some(id) = &args.in_reply_to {
        println!("creating proposal revision for: {id}");
    }

    let (main_branch_name, main_tip) = git_repo
        .get_main_or_master_branch()
        .context("the default branches (main or master) do not exist")?;

    let mut commits: Vec<Sha1Hash> = {
        if args.since_or_range.is_empty() {
            let branch_name = git_repo.get_checked_out_branch_name()?;
            let proposed_commits = if branch_name.eq(main_branch_name) {
                vec![main_tip]
            } else {
                let (_, _, ahead, _) = identify_ahead_behind(&git_repo, &None, &None)?;
                ahead
            };
            choose_commits(&git_repo, proposed_commits)?
        } else {
            git_repo
                .parse_starting_commits(&args.since_or_range)
                .context("cannot parse specified starting commit or range")?
        }
    };

    if commits.is_empty() {
        bail!("no commits selected");
    }
    println!("creating proposal from {} commits:", commits.len());

    let dim = Style::new().color256(247);
    for commit in &commits {
        println!(
            "{} {}",
            dim.apply_to(commit.to_string().chars().take(7).collect::<String>()),
            git_repo.get_commit_message_summary(commit)?
        );
    }

    let (first_commit_ahead, behind) =
        git_repo.get_commits_ahead_behind(&main_tip, commits.last().context("no commits")?)?;

    // check proposal ahead of origin/main
    if first_commit_ahead.len().gt(&1) && !Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt(
                    format!("proposal builds on a commit {} ahead of '{main_branch_name}' - do you want to continue?", first_commit_ahead.len() - 1)
                )
                .with_default(false)
        ).context("failed to get confirmation response from interactor confirm")? {
        bail!("aborting because selected commits were ahead of origin/master");
    }

    // check if a selected commit is already in origin
    if commits.iter().any(|c| c.eq(&main_tip)) {
        if !Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt(
                    format!("proposal contains commit(s) already in  '{main_branch_name}'. proceed anyway?")
                )
                .with_default(false)
        ).context("failed to get confirmation response from interactor confirm")? {
            bail!("aborting as proposal contains commit(s) already in '{main_branch_name}'");
        }
    }
    // check proposal isn't behind origin/main
    else if !behind.is_empty() && !Interactor::default().confirm(
            PromptConfirmParms::default()
                .with_prompt(
                    format!("proposal is {} behind '{main_branch_name}'. consider rebasing before submission. proceed anyway?", behind.len())
                )
                .with_default(false)
        ).context("failed to get confirmation response from interactor confirm")? {
        bail!("aborting so commits can be rebased");
    }

    let title = if args.no_cover_letter {
        None
    } else {
        match &args.title {
            Some(t) => Some(t.clone()),
            None => {
                if Interactor::default().confirm(
                    PromptConfirmParms::default()
                        .with_default(false)
                        .with_prompt("include cover letter?"),
                )? {
                    Some(
                        Interactor::default()
                            .input(PromptInputParms::default().with_prompt("title"))?
                            .clone(),
                    )
                } else {
                    None
                }
            }
        }
    };

    let cover_letter_title_description = if let Some(title) = title {
        Some((
            title,
            if let Some(t) = &args.description {
                t.clone()
            } else {
                Interactor::default()
                    .input(PromptInputParms::default().with_prompt("cover letter description"))?
                    .clone()
            },
        ))
    } else {
        None
    };

    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let (keys, user_ref) = login::launch(&cli_args.nsec, &cli_args.password, Some(&client)).await?;

    client.set_keys(&keys).await;

    let repo_ref = repo_ref::fetch(
        &git_repo,
        git_repo
            .get_root_commit()
            .context("failed to get root commit of the repository")?
            .to_string(),
        &client,
        user_ref.relays.write(),
        true,
    )
    .await?;

    // oldest first
    commits.reverse();

    let events = generate_cover_letter_and_patch_events(
        cover_letter_title_description.clone(),
        &git_repo,
        &commits,
        &keys,
        &repo_ref,
        &args.in_reply_to,
    )?;

    println!(
        "posting {} patch{} {} a covering letter...",
        if cover_letter_title_description.is_none() {
            events.len()
        } else {
            events.len() - 1
        },
        if cover_letter_title_description.is_none() && events.len().eq(&1)
            || cover_letter_title_description.is_some() && events.len().eq(&2)
        {
            ""
        } else {
            "es"
        },
        if cover_letter_title_description.is_none() {
            "without"
        } else {
            "with"
        }
    );

    send_events(
        &client,
        events,
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
    )
    .await?;
    // TODO check if there is already a similarly named
    Ok(())
}

#[allow(clippy::module_name_repetitions)]
pub async fn send_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    events: Vec<nostr::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<String>,
    animate: bool,
) -> Result<()> {
    let (_, _, _, mut all) = unique_and_duplicate_all(&my_write_relays, &repo_read_relays);

    let mut fallback = client.get_fallback_relays().clone();

    // blast repo events
    if events.iter().any(|e| e.kind().as_u64().eq(&REPO_REF_KIND)) {
        for r in client.get_blaster_relays() {
            fallback.push(r.to_string());
        }
    }

    // then remaining fallback list
    for r in &fallback {
        if !all.iter().any(|r2| r2.eq(&r)) {
            all.push(r);
        }
    }

    let m = MultiProgress::new();
    let pb_style = ProgressStyle::with_template(if animate {
        " {spinner} {prefix} {bar} {pos}/{len} {msg}"
    } else {
        " - {prefix} {bar} {pos}/{len} {msg}"
    })?
    .progress_chars("##-");

    let pb_after_style =
        |symbol| ProgressStyle::with_template(format!(" {symbol} {}", "{prefix} {msg}",).as_str());
    let pb_after_style_succeeded = pb_after_style(if animate {
        console::style("✔".to_string())
            .for_stderr()
            .green()
            .to_string()
    } else {
        "y".to_string()
    })?;

    let pb_after_style_failed = pb_after_style(if animate {
        console::style("✘".to_string())
            .for_stderr()
            .red()
            .to_string()
    } else {
        "x".to_string()
    })?;

    join_all(all.iter().map(|&relay| async {
        let details = format!(
            "{}{}{} {}",
            if my_write_relays.iter().any(|r| relay.eq(r)) {
                " [my-relay]"
            } else {
                ""
            },
            if repo_read_relays.iter().any(|r| relay.eq(r)) {
                " [repo-relay]"
            } else {
                ""
            },
            if fallback.iter().any(|r| relay.eq(r)) {
                " [default]"
            } else {
                ""
            },
            *relay,
        );
        let pb = m.add(
            ProgressBar::new(events.len() as u64)
                .with_prefix(details.to_string())
                .with_style(pb_style.clone()),
        );
        if animate {
            pb.enable_steady_tick(Duration::from_millis(300));
        }
        pb.inc(0); // need to make pb display intially
        let mut failed = false;
        for event in &events {
            match client.send_event_to(relay.as_str(), event.clone()).await {
                Ok(_) => pb.inc(1),
                Err(e) => {
                    pb.set_style(pb_after_style_failed.clone());
                    pb.finish_with_message(
                        console::style(
                            e.to_string()
                                .replace("relay pool error:", "error:")
                                .replace("event not published: ", ""),
                        )
                        .for_stderr()
                        .red()
                        .to_string(),
                    );
                    failed = true;
                    break;
                }
            };
        }
        if !failed {
            pb.set_style(pb_after_style_succeeded.clone());
            pb.finish_with_message("");
        }
    }))
    .await;
    Ok(())
}

/// returns `(unique_vec1, unique_vec2, duplicates, all)`
fn unique_and_duplicate_all<'a, S>(
    vec1: &'a Vec<S>,
    vec2: &'a Vec<S>,
) -> (Vec<&'a S>, Vec<&'a S>, Vec<&'a S>, Vec<&'a S>)
where
    S: PartialEq,
{
    let mut vec1_u = vec![];
    let mut vec2_u = vec![];
    let mut dup = vec![];
    let mut all = vec![];
    for s1 in vec1 {
        if vec2.iter().any(|s2| s1.eq(s2)) {
            dup.push(s1);
        } else {
            vec1_u.push(s1);
        }
    }
    for s2 in vec2 {
        if !vec1.iter().any(|s1| s2.eq(s1)) {
            vec2_u.push(s2);
        }
    }
    for a in [&dup, &vec1_u, &vec2_u] {
        for e in a {
            all.push(&**e);
        }
    }
    (vec1_u, vec2_u, dup, all)
}

fn choose_commits(git_repo: &Repo, proposed_commits: Vec<Sha1Hash>) -> Result<Vec<Sha1Hash>> {
    let mut proposed_commits = if proposed_commits.len().gt(&10) {
        vec![]
    } else {
        proposed_commits
    };

    let tip_of_head = git_repo.get_tip_of_branch(&git_repo.get_checked_out_branch_name()?)?;
    let most_recent_commit = proposed_commits.first().unwrap_or(&tip_of_head);

    let mut last_15_commits = vec![*most_recent_commit];

    while last_15_commits.len().lt(&15) {
        if let Ok(parent_commit) = git_repo.get_commit_parent(last_15_commits.last().unwrap()) {
            last_15_commits.push(parent_commit);
        } else {
            break;
        }
    }

    let term = console::Term::stderr();
    let mut printed_error_line = false;

    let selected_commits = 'outer: loop {
        let selected = Interactor::default().multi_choice(
            PromptMultiChoiceParms::default()
                .with_prompt("select commits for proposal")
                .dont_report()
                .with_choices(
                    last_15_commits
                        .iter()
                        .map(|h| summarise_commit_for_selection(git_repo, h).unwrap())
                        .collect(),
                )
                .with_defaults(
                    last_15_commits
                        .iter()
                        .map(|h| proposed_commits.iter().any(|c| c.eq(h)))
                        .collect(),
                ),
        )?;
        proposed_commits = selected.iter().map(|i| last_15_commits[*i]).collect();

        if printed_error_line {
            term.clear_last_lines(1)?;
        }

        if proposed_commits.is_empty() {
            term.write_line("no commits selected")?;
            printed_error_line = true;
            continue;
        }
        for (i, selected_i) in selected.iter().enumerate() {
            if i.gt(&0) && selected_i.ne(&(selected[i - 1] + 1)) {
                term.write_line("commits must be consecutive. try again.")?;
                printed_error_line = true;
                continue 'outer;
            }
        }

        break proposed_commits;
    };
    Ok(selected_commits)
}

fn summarise_commit_for_selection(git_repo: &Repo, commit: &Sha1Hash) -> Result<String> {
    let references = git_repo.get_refs(commit)?;
    let dim = Style::new().color256(247);
    let prefix = format!("({})", git_repo.get_commit_author(commit)?[0],);
    let references_string = if references.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            references
                .iter()
                .map(|r| format!("[{r}]"))
                .collect::<Vec<String>>()
                .join(" ")
        )
    };

    Ok(format!(
        "{} {}{} {}",
        dim.apply_to(prefix),
        git_repo.get_commit_message_summary(commit)?,
        Style::new().magenta().apply_to(references_string),
        dim.apply_to(commit.to_string().chars().take(7).collect::<String>(),),
    ))
}

mod tests_unique_and_duplicate {

    #[test]
    fn correct_number_of_unique_and_duplicate_items() {
        let v1 = vec![
            "t1".to_string(),
            "t2".to_string(),
            "t3".to_string(),
            "t4".to_string(),
            "t5".to_string(),
        ];
        let v2 = vec![
            "t3".to_string(),
            "t4".to_string(),
            "t5".to_string(),
            "t6".to_string(),
        ];

        let (v1_u, v2_u, d, a) = super::unique_and_duplicate_all(&v1, &v2);

        assert_eq!(v1_u.len(), 2);
        assert_eq!(v2_u.len(), 1);
        assert_eq!(d.len(), 3);
        assert_eq!(a.len(), 6);
    }
    #[test]
    fn all_begins_with_duplicates() {
        let v1 = vec![
            "t1".to_string(),
            "t2".to_string(),
            "t3".to_string(),
            "t4".to_string(),
            "t5".to_string(),
        ];
        let v2 = vec![
            "t3".to_string(),
            "t4".to_string(),
            "t5".to_string(),
            "t6".to_string(),
        ];

        let (_, _, d, a) = super::unique_and_duplicate_all(&v1, &v2);

        assert_eq!(a[0], d[0]);
    }
}

pub static PATCH_KIND: u64 = 1617;

pub fn generate_cover_letter_and_patch_events(
    cover_letter_title_description: Option<(String, String)>,
    git_repo: &Repo,
    commits: &[Sha1Hash],
    keys: &nostr::Keys,
    repo_ref: &RepoRef,
    in_reply_to: &Option<String>,
) -> Result<Vec<nostr::Event>> {
    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    let mut events = vec![];

    if let Some((title, description)) = cover_letter_title_description {
        events.push(EventBuilder::new(
        nostr::event::Kind::Custom(PATCH_KIND),
        format!(
            "From {} Mon Sep 17 00:00:00 2001\nSubject: [PATCH 0/{}] {title}\n\n{description}",
            commits.last().unwrap(),
            commits.len()
        ),
        [
            vec![
                // TODO: why not tag all maintainer identifiers?
                Tag::A {
                    coordinate: Coordinate {
                        kind: nostr::Kind::Custom(REPO_REF_KIND),
                        public_key: *repo_ref.maintainers.first()
                            .context("repo reference should always have at least one maintainer")?,
                        identifier: repo_ref.identifier.to_string(),
                        relays: repo_ref.relays.clone(),
                    },
                    relay_url: repo_ref.relays.first().map(nostr::UncheckedUrl::from).clone(),
                },
                Tag::Reference(format!("{root_commit}")),
                Tag::Hashtag("cover-letter".to_string()),
            ],
            if let Some(event_ref) = in_reply_to.clone() {
                vec![
                    Tag::Hashtag("root".to_string()),
                    Tag::Hashtag("revision-root".to_string()),
                    // TODO check if id is for a root proposal (perhaps its for an issue?)
                    e_tag_from_nip19(&event_ref,"proposal",nostr::Marker::Reply)?,
                ]
            } else {
                vec![
                    Tag::Hashtag("root".to_string()),
                ]
            },
            // this is not strictly needed but makes for prettier branch names
            // eventually a prefix will be needed of the event id to stop 2 proposals with the same name colliding
            // a change like this, or the removal of this tag will require the actual branch name to be tracked
            // so pulling and pushing still work
            if let Ok(branch_name) = git_repo.get_checked_out_branch_name() {
                if !branch_name.eq("main")
                    && !branch_name.eq("master")
                    && !branch_name.eq("origin/main")
                    && !branch_name.eq("origin/master")
                {
                    vec![Tag::Generic(
                        TagKind::Custom("branch-name".to_string()),
                        vec![branch_name],
                    )]
                }
                else { vec![] }
            } else {
                vec![]
            },
            repo_ref.maintainers
                .iter()
                .map(|pk| Tag::public_key(*pk))
                .collect(),
        ].concat(),
    )
    .to_event(keys)
    .context("failed to create cover-letter event")?);
    }

    for (i, commit) in commits.iter().enumerate() {
        events.push(
            generate_patch_event(
                git_repo,
                &root_commit,
                commit,
                events.first().map(|event| event.id),
                keys,
                repo_ref,
                events.last().map(nostr::Event::id),
                if events.is_empty() {
                    None
                } else {
                    Some(((i + 1).try_into()?, commits.len().try_into()?))
                },
                if events.is_empty() {
                    if let Ok(branch_name) = git_repo.get_checked_out_branch_name() {
                        if !branch_name.eq("main")
                            && !branch_name.eq("master")
                            && !branch_name.eq("origin/main")
                            && !branch_name.eq("origin/master")
                        {
                            Some(branch_name)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                },
                in_reply_to,
            )
            .context("failed to generate patch event")?,
        );
    }
    Ok(events)
}

fn e_tag_from_nip19(
    reference: &str,
    reference_name: &str,
    marker: nostr::Marker,
) -> Result<nostr::Tag> {
    let mut bech32 = reference.to_string();
    loop {
        if bech32.is_empty() {
            bech32 = Interactor::default().input(
                PromptInputParms::default().with_prompt(&format!("{reference_name} nevent")),
            )?;
        }

        if let Ok(nip19) = Nip19::from_bech32(bech32.clone()) {
            match nip19 {
                Nip19::Event(n) => {
                    break Ok(nostr::Tag::Event {
                        event_id: n.event_id,
                        relay_url: n.relays.first().map(UncheckedUrl::new),
                        marker: Some(marker),
                    });
                }
                Nip19::EventId(id) => {
                    break Ok(nostr::Tag::Event {
                        event_id: id,
                        relay_url: None,
                        marker: Some(marker),
                    });
                }
                _ => {}
            }
        }
        if let Ok(id) = nostr::EventId::from_str(&bech32) {
            break Ok(nostr::Tag::Event {
                event_id: id,
                relay_url: None,
                marker: Some(marker),
            });
        }
        println!("not a valid {reference_name} event reference");

        bech32 = String::new();
    }
}

pub struct CoverLetter {
    pub title: String,
    pub description: String,
    pub branch_name: String,
}

pub fn event_is_cover_letter(event: &nostr::Event) -> bool {
    // TODO: look for Subject:[ PATCH 0/n ] but watch out for:
    //   [PATCH v1 0/n ] or
    //   [PATCH subsystem v2 0/n ]
    event.kind.as_u64().eq(&PATCH_KIND)
        && event.iter_tags().any(|t| t.as_vec()[1].eq("root"))
        && event.iter_tags().any(|t| t.as_vec()[1].eq("cover-letter"))
}

pub fn commit_msg_from_patch(patch: &nostr::Event) -> Result<String> {
    if let Ok(msg) = tag_value(patch, "description") {
        Ok(msg)
    } else {
        let start_index = patch
            .content
            .find("] ")
            .context("event is not formatted as a patch or cover letter")?
            + 2;
        let end_index = patch.content[start_index..]
            .find("\ndiff --git")
            .unwrap_or(patch.content.len());
        Ok(patch.content[start_index..end_index].to_string())
    }
}

pub fn commit_msg_from_patch_oneliner(patch: &nostr::Event) -> Result<String> {
    Ok(commit_msg_from_patch(patch)?
        .split('\n')
        .collect::<Vec<&str>>()[0]
        .to_string())
}

pub fn event_to_cover_letter(event: &nostr::Event) -> Result<CoverLetter> {
    if !event_is_patch_set_root(event) {
        bail!("event is not a patch set root event (root patch or cover letter)")
    }

    let title = commit_msg_from_patch_oneliner(event)?;
    let full = commit_msg_from_patch(event)?;
    let description = full[title.len()..].trim().to_string();

    Ok(CoverLetter {
        title: title.clone(),
        description,
        // TODO should this be prefixed by format!("{}-"e.id.to_string()[..5]?)
        branch_name: if let Ok(name) = match tag_value(event, "branch-name") {
            Ok(name) => {
                if !name.eq("main") && !name.eq("master") {
                    Ok(name)
                } else {
                    Err(())
                }
            }
            _ => Err(()),
        } {
            name
        } else {
            let s = title
                .replace(' ', "-")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c.eq(&'/') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            s
        },
    })
}

pub fn event_is_patch_set_root(event: &nostr::Event) -> bool {
    event.kind.as_u64().eq(&PATCH_KIND) && event.iter_tags().any(|t| t.as_vec()[1].eq("root"))
}

pub fn event_is_revision_root(event: &nostr::Event) -> bool {
    event.kind.as_u64().eq(&PATCH_KIND)
        && event.iter_tags().any(|t| t.as_vec()[1].eq("revision-root"))
}

pub fn patch_supports_commit_ids(event: &nostr::Event) -> bool {
    event.kind.as_u64().eq(&PATCH_KIND)
        && event
            .iter_tags()
            .any(|t| t.as_vec()[0].eq("commit-pgp-sig"))
}

#[allow(clippy::too_many_arguments)]
pub fn generate_patch_event(
    git_repo: &Repo,
    root_commit: &Sha1Hash,
    commit: &Sha1Hash,
    thread_event_id: Option<nostr::EventId>,
    keys: &nostr::Keys,
    repo_ref: &RepoRef,
    parent_patch_event_id: Option<nostr::EventId>,
    series_count: Option<(u64, u64)>,
    branch_name: Option<String>,
    in_reply_to: &Option<String>,
) -> Result<nostr::Event> {
    let commit_parent = git_repo
        .get_commit_parent(commit)
        .context("failed to get parent commit")?;
    let relay_hint = repo_ref.relays.first().map(nostr::UncheckedUrl::from);
    EventBuilder::new(
        nostr::event::Kind::Custom(PATCH_KIND),
        git_repo
            .make_patch_from_commit(commit,&series_count)
            .context(format!("cannot make patch for commit {commit}"))?,
        [
            vec![
                Tag::A {
                    coordinate: Coordinate {
                        kind: nostr::Kind::Custom(REPO_REF_KIND),
                        public_key: *repo_ref.maintainers.first()
                            .context("repo reference should always have at least one maintainer - the issuer of the repo event")
                            ?,
                        identifier: repo_ref.identifier.to_string(),
                        relays: repo_ref.relays.clone(),
                    },
                    relay_url: relay_hint.clone(),
                },
                Tag::Reference(format!("{root_commit}")),
                // commit id reference is a trade-off. its now
                // unclear which one is the root commit id but it
                // enables easier location of code comments againt
                // code that makes it into the main branch, assuming
                // the commit id is correct
                Tag::Reference(commit.to_string()),
            ],

            if let Some(thread_event_id) = thread_event_id {
                vec![Tag::Event {
                    event_id: thread_event_id,
                    relay_url: relay_hint.clone(),
                    marker: Some(Marker::Root),
                }]
            } else if let Some(event_ref) = in_reply_to.clone() {
                vec![
                    Tag::Hashtag("root".to_string()),
                    Tag::Hashtag("revision-root".to_string()),
                    // TODO check if id is for a root proposal (perhaps its for an issue?)
                    e_tag_from_nip19(&event_ref,"proposal",nostr::Marker::Reply)?,
                ]
            } else {
                vec![
                    Tag::Hashtag("root".to_string()),
                ]
            },

            if let Some(id) = parent_patch_event_id {
                vec![Tag::Event {
                    event_id: id,
                    relay_url: relay_hint.clone(),
                    marker: Some(Marker::Reply),
                }]
            } else {
                vec![]
            },
            // see comment on branch names in cover letter event creation
            if let Some(branch_name) = branch_name {
                if thread_event_id.is_none() {
                    vec![
                        Tag::Generic(
                            TagKind::Custom("branch-name".to_string()),
                            vec![branch_name.to_string()],
                        )
                    ]
                }
                else { vec![]}
            }
            else { vec![]},
            // whilst it is in nip34 draft to tag the maintainers
            // I'm not sure it is a good idea because if they are
            // interested in all patches then their specialised
            // client should subscribe to patches tagged with the
            // repo reference. maintainers of large repos will not
            // be interested in every patch.
            repo_ref.maintainers
                    .iter()
                    .map(|pk| Tag::public_key(*pk))
                    .collect(),
            vec![
                // a fallback is now in place to extract this from the patch
                Tag::Generic(
                    TagKind::Custom("commit".to_string()),
                    vec![commit.to_string()],
                ),
                // this is required as patches cannot be relied upon to include the 'base commit'
                Tag::Generic(
                    TagKind::Custom("parent-commit".to_string()),
                    vec![commit_parent.to_string()],
                ),
                // this is required to ensure the commit id matches
                Tag::Generic(
                    TagKind::Custom("commit-pgp-sig".to_string()),
                    vec![
                        git_repo
                            .extract_commit_pgp_signature(commit)
                            .unwrap_or_default(),
                    ],
                ),
                // removing description tag will not cause anything to break
                Tag::Description(git_repo.get_commit_message(commit)?.to_string()),
                Tag::Generic(
                    TagKind::Custom("author".to_string()),
                    git_repo.get_commit_author(commit)?,
                ),
                // this is required to ensure the commit id matches
                Tag::Generic(
                    TagKind::Custom("committer".to_string()),
                    git_repo.get_commit_comitter(commit)?,
                ),
            ],
        ]
        .concat(),
    )
    .to_event(keys)
    .context("failed to sign event")
}
// TODO
// - find profile
// - file relays
// - find repo events
// -

/**
 * returns `(from_branch,to_branch,ahead,behind)`
 */
fn identify_ahead_behind(
    git_repo: &Repo,
    from_branch: &Option<String>,
    to_branch: &Option<String>,
) -> Result<(String, String, Vec<Sha1Hash>, Vec<Sha1Hash>)> {
    let (from_branch, from_tip) = match from_branch {
        Some(name) => (
            name.to_string(),
            git_repo
                .get_tip_of_branch(name)
                .context(format!("cannot find from_branch '{name}'"))?,
        ),
        None => (
            if let Ok(name) = git_repo.get_checked_out_branch_name() {
                name
            } else {
                "head".to_string()
            },
            git_repo
                .get_head_commit()
                .context("failed to get head commit")
                .context(
                    "checkout a commit or specify a from_branch. head does not reveal a commit",
                )?,
        ),
    };

    let (to_branch, to_tip) = match to_branch {
        Some(name) => (
            name.to_string(),
            git_repo
                .get_tip_of_branch(name)
                .context(format!("cannot find to_branch '{name}'"))?,
        ),
        None => {
            let (name, commit) = git_repo
                .get_main_or_master_branch()
                .context("the default branches (main or master) do not exist")?;
            (name.to_string(), commit)
        }
    };

    match git_repo.get_commits_ahead_behind(&to_tip, &from_tip) {
        Err(e) => {
            if e.to_string().contains("is not an ancestor of") {
                return Err(e).context(format!(
                    "'{from_branch}' is not branched from '{to_branch}'"
                ));
            }
            Err(e).context(format!(
                "failed to get commits ahead and behind from '{from_branch}' to '{to_branch}'"
            ))
        }
        Ok((ahead, behind)) => Ok((from_branch, to_branch, ahead, behind)),
    }
}

#[cfg(test)]
mod tests {
    use test_utils::git::GitTestRepo;

    use super::*;
    mod identify_ahead_behind {

        use super::*;
        use crate::git::oid_to_sha1;

        #[test]
        fn when_from_branch_doesnt_exist_return_error() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let git_repo = Repo::from_path(&test_repo.dir)?;

            test_repo.populate()?;
            let branch_name = "doesnt_exist";
            assert_eq!(
                identify_ahead_behind(&git_repo, &Some(branch_name.to_string()), &None)
                    .unwrap_err()
                    .to_string(),
                format!("cannot find from_branch '{}'", &branch_name),
            );
            Ok(())
        }

        #[test]
        fn when_to_branch_doesnt_exist_return_error() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let git_repo = Repo::from_path(&test_repo.dir)?;

            test_repo.populate()?;
            let branch_name = "doesnt_exist";
            assert_eq!(
                identify_ahead_behind(&git_repo, &None, &Some(branch_name.to_string()))
                    .unwrap_err()
                    .to_string(),
                format!("cannot find to_branch '{}'", &branch_name),
            );
            Ok(())
        }

        #[test]
        fn when_to_branch_is_none_and_no_main_or_master_branch_return_error() -> Result<()> {
            let test_repo = GitTestRepo::new("notmain")?;
            let git_repo = Repo::from_path(&test_repo.dir)?;

            test_repo.populate()?;

            assert_eq!(
                identify_ahead_behind(&git_repo, &None, &None)
                    .unwrap_err()
                    .to_string(),
                "the default branches (main or master) do not exist",
            );
            Ok(())
        }

        #[test]
        fn when_from_branch_is_not_head_return_as_from_branch() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let git_repo = Repo::from_path(&test_repo.dir)?;

            test_repo.populate()?;
            // create feature branch with 1 commit ahead
            test_repo.create_branch("feature")?;
            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let head_oid = test_repo.stage_and_commit("add t3.md")?;

            // make feature branch 1 commit behind
            test_repo.checkout("main")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            let main_oid = test_repo.stage_and_commit("add t4.md")?;

            let (from_branch, to_branch, ahead, behind) =
                identify_ahead_behind(&git_repo, &Some("feature".to_string()), &None)?;

            assert_eq!(from_branch, "feature");
            assert_eq!(ahead, vec![oid_to_sha1(&head_oid)]);
            assert_eq!(to_branch, "main");
            assert_eq!(behind, vec![oid_to_sha1(&main_oid)]);
            Ok(())
        }

        #[test]
        fn when_to_branch_is_not_main_return_as_to_branch() -> Result<()> {
            let test_repo = GitTestRepo::default();
            let git_repo = Repo::from_path(&test_repo.dir)?;

            test_repo.populate()?;
            // create dev branch with 1 commit ahead
            test_repo.create_branch("dev")?;
            test_repo.checkout("dev")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let dev_oid_first = test_repo.stage_and_commit("add t3.md")?;

            // create feature branch with 1 commit ahead of dev
            test_repo.create_branch("feature")?;
            test_repo.checkout("feature")?;
            std::fs::write(test_repo.dir.join("t4.md"), "some content")?;
            let feature_oid = test_repo.stage_and_commit("add t4.md")?;

            // make feature branch 1 behind
            test_repo.checkout("dev")?;
            std::fs::write(test_repo.dir.join("t3.md"), "some content")?;
            let dev_oid = test_repo.stage_and_commit("add t3.md")?;

            let (from_branch, to_branch, ahead, behind) = identify_ahead_behind(
                &git_repo,
                &Some("feature".to_string()),
                &Some("dev".to_string()),
            )?;

            assert_eq!(from_branch, "feature");
            assert_eq!(ahead, vec![oid_to_sha1(&feature_oid)]);
            assert_eq!(to_branch, "dev");
            assert_eq!(behind, vec![oid_to_sha1(&dev_oid)]);

            let (from_branch, to_branch, ahead, behind) =
                identify_ahead_behind(&git_repo, &Some("feature".to_string()), &None)?;

            assert_eq!(from_branch, "feature");
            assert_eq!(
                ahead,
                vec![oid_to_sha1(&feature_oid), oid_to_sha1(&dev_oid_first)]
            );
            assert_eq!(to_branch, "main");
            assert_eq!(behind, vec![]);

            Ok(())
        }
    }

    mod event_to_cover_letter {
        use super::*;

        fn generate_cover_letter(title: &str, description: &str) -> Result<nostr::Event> {
            Ok(nostr::event::EventBuilder::new(
                nostr::event::Kind::Custom(PATCH_KIND),
                format!("From ea897e987ea9a7a98e7a987e97987ea98e7a3334 Mon Sep 17 00:00:00 2001\nSubject: [PATCH 0/2] {title}\n\n{description}"),
                [
                    Tag::Hashtag("cover-letter".to_string()),
                    Tag::Hashtag("root".to_string()),
                ],
            )
            .to_event(&nostr::Keys::generate())?)
        }

        #[test]
        fn basic_title() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter("the title", "description here")?)?
                    .title,
                "the title",
            );
            Ok(())
        }

        #[test]
        fn basic_description() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter("the title", "description here")?)?
                    .description,
                "description here",
            );
            Ok(())
        }

        #[test]
        fn description_trimmed() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title",
                    " \n \ndescription here\n\n "
                )?)?
                .description,
                "description here",
            );
            Ok(())
        }

        #[test]
        fn multi_line_description() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title",
                    "description here\n\nmore here\nmore"
                )?)?
                .description,
                "description here\n\nmore here\nmore",
            );
            Ok(())
        }

        #[test]
        fn new_lines_in_title_forms_part_of_description() -> Result<()> {
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title\nwith new line",
                    "description here\n\nmore here\nmore"
                )?)?
                .title,
                "the title",
            );
            assert_eq!(
                event_to_cover_letter(&generate_cover_letter(
                    "the title\nwith new line",
                    "description here\n\nmore here\nmore"
                )?)?
                .description,
                "with new line\n\ndescription here\n\nmore here\nmore",
            );
            Ok(())
        }

        mod blank_description {
            use super::*;

            #[test]
            fn title_correct() -> Result<()> {
                assert_eq!(
                    event_to_cover_letter(&generate_cover_letter("the title", "")?)?.title,
                    "the title",
                );
                Ok(())
            }

            #[test]
            fn description_is_empty_string() -> Result<()> {
                assert_eq!(
                    event_to_cover_letter(&generate_cover_letter("the title", "")?)?.description,
                    "",
                );
                Ok(())
            }
        }
    }
}
