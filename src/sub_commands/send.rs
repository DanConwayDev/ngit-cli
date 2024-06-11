use std::{str::FromStr, time::Duration};

use anyhow::{bail, Context, Result};
use console::Style;
use futures::future::join_all;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use nostr::{
    nips::{nip01::Coordinate, nip10::Marker, nip19::Nip19},
    EventBuilder, FromBech32, Tag, TagKind, ToBech32, UncheckedUrl,
};
use nostr_sdk::{hashes::sha1::Hash as Sha1Hash, TagStandard};

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
    #[clap(long, value_parser, num_args = 0.., value_delimiter = ' ')]
    /// references to an existing proposal for which this is a new
    /// version and/or events / npubs to tag as mentions
    pub(crate) in_reply_to: Vec<String>,
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

    let (main_branch_name, main_tip) = git_repo
        .get_main_or_master_branch()
        .context("the default branches (main or master) do not exist")?;

    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let (root_proposal_id, mention_tags) = get_root_proposal_id_and_mentions_from_in_reply_to(
        &client,
        // TODO: user repo relays when when event cache is in place
        client.get_fallback_relays(),
        &args.in_reply_to,
    )
    .await?;

    if let Some(root_ref) = args.in_reply_to.first() {
        if root_proposal_id.is_some() {
            println!("creating proposal revision for: {root_ref}");
        }
    }

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
        &root_proposal_id,
        &mention_tags,
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
        events.clone(),
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
    )
    .await?;

    if root_proposal_id.is_none() {
        if let Some(event) = events.first() {
            // TODO: add gitworkshop.dev to njump and remove direct gitworkshop link
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in gitworkshop.dev: https://gitworkshop.dev/repo/{}/proposal/{}",
                    repo_ref.identifier,
                    event.id(),
                ))
            );
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in another client:  https://njump.me/{}",
                    event
                        .id()
                        .to_bech32()
                        .context("cannot produce nevent from event id")?
                ))
            );
        }
    }
    // TODO check if there is already a similarly named
    Ok(())
}

