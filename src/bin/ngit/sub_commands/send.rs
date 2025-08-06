use std::{path::Path, str::FromStr, thread, time::Duration};

use anyhow::{Context, Result, bail};
use console::Style;
use ngit::{
    cli_interactor::{PromptChoiceParms, multi_select_with_custom_value},
    client::{Params, send_events},
    git::nostr_url::CloneUrl,
    git_events::{
        EventRefType, KIND_PULL_REQUEST, KIND_PULL_REQUEST_UPDATE,
        generate_cover_letter_and_patch_events,
    },
    push::push_refs_and_generate_pr_or_pr_update_event,
    repo_ref::{
        format_grasp_server_url_as_clone_url, format_grasp_server_url_as_relay_url,
        is_grasp_server, normalize_grasp_server_url,
    },
    utils::proposal_tip_is_pr_or_pr_update,
};
use nostr::{
    ToBech32,
    event::{Event, Kind},
    nips::{
        nip01::Coordinate,
        nip19::{Nip19Coordinate, Nip19Event},
    },
};
use nostr_sdk::hashes::sha1::Hash as Sha1Hash;

use crate::{
    cli::{Cli, extract_signer_cli_arguments},
    cli_interactor::{
        Interactor, InteractorPrompt, PromptConfirmParms, PromptInputParms, PromptMultiChoiceParms,
    },
    client::{
        Client, Connect, fetching_with_report, get_events_from_local_cache, get_repo_ref_from_cache,
    },
    git::{Repo, RepoActions, identify_ahead_behind},
    git_events::{event_is_patch_set_root, event_tag_from_nip19_or_hex},
    login,
    repo_ref::get_repo_coordinates_when_remote_unknown,
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
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let (main_branch_name, main_tip) = git_repo
        .get_main_or_master_branch()
        .context("the default branches (main or master) do not exist")?;

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let repo_coordinates = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    if !no_fetch {
        fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;
    }

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinates).await?;

    let (root_proposal, mention_tags) =
        get_root_proposal_and_mentions_from_in_reply_to(git_repo.get_path()?, &args.in_reply_to)
            .await?;

    if let Some(root_ref) = args.in_reply_to.first() {
        if root_proposal.is_some() {
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
                .context("failed to parse specified starting commit or range")?
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

    check_commits_are_suitable_for_proposal(
        &first_commit_ahead,
        &commits,
        &behind,
        main_branch_name,
        &main_tip,
    )?;

    let as_pr = {
        if let Some(root_proposal) = &root_proposal {
            proposal_tip_is_pr_or_pr_update(git_repo_path, &repo_ref, &root_proposal.id).await?
        } else {
            false
        }
    } || git_repo.are_commits_too_big_for_patches(&commits);

    let title = if as_pr {
        match &args.title {
            Some(t) => Some(t.clone()),
            None => {
                if root_proposal.is_none() {
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
    } else if args.no_cover_letter {
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
                    .input(PromptInputParms::default().with_prompt("description"))?
                    .clone()
            },
        ))
    } else {
        None
    };

    let (signer, mut user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        &extract_signer_cli_arguments(cli_args).unwrap_or(None),
        &cli_args.password,
        Some(&client),
        true,
    )
    .await?;

    client.set_signer(signer.clone()).await;

    // oldest first
    commits.reverse();

    let events = if as_pr {
        let mut to_try = vec![];
        let mut tried = vec![];
        let repo_grasps = repo_ref.grasp_servers();
        // if the user already has a fork, or is a maintainer, use those git servers
        let mut user_repo_ref = get_repo_ref_from_cache(
            Some(git_repo_path),
            &Nip19Coordinate {
                coordinate: Coordinate {
                    kind: nostr::event::Kind::GitRepoAnnouncement,
                    public_key: user_ref.public_key,
                    identifier: repo_ref.identifier.clone(),
                },
                relays: vec![],
            },
        )
        .await
        .ok();
        if let Some(user_repo_ref) = &user_repo_ref {
            for url in &user_repo_ref.git_server {
                if CloneUrl::from_str(url).is_ok() {
                    to_try.push(url.clone());
                }
            }
        }
        if !to_try.is_empty() || !repo_grasps.is_empty() {
            println!(
                "pushing proposal refs to {}",
                if repo_ref.maintainers.contains(&user_ref.public_key) {
                    "repository git servers"
                } else if to_try.is_empty() {
                    "repository grasp servers"
                } else if repo_grasps.is_empty() {
                    "the git servers listed in your fork"
                } else {
                    "the git servers listed in your fork and repository grasp servers"
                }
            );
        } else {
            println!(
                "The repository doesn't list a grasp server which would otherwise be used to submit your proposal as nostr Pull Request."
            );
        }
        // also use repo grasp servers
        for url in &repo_ref.git_server {
            if is_grasp_server(url, &repo_grasps) && !to_try.contains(url) {
                to_try.push(url.clone());
            }
        }

        let mut git_ref = None;
        let events = loop {
            let (events, _server_responses) = push_refs_and_generate_pr_or_pr_update_event(
                &git_repo,
                &repo_ref,
                commits.last().context("no commits")?,
                &user_ref,
                root_proposal.as_ref(),
                &cover_letter_title_description,
                &to_try,
                git_ref.clone(),
                &signer,
                &console::Term::stdout(),
            )
            .await?;
            for url in to_try {
                tried.push(url);
            }
            to_try = vec![];
            if let Some(events) = events {
                break events;
            }
            // fallback to creating user personal-fork on their grasp servers
            let untried_user_grasp_servers: Vec<String> = user_ref
                .grasp_list
                .urls
                .iter()
                .map(std::string::ToString::to_string)
                .filter(|g| {
                    // is a grasp server not in list of tried
                    !is_grasp_server(g, &tried)
                })
                .collect();

            if untried_user_grasp_servers.is_empty()
                && Interactor::default().choice(
                    PromptChoiceParms::default()
                        .with_prompt("choose alternative git server")
                        .dont_report()
                        .with_choices(vec![
                            "choose grasp server(s)".to_string(),
                            "enter a git repo url with write permission".to_string(),
                        ])
                        .with_default(0),
                )? == 1
            {
                loop {
                    let clone_url = Interactor::default()
                        .input(
                            PromptInputParms::default()
                                .with_prompt("git repo url with write permission"),
                        )?
                        .clone();
                    if CloneUrl::from_str(&clone_url).is_ok() {
                        to_try.push(clone_url);
                        let mut git_ref_or_branch_name = Interactor::default()
                            .input(
                                PromptInputParms::default()
                                    .with_prompt("ref / branch name")
                                    .with_default(
                                        git_ref.unwrap_or("refs/nostr/<event-id>".to_string()),
                                    ),
                            )?
                            .clone();
                        if !git_ref_or_branch_name.starts_with("refs/") {
                            git_ref_or_branch_name = format!("refs/heads/{git_ref_or_branch_name}");
                        }
                        git_ref = Some(git_ref_or_branch_name);
                        break;
                    }
                    println!("invalid clone url");
                }
                continue;
            }

            let mut new_grasp_server_events: Vec<Event> = vec![];

            let grasp_servers = if untried_user_grasp_servers.is_empty() {
                let default_choices: Vec<String> = client
                    .get_grasp_default_set()
                    .iter()
                    .filter(|g| !is_grasp_server(g, &tried))
                    .cloned()
                    .collect();
                let selections = vec![true; default_choices.len()]; // all selected by default
                let grasp_servers = multi_select_with_custom_value(
                    "grasp server(s)",
                    "grasp server",
                    default_choices,
                    selections,
                    normalize_grasp_server_url,
                )?;
                if grasp_servers.is_empty() {
                    // ask again
                    continue;
                }
                let normalised_grasp_servers: Vec<String> = grasp_servers
                    .iter()
                    .filter_map(|g| normalize_grasp_server_url(g).ok())
                    .collect();
                // if any grasp servers not listed in user grasp list prompt to update
                let grasp_servers_not_in_user_prefs: Vec<String> = normalised_grasp_servers
                    .iter()
                    .filter(|g| {
                        !user_ref.grasp_list.urls.contains(
                            // unwrap is safe as we constructed g
                            &nostr::Url::parse(&format_grasp_server_url_as_relay_url(g).unwrap())
                                .unwrap(),
                        )
                    })
                    .cloned()
                    .collect();
                if !grasp_servers_not_in_user_prefs.is_empty()
                    && Interactor::default().confirm(
                        PromptConfirmParms::default()
                            .with_prompt(
                                "add these to your list of prefered grasp servers?".to_string(),
                            )
                            .with_default(true),
                    )?
                {
                    for g in &normalised_grasp_servers {
                        let as_url = nostr::Url::parse(&format_grasp_server_url_as_relay_url(g)?)?;
                        if !user_ref.grasp_list.urls.contains(&as_url) {
                            user_ref.grasp_list.urls.push(as_url);
                        }
                    }
                    new_grasp_server_events.push(user_ref.grasp_list.to_event(&signer).await?);
                }
                normalised_grasp_servers
            } else {
                println!(
                    "{} personal-fork so we can push commits to your prefered grasp servers",
                    if user_repo_ref.is_some() {
                        "Updating"
                    } else {
                        "Creating a"
                    },
                );
                untried_user_grasp_servers
            };

            let grasp_servers_as_personal_clone_url: Vec<String> = grasp_servers
                .iter()
                .filter_map(|g| {
                    format_grasp_server_url_as_clone_url(
                        g,
                        &user_ref.public_key,
                        &repo_ref.identifier,
                    )
                    .ok()
                })
                .collect();

            // create personal-fork / update existing user repo and add these grasp servers
            let updated_user_repo_ref = {
                if let Some(mut user_repo_ref) = user_repo_ref {
                    for g in &grasp_servers_as_personal_clone_url {
                        let _ = user_repo_ref.add_grasp_server(g);
                    }
                    user_repo_ref
                } else {
                    // clone repo_ref and reset as personal-fork
                    let mut user_repo_ref = repo_ref.clone();
                    user_repo_ref.trusted_maintainer = user_ref.public_key;
                    user_repo_ref.maintainers = vec![user_ref.public_key];
                    user_repo_ref.git_server = vec![];
                    user_repo_ref.relays = vec![];
                    if !user_repo_ref
                        .hashtags
                        .contains(&"personal-fork".to_string())
                    {
                        user_repo_ref.hashtags.push("personal-fork".to_string());
                    }
                    user_repo_ref
                }
            };
            // pubish event to my-relays and my-fork-relays
            new_grasp_server_events.push(updated_user_repo_ref.to_event(&signer).await?);
            send_events(
                &client,
                Some(git_repo_path),
                new_grasp_server_events,
                user_ref.relays.write(),
                updated_user_repo_ref.relays.clone(),
                !cli_args.disable_cli_spinners,
                false,
            )
            .await?;
            user_repo_ref = Some(updated_user_repo_ref);
            // wait a few seconds
            let countdown_start = 5;
            let term = console::Term::stdout();
            for i in (1..=countdown_start).rev() {
                term.write_line(
                    format!(
                        "waiting {i}s grasp servers to create your repo before we push your data"
                    )
                    .as_str(),
                )?;
                thread::sleep(Duration::new(1, 0)); // Sleep for 1 second
                term.clear_last_lines(1)?;
            }
            term.flush().unwrap(); // Ensure the output is flushed to the terminal

            // add grasp servers to to_try
            for url in grasp_servers_as_personal_clone_url {
                to_try.push(url);
            }
            // the loop with continue with the grasp servers
        };
        println!(
            "posting {}",
            if events.iter().any(|e| e.kind.eq(&Kind::GitStatusClosed)) {
                "proposal revision as new PR event, and a close status for the old patch"
            } else if events.iter().any(|e| e.kind.eq(&KIND_PULL_REQUEST_UPDATE)) {
                "proposal revision as PR update event"
            } else {
                "proposal as PR event"
            }
        );
        events
    } else {
        let events = generate_cover_letter_and_patch_events(
            cover_letter_title_description.clone(),
            &git_repo,
            &commits,
            &signer,
            &repo_ref,
            &root_proposal.as_ref().map(|e| e.id.to_string()),
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
        events
    };

    send_events(
        &client,
        Some(git_repo_path),
        events.clone(),
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
        false,
    )
    .await?;

    if root_proposal.is_none() {
        if let Some(event) = events.first() {
            let event_bech32 = if let Some(relay) = repo_ref.relays.first() {
                Nip19Event {
                    event_id: event.id,
                    relays: vec![relay.clone()],
                    author: None,
                    kind: None,
                }
                .to_bech32()?
            } else {
                event.id.to_bech32()?
            };
            println!(
                "{}",
                dim.apply_to(format!(
                    "view in gitworkshop.dev: https://gitworkshop.dev/{}",
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

fn check_commits_are_suitable_for_proposal(
    first_commit_ahead: &[Sha1Hash],
    commits: &[Sha1Hash],
    behind: &[Sha1Hash],
    main_branch_name: &str,
    main_tip: &Sha1Hash,
) -> Result<()> {
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
    if commits.iter().any(|c| c.eq(main_tip)) {
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

async fn get_root_proposal_and_mentions_from_in_reply_to(
    git_repo_path: &Path,
    in_reply_to: &[String],
) -> Result<(Option<Event>, Vec<nostr::Tag>)> {
    let root_proposal = if let Some(first) = in_reply_to.first() {
        match event_tag_from_nip19_or_hex(first, "in-reply-to", EventRefType::Root, true, false)?
            .as_standardized()
        {
            Some(nostr_sdk::TagStandard::Event {
                event_id,
                relay_url: _,
                marker: _,
                public_key: _,
                uppercase: false,
            }) => {
                let events = get_events_from_local_cache(
                    git_repo_path,
                    vec![nostr::Filter::new().id(*event_id)],
                )
                .await?;

                if let Some(first) = events.iter().find(|e| e.id.eq(event_id)) {
                    if event_is_patch_set_root(first) || first.kind.eq(&KIND_PULL_REQUEST) {
                        Some(first.clone())
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
        if i.ne(&0) || root_proposal.is_none() {
            mention_tags.push(
                event_tag_from_nip19_or_hex(
                    reply_to,
                    "in-reply-to",
                    EventRefType::Quote,
                    true,
                    false,
                )
                .context(format!(
                    "{reply_to} in 'in-reply-to' not a valid nostr reference"
                ))?,
            );
        }
    }

    Ok((root_proposal, mention_tags))
}

// TODO
// - find profile
// - file relays
// - find repo events
// -
