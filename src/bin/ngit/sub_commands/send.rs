use std::path::Path;

use anyhow::{bail, Context, Result};
use console::Style;
use ngit::{client::send_events, git_events::generate_cover_letter_and_patch_events};
use nostr::{
    nips::{nip10::Marker, nip19::Nip19Event},
    ToBech32,
};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;

use crate::{
    cli::Cli,
    cli_interactor::{
        Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms, PromptMultiChoiceParms,
    },
    client::{
        fetching_with_report, get_events_from_cache, get_repo_ref_from_cache, Client, Connect,
    },
    git::{identify_ahead_behind, Repo, RepoActions},
    git_events::{event_is_patch_set_root, event_tag_from_nip19_or_hex},
    login,
    repo_ref::get_repo_coordinates,
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
pub async fn launch(cli_args: &Cli, args: &SubCommandArgs, no_fetch: bool) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let (main_branch_name, main_tip) = git_repo
        .get_main_or_master_branch()
        .context("the default branches (main or master) do not exist")?;

    let mut client = Client::default();

    let repo_coordinates = get_repo_coordinates(&git_repo, &client).await?;

    if !no_fetch {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let (root_proposal_id, mention_tags) =
        get_root_proposal_id_and_mentions_from_in_reply_to(git_repo.get_path()?, &args.in_reply_to)
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
    let (signer, user_ref) = login::launch(
        &git_repo,
        &cli_args.bunker_uri,
        &cli_args.bunker_app_key,
        &cli_args.nsec,
        &cli_args.password,
        Some(&client),
        false,
        false,
    )
    .await?;

    client.set_signer(signer.clone()).await;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &repo_coordinates).await?;

    // oldest first
    commits.reverse();

    let events = generate_cover_letter_and_patch_events(
        cover_letter_title_description.clone(),
        &git_repo,
        &commits,
        &signer,
        &repo_ref,
        &root_proposal_id,
        &mention_tags,
    )
    .await?;

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
        git_repo_path,
        events.clone(),
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
        false,
    )
    .await?;

    if root_proposal_id.is_none() {
        if let Some(event) = events.first() {
            let event_bech32 = if let Some(relay) = repo_ref.relays.first() {
                Nip19Event::new(event.id(), vec![relay]).to_bech32()?
            } else {
                event.id().to_bech32()?
            };
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in gitworkshop.dev: https://gitworkshop.dev/repo/{}/proposal/{}",
                    repo_ref.coordinate_with_hint().to_bech32()?,
                    &event_bech32,
                ))
            );
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in another client:  https://njump.me/{}",
                    &event_bech32,
                ))
            );
        }
    }
    // TODO check if there is already a similarly named
    Ok(())
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
    git_repo_path: &Path,
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
                let events =
                    get_events_from_cache(git_repo_path, vec![nostr::Filter::new().id(*event_id)])
                        .await?;

                if let Some(first) = events.iter().find(|e| e.id.eq(event_id)) {
                    if event_is_patch_set_root(first) {
                        Some(event_id.to_string())
                    } else {
                        None
                    }
                } else {
                    None
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

// TODO
// - find profile
// - file relays
// - find repo events
// -