#[allow(clippy::module_name_repetitions)]
#[allow(clippy::too_many_lines)]
pub async fn send_events(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    events: Vec<nostr::Event>,
    my_write_relays: Vec<String>,
    repo_read_relays: Vec<String>,
    animate: bool,
) -> Result<()> {
    let fallback = [
        client.get_fallback_relays().clone(),
        if events.iter().any(|e| e.kind().as_u16().eq(&REPO_REF_KIND)) {
            client.get_blaster_relays().clone()
        } else {
            vec![]
        },
    ]
    .concat();
    let mut relays: Vec<&String> = vec![];

    let all = &[
        repo_read_relays.clone(),
        my_write_relays.clone(),
        fallback.clone(),
    ]
    .concat();
    // add duplicates first
    for r in &repo_read_relays {
        let r_clean = remove_trailing_slash(r);
        if !my_write_relays
            .iter()
            .filter(|x| r_clean.eq(&remove_trailing_slash(x)))
            .count()
            > 1
            && !relays.iter().any(|x| r_clean.eq(&remove_trailing_slash(x)))
        {
            relays.push(r);
        }
    }

    for r in all {
        let r_clean = remove_trailing_slash(r);
        if !relays.iter().any(|x| r_clean.eq(&remove_trailing_slash(x))) {
            relays.push(r);
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

    #[allow(clippy::borrow_deref_ref)]
    join_all(relays.iter().map(|&relay| async {
        let relay_clean = remove_trailing_slash(&*relay);
        let details = format!(
            "{}{}{} {}",
            if my_write_relays
                .iter()
                .any(|r| relay_clean.eq(&remove_trailing_slash(r)))
            {
                " [my-relay]"
            } else {
                ""
            },
            if repo_read_relays
                .iter()
                .any(|r| relay_clean.eq(&remove_trailing_slash(r)))
            {
                " [repo-relay]"
            } else {
                ""
            },
            if fallback
                .iter()
                .any(|r| relay_clean.eq(&remove_trailing_slash(r)))
            {
                " [default]"
            } else {
                ""
            },
            relay_clean,
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

fn remove_trailing_slash(s: &String) -> String {
    match s.as_str().strip_suffix('/') {
        Some(s) => s,
        None => s,
    }
    .to_string()
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

async fn get_root_proposal_id_and_mentions_from_in_reply_to(
    #[cfg(test)] client: &crate::client::MockConnect,
    #[cfg(not(test))] client: &Client,
    repo_relays: &[String],
    in_reply_to: &[String],
) -> Result<(Option<String>, Vec<nostr::Tag>)> {
    let root_proposal_id = if let Some(first) = in_reply_to.first() {
        match event_tag_from_nip19_or_hex(first, "in-reply-to", Marker::Root, true, false)?
            .as_standardized()
        {
            Some(nostr_sdk::TagStandard::Event {
                event_id,
                relay_url: _,
                marker: _,
                public_key: _,
            }) => {
                let events = client
                    .get_events(
                        repo_relays.to_vec(),
                        vec![nostr::Filter::new().id(*event_id)],
                    )
                    .await
                    .context("whilst getting events specified in --in-reply-to")?;
                if let Some(first) = events.iter().find(|e| e.id.eq(event_id)) {
                    if event_is_patch_set_root(first) {
                        Some(event_id.to_string())
                    } else {
                        None
                    }
                } else {
                    bail!(
                        "cannot find first event specified in --in-reply-to \"{}\"",
                        first,
                    )
                }
            }
            _ => None,
        }
    } else {
        return Ok((None, vec![]));
    };

    let mut mention_tags = vec![];
    for (i, reply_to) in in_reply_to.iter().enumerate() {
        if i.ne(&0) || root_proposal_id.is_none() {
            mention_tags.push(
                event_tag_from_nip19_or_hex(reply_to, "in-reply-to", Marker::Mention, true, false)
                    .context(format!(
                        "{reply_to} in 'in-reply-to' not a valid nostr reference"
                    ))?,
            );
        }
    }

    Ok((root_proposal_id, mention_tags))
}

pub static PATCH_KIND: u16 = 1617;

#[allow(clippy::too_many_lines)]
pub fn generate_cover_letter_and_patch_events(
    cover_letter_title_description: Option<(String, String)>,
    git_repo: &Repo,
    commits: &[Sha1Hash],
    keys: &nostr::Keys,
    repo_ref: &RepoRef,
    root_proposal_id: &Option<String>,
    mentions: &[nostr::Tag],
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
                Tag::coordinate(Coordinate {
                    kind: nostr::Kind::Custom(REPO_REF_KIND),
                    public_key: *repo_ref.maintainers.first()
                        .context("repo reference should always have at least one maintainer")?,
                    identifier: repo_ref.identifier.to_string(),
                    relays: repo_ref.relays.clone(),
                }),
                Tag::from_standardized(TagStandard::Reference(format!("{root_commit}"))),
                Tag::hashtag("cover-letter"),
                Tag::custom(
                    nostr::TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                    vec![format!("git patch cover letter: {}", title.clone())],
                ),
            ],
            if let Some(event_ref) = root_proposal_id.clone() {
                vec![
                    Tag::hashtag("root"),
                    Tag::hashtag("revision-root"),
                    // TODO check if id is for a root proposal (perhaps its for an issue?)
                    event_tag_from_nip19_or_hex(&event_ref,"proposal",Marker::Reply, false, false)?,
                ]
            } else {
                vec![
                    Tag::hashtag("root"),
                ]
            },
            mentions.to_vec(),
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
                    vec![
                        Tag::custom(
                            nostr::TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
                            vec![branch_name],
                        ),
                    ]
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
                root_proposal_id,
                if events.is_empty() { mentions } else { &[] },
            )
            .context("failed to generate patch event")?,
        );
    }
    Ok(events)
}

fn event_tag_from_nip19_or_hex(
    reference: &str,
    reference_name: &str,
    marker: Marker,
    allow_npub_reference: bool,
    prompt_for_correction: bool,
) -> Result<nostr::Tag> {
    let mut bech32 = reference.to_string();
    loop {
        if bech32.is_empty() {
            bech32 = Interactor::default().input(
                PromptInputParms::default().with_prompt(&format!("{reference_name} reference")),
            )?;
        }
        if let Ok(nip19) = Nip19::from_bech32(bech32.clone()) {
            match nip19 {
                Nip19::Event(n) => {
                    break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: n.event_id,
                        relay_url: n.relays.first().map(UncheckedUrl::new),
                        marker: Some(marker),
                        public_key: None,
                    }));
                }
                Nip19::EventId(id) => {
                    break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                        event_id: id,
                        relay_url: None,
                        marker: Some(marker),
                        public_key: None,
                    }));
                }
                Nip19::Coordinate(coordinate) => {
                    break Ok(Tag::coordinate(coordinate));
                }
                Nip19::Profile(profile) => {
                    if allow_npub_reference {
                        break Ok(Tag::public_key(profile.public_key));
                    }
                }
                Nip19::Pubkey(public_key) => {
                    if allow_npub_reference {
                        break Ok(Tag::public_key(public_key));
                    }
                }
                _ => {}
            }
        }
        if let Ok(id) = nostr::EventId::from_str(&bech32) {
            break Ok(Tag::from_standardized(nostr_sdk::TagStandard::Event {
                event_id: id,
                relay_url: None,
                marker: Some(marker),
                public_key: None,
            }));
        }
        if prompt_for_correction {
            println!("not a valid {reference_name} event reference");
        } else {
            bail!(format!("not a valid {reference_name} event reference"));
        }

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
    event.kind.as_u16().eq(&PATCH_KIND)
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
    event.kind.as_u16().eq(&PATCH_KIND) && event.iter_tags().any(|t| t.as_vec()[1].eq("root"))
}

pub fn event_is_revision_root(event: &nostr::Event) -> bool {
    event.kind.as_u16().eq(&PATCH_KIND)
        && event.iter_tags().any(|t| t.as_vec()[1].eq("revision-root"))
}

pub fn patch_supports_commit_ids(event: &nostr::Event) -> bool {
    event.kind.as_u16().eq(&PATCH_KIND)
        && event
            .iter_tags()
            .any(|t| t.as_vec()[0].eq("commit-pgp-sig"))
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
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
    root_proposal_id: &Option<String>,
    mentions: &[nostr::Tag],
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
                Tag::coordinate(Coordinate {
                    kind: nostr::Kind::Custom(REPO_REF_KIND),
                    public_key: *repo_ref.maintainers.first()
                        .context("repo reference should always have at least one maintainer - the issuer of the repo event")
                        ?,
                    identifier: repo_ref.identifier.to_string(),
                    relays: repo_ref.relays.clone(),
                }),
                Tag::from_standardized(TagStandard::Reference(root_commit.to_string())),
                // commit id reference is a trade-off. its now
                // unclear which one is the root commit id but it
                // enables easier location of code comments againt
                // code that makes it into the main branch, assuming
                // the commit id is correct
                Tag::from_standardized(TagStandard::Reference(commit.to_string())),
                Tag::custom(
                    TagKind::Custom(std::borrow::Cow::Borrowed("alt")),
                    vec![format!("git patch: {}", git_repo.get_commit_message_summary(commit).unwrap_or_default())],
                ),
            ],

            if let Some(thread_event_id) = thread_event_id {
                vec![Tag::from_standardized(nostr_sdk::TagStandard::Event {
                    event_id: thread_event_id,
                    relay_url: relay_hint.clone(),
                    marker: Some(Marker::Root),
                    public_key: None,
                })]
            } else if let Some(event_ref) = root_proposal_id.clone() {
                vec![
                    Tag::hashtag("root"),
                    Tag::hashtag("revision-root"),
                    // TODO check if id is for a root proposal (perhaps its for an issue?)
                    event_tag_from_nip19_or_hex(&event_ref,"proposal", Marker::Reply, false, false)?,
                ]
            } else {
                vec![
                    Tag::hashtag("root"),
                ]
            },
            mentions.to_vec(),
            if let Some(id) = parent_patch_event_id {
                vec![Tag::from_standardized(nostr_sdk::TagStandard::Event {
                    event_id: id,
                    relay_url: relay_hint.clone(),
                    marker: Some(Marker::Reply),
                    public_key: None,
                })]
            } else {
                vec![]
            },
            // see comment on branch names in cover letter event creation
            if let Some(branch_name) = branch_name {
                if thread_event_id.is_none() {
                    vec![
                        Tag::custom(
                            TagKind::Custom(std::borrow::Cow::Borrowed("branch-name")),
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
                Tag::custom(
                    TagKind::Custom(std::borrow::Cow::Borrowed("commit")),
                    vec![commit.to_string()],
                ),
                // this is required as patches cannot be relied upon to include the 'base commit'
                Tag::custom(
                    TagKind::Custom(std::borrow::Cow::Borrowed("parent-commit")),
                    vec![commit_parent.to_string()],
                ),
                // this is required to ensure the commit id matches
                Tag::custom(
                    TagKind::Custom(std::borrow::Cow::Borrowed("commit-pgp-sig")),
                    vec![
                        git_repo
                            .extract_commit_pgp_signature(commit)
                            .unwrap_or_default(),
                        ],
                ),
                // removing description tag will not cause anything to break
                Tag::from_standardized(nostr_sdk::TagStandard::Description(
                    git_repo.get_commit_message(commit)?.to_string()
                )),
                Tag::custom(
                    TagKind::Custom(std::borrow::Cow::Borrowed("author")),
                    git_repo.get_commit_author(commit)?,
                ),
                // this is required to ensure the commit id matches
                Tag::custom(
                    TagKind::Custom(std::borrow::Cow::Borrowed("committer")),
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
                    Tag::hashtag("cover-letter"),
                    Tag::hashtag("root"),
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
