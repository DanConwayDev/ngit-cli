use std::sync::Arc;

use anyhow::{Context, Result};
use ngit::{
    accept_maintainership::{
        accept_maintainership_with_defaults, acceptance_lead, default_acceptance_maintainers,
        wait_for_grasp_servers,
    },
    cli_interactor::{cli_error, cli_error_with_category},
    client::{Params, get_repo_ref_from_cache, send_events},
    git::nostr_url::NostrUrlDecoded,
    login::user::{UserRef, publish_private_git_relay_list},
    repo_ref::{RepoRef, apply_grasp_infrastructure, latest_event_repo_ref},
    signer::NgitSigner,
};
use nostr::prelude::{RelayUrl, Timestamp, ToBech32, nip19::Nip19Coordinate};

use crate::{
    cli::SignerParams,
    client::{Client, Connect},
    git::{Repo, RepoActions},
    login,
    repo_ref::{print_selected_repo, try_resolve_repo_coordinate},
    sub_commands::{init, repository_fetch::prepare_account_for_repo_fetch},
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[clap(short, long, value_parser, num_args = 1..)]
    /// where your git+nostr data is hosted (optional; uses your saved grasp
    /// server list or the selected maintainer's servers if not specified)
    grasp_server: Vec<String>,
    #[arg(long)]
    /// reserved for future state-only replacement; collisions still fail
    force: bool,
}

async fn preflight_existing_announcement(
    git_repo_path: &std::path::Path,
    repo_ref: &RepoRef,
    client: &Client,
    selected: nostr::prelude::PublicKey,
    my_pubkey: nostr::prelude::PublicKey,
    force_requested: bool,
) -> Result<Option<RepoRef>> {
    let discovered =
        super::preflight::discover_candidate_events(client, repo_ref, my_pubkey).await?;
    let Some(existing) = super::preflight::latest_announcement(
        git_repo_path,
        &repo_ref.identifier,
        my_pubkey,
        &discovered,
    )
    .await
    else {
        return Ok(None);
    };
    let existing_event_id = existing.id.to_hex();
    let existing = RepoRef::try_from((existing, None))
        .context("failed to parse your existing same-identifier announcement")?;
    let selected_roster = repo_ref
        .events
        .values()
        .find(|event| event.pubkey == selected)
        .cloned()
        .map(|event| RepoRef::try_from((event, None)))
        .transpose()
        .context("failed to parse the selected maintainer's announcement")?
        .map_or_else(
            || repo_ref.maintainers.clone(),
            |selected| selected.maintainers,
        );
    super::preflight::require_no_joined_component(
        &existing,
        &selected_roster,
        selected,
        my_pubkey,
    )?;
    let repairs_maintainer_self_defer = existing
        .invalid_self_defers()
        .iter()
        .any(|invalid| invalid.author == my_pubkey && invalid.role != "o");
    if repairs_maintainer_self_defer {
        if existing.root_commit != repo_ref.root_commit {
            return Err(cli_error_with_category(
                "membership_identity_conflict",
                "repairing this announcement would join a different repository identity",
                &[
                    (
                        "existing earliest unique commit",
                        existing.root_commit.as_str(),
                    ),
                    (
                        "invitation earliest unique commit",
                        repo_ref.root_commit.as_str(),
                    ),
                ],
                &[
                    "preserve the repositories under separate identifiers or reconcile their identity first",
                ],
            ));
        }
        super::preflight::require_equivalent_activating_state(
            git_repo_path,
            repo_ref,
            my_pubkey,
            true,
            false,
            force_requested,
            &discovered,
        )
        .await?;
        return Ok(Some(existing));
    }
    let force_guidance = if force_requested {
        "--force cannot replace an existing repository announcement"
    } else {
        "--force cannot override repository identity, relationships, or announcement history"
    };
    Err(cli_error_with_category(
        "maintainer_acceptance_existing_announcement",
        "accepting would replace your existing same-identifier repository announcement",
        &[("existing event", existing_event_id.as_str())],
        &[
            "preserve that repository under another identifier or reconcile it with this repository first",
            force_guidance,
        ],
    ))
}

async fn accept_by_repairing_self_defer(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    mut existing: RepoRef,
    signer: &Arc<NgitSigner>,
    user_ref: &UserRef,
    client: &mut Client,
    grasp_servers: &[String],
) -> Result<()> {
    let my_pubkey = user_ref.public_key;
    let maintainers = default_acceptance_maintainers(repo_ref, my_pubkey);
    let lead = acceptance_lead(repo_ref, my_pubkey).filter(|lead| maintainers.contains(lead));
    let now = Timestamp::now().as_secs();
    existing.role_tags = existing
        .role_history_for_self_defer_acceptance(&my_pubkey, &maintainers, lead, now)
        .context("failed to repair the invalid self-defer while accepting maintainership")?;
    existing.maintainers = maintainers;
    existing.lead = lead;
    existing.private = repo_ref.private;
    existing.events = repo_ref.events.clone();

    if !grasp_servers.is_empty() {
        let mut git_servers = Vec::new();
        let mut relay_strings = Vec::new();
        apply_grasp_infrastructure(
            grasp_servers,
            &mut git_servers,
            &mut relay_strings,
            &my_pubkey,
            &repo_ref.identifier,
        )?;
        existing.git_server = git_servers;
        existing.relays = relay_strings
            .iter()
            .filter_map(|relay| RelayUrl::parse(relay).ok())
            .collect();
    }
    require_reachable_repair_hosting(&existing.git_server, &existing.relays, repo_ref.private)?;

    let event = existing.to_event(signer).await?;
    client.set_signer(signer.clone()).await;
    if repo_ref.private {
        publish_private_git_relay_list(client, &existing.relays, user_ref, signer)
            .await
            .context("failed to publish private Git relay discovery list")?;
    }
    let mut repository_relays = repo_ref.relays.clone();
    for relay in &existing.relays {
        if !repository_relays.contains(relay) {
            repository_relays.push(relay.clone());
        }
    }
    let _ = send_events(
        client,
        Some(git_repo.get_path()?),
        vec![event],
        user_ref.relays.write(),
        repository_relays,
        true,
        false,
    )
    .await
    .context("failed to publish the repaired maintainer acceptance")?;

    if !grasp_servers.is_empty() {
        wait_for_grasp_servers(
            git_repo,
            grasp_servers,
            &my_pubkey,
            &repo_ref.identifier,
            repo_ref.private.then(|| signer.clone()),
        )
        .await?;
    }
    Ok(())
}

/// Refuse to sign a repaired acceptance whose hosting cannot reach the
/// repository. The repair republishes the existing announcement's hosting
/// verbatim (unless `--grasp-server` replaced it upstream), so the
/// reachability invariant has to hold here just like in `ngit init` and
/// `ngit repo edit`. A private repository additionally needs a relay hint
/// for its discovery list, checked first for its more specific guidance.
fn require_reachable_repair_hosting(
    git_servers: &[String],
    relays: &[RelayUrl],
    private: bool,
) -> Result<()> {
    if private && relays.is_empty() {
        return Err(cli_error(
            "a private repository announcement requires a relay hint",
            &[],
            &["retry with `ngit repo accept --grasp-server <server>`"],
        ));
    }
    init::validate_announcement_hosting(git_servers, relays, init::LaunchMode::RepoAccept)
}

fn require_pending_invitation(
    repo_ref: &RepoRef,
    selected: nostr::prelude::PublicKey,
    my_pubkey: nostr::prelude::PublicKey,
) -> Result<()> {
    if selected == my_pubkey {
        return Err(cli_error(
            "you are already the selected maintainer of this repository",
            &[],
            &["use `ngit repo edit` to update your announcement"],
        ));
    }
    if repo_ref.confirmed_maintainers().contains(&my_pubkey) {
        return Err(cli_error(
            "you are already a confirmed maintainer of this repository",
            &[],
            &["use `ngit repo edit` to update your announcement"],
        ));
    }
    if repo_ref.invited_maintainers().contains(&my_pubkey) {
        return Ok(());
    }
    let selected_npub = selected.to_bech32().unwrap_or_else(|_| selected.to_hex());
    Err(cli_error_with_category(
        "maintainer_invitation_missing",
        "you have not been invited as a maintainer of this repository",
        &[("selected maintainer", selected_npub.as_str())],
        &["the selected maintainer must add your npub to their announcement first"],
    ))
}

pub async fn launch(args: &SubCommandArgs, signer: SignerParams<'_>) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;
    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (signer, user_ref, _) = login::login_or_signup(
        &Some(&git_repo),
        signer.info,
        signer.password,
        Some(&client),
        false,
    )
    .await?;

    let my_pubkey = user_ref.public_key;

    let Some(resolved_repo_coordinate) = try_resolve_repo_coordinate(&git_repo).await? else {
        return Err(cli_error(
            "no nostr repository found",
            &[],
            &["use `ngit repo init` to publish this repository to nostr"],
        ));
    };
    print_selected_repo(&resolved_repo_coordinate);
    let mut repo_coordinate = resolved_repo_coordinate.coordinate;

    // Fetch latest data from relays
    let private_discovery = prepare_account_for_repo_fetch(
        &git_repo,
        &mut client,
        &repo_coordinate,
        &signer,
        &user_ref,
    )
    .await;
    ngit::client::fetching_with_private_discovery(
        git_repo_path,
        &client,
        &mut repo_coordinate,
        &private_discovery,
    )
    .await?;

    let Some(repo_ref) =
        (get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await).ok()
    else {
        return Err(cli_error(
            "no announcement found on relays for this repository",
            &[],
            &[
                "if you created this repository, use `ngit repo init` to publish an announcement",
                "if this is a relay or network issue, try again later",
            ],
        ));
    };

    // Validate state
    let selected = repo_ref.selected_maintainer;

    require_pending_invitation(&repo_ref, selected, my_pubkey)?;

    let existing_self_defer = preflight_existing_announcement(
        git_repo_path,
        &repo_ref,
        &client,
        selected,
        my_pubkey,
        args.force,
    )
    .await?;

    // Happy path: CoMaintainer state without an existing announcement
    let repo_name = &repo_ref.name;
    println!("accepting maintainer invitation for '{repo_name}'");
    println!("publishing your repository announcement to nostr...");

    if let Some(existing) = existing_self_defer {
        accept_by_repairing_self_defer(
            &git_repo,
            &repo_ref,
            existing,
            &signer,
            &user_ref,
            &mut client,
            &args.grasp_server,
        )
        .await?;
    } else if args.grasp_server.is_empty() {
        // Use the existing defaults logic from the library
        accept_maintainership_with_defaults(&git_repo, &repo_ref, &user_ref, &mut client, &signer)
            .await?;
    } else {
        // User specified grasp servers explicitly — use them
        accept_with_grasp_servers(
            &git_repo,
            &repo_ref,
            &signer,
            &user_ref,
            &mut client,
            &args.grasp_server,
        )
        .await?;
    }

    if crate::output::is_json() {
        crate::output::set_value(serde_json::json!({
            "command_status": "ok",
            "action": "accepted",
            "entity": "repository",
            "name": repo_name,
            "coordinate": repo_coordinate.to_bech32()?,
        }));
    }
    print_completion_message(&git_repo, repo_coordinate);

    Ok(())
}

/// Report success and, when origin is not a `nostr://` remote, explain how
/// to get pushes flowing through nostr — suggesting the coordinate accept
/// ran against (the inviter's), never the accepter's own npub. See
/// `accept_with_grasp_servers` for why resolution must stay rooted on the
/// inviter's coordinate.
fn print_completion_message(git_repo: &Repo, repo_coordinate: Nip19Coordinate) {
    println!("co-maintainership accepted.");
    let origin_is_nostr = git_repo
        .git_repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(std::string::ToString::to_string).ok())
        .is_some_and(|url| url.starts_with("nostr://"));
    if origin_is_nostr {
        println!("your announcement has been published to nostr. you can now push updates.");
    } else {
        let inviter_url = NostrUrlDecoded {
            original_string: String::new(),
            coordinate: repo_coordinate,
            protocol: None,
            ssh_key_file: None,
            nip05: None,
        };
        println!("your announcement has been published to nostr.");
        println!(
            "pushes go through nostr only via a nostr remote; add one with `git remote add nostr {inviter_url}`"
        );
    }
    println!("run `ngit repo edit` at any time to update your announcement.");
}

/// Accept co-maintainership with explicitly specified grasp servers.
async fn accept_with_grasp_servers(
    git_repo: &Repo,
    repo_ref: &RepoRef,
    signer: &Arc<NgitSigner>,
    user_ref: &ngit::login::user::UserRef,
    client: &mut Client,
    grasp_servers: &[String],
) -> Result<()> {
    let my_pubkey = &user_ref.public_key;
    let identifier = &repo_ref.identifier;

    let mut git_servers: Vec<String> = vec![];
    let mut relay_strings: Vec<String> = vec![];

    apply_grasp_infrastructure(
        grasp_servers,
        &mut git_servers,
        &mut relay_strings,
        my_pubkey,
        identifier,
    )?;

    let relays: Vec<RelayUrl> = relay_strings
        .iter()
        .filter_map(|r| RelayUrl::parse(r).ok())
        .collect();

    let latest = latest_event_repo_ref(repo_ref);
    let name = latest
        .as_ref()
        .map_or_else(|| identifier.clone(), |lr| lr.name.clone());
    let description = latest
        .as_ref()
        .map(|lr| lr.description.clone())
        .unwrap_or_default();
    let web = latest.as_ref().map(|lr| lr.web.clone()).unwrap_or_default();
    let upstream = latest
        .as_ref()
        .map(|lr| lr.upstream.clone())
        .unwrap_or_default();
    let hashtags = latest
        .as_ref()
        .map(|lr| lr.hashtags.clone())
        .unwrap_or_default();
    let blossoms = latest
        .as_ref()
        .map(|lr| lr.blossoms.clone())
        .unwrap_or_default();
    let root_commit = latest
        .as_ref()
        .map(|lr| lr.root_commit.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| repo_ref.root_commit.clone());

    let maintainers = default_acceptance_maintainers(repo_ref, *my_pubkey);
    // per NIP-34 the acceptance re-asserts the repository's wire lead as
    // `M`; the guard is defensive — a lead reported by lead_maintainer()
    // always ends up in the default listing
    let lead = acceptance_lead(repo_ref, *my_pubkey).filter(|lead| maintainers.contains(lead));

    let now = Timestamp::now().as_secs();
    let role_tags = repo_ref.role_history_for_acceptance(my_pubkey, &maintainers, lead, now);
    let my_repo_ref = RepoRef {
        identifier: identifier.clone(),
        name,
        description,
        root_commit,
        git_server: git_servers,
        web,
        upstream,
        relays: relays.clone(),
        blossoms,
        hashtags,
        private: repo_ref.private,
        selected_maintainer: *my_pubkey,
        maintainers_without_annoucnement: None,
        maintainers,
        events: std::collections::HashMap::new(),
        nostr_git_url: None,
        extra_tags: vec![],
        role_tags,
        moderators: vec![],
        lead,
    };

    let repo_event = my_repo_ref.to_event(signer).await?;

    client.set_signer(signer.clone()).await;

    if repo_ref.private {
        publish_private_git_relay_list(client, &relays, user_ref, signer)
            .await
            .context("failed to publish private Git relay discovery list")?;
    }

    let _ = send_events(
        client,
        Some(git_repo.get_path()?),
        vec![repo_event],
        user_ref.relays.write(),
        relays.clone(),
        true,
        false,
    )
    .await
    .context("failed to publish co-maintainer announcement")?;

    if !grasp_servers.is_empty() {
        wait_for_grasp_servers(
            git_repo,
            grasp_servers,
            my_pubkey,
            identifier,
            repo_ref.private.then(|| signer.clone()),
        )
        .await?;
    }

    // Deliberately leave `nostr.repo` and the origin remote untouched: the
    // coordinate the repo resolves from is the root of trust. Re-rooting
    // resolution on the accepter's own announcement — which always lists
    // them as a maintainer — would make it impossible to observe the
    // inviter removing them later. Keeping resolution on the inviter's
    // coordinate means removal surfaces naturally. `ngit repo follow-lead`
    // is the explicit way to move the checkout to the resolved lead.

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(url: &str) -> RelayUrl {
        RelayUrl::parse(url).unwrap()
    }

    #[test]
    fn a_repair_with_a_relay_and_a_git_server_is_accepted() {
        for private in [false, true] {
            assert!(
                require_reachable_repair_hosting(
                    &["https://git.example.com/x.git".to_string()],
                    &[relay("wss://relay.example.com")],
                    private,
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn a_public_repair_without_hosting_is_refused_before_signing() {
        let error = require_reachable_repair_hosting(&[], &[], false)
            .expect_err("hosting-less repair must be refused");
        assert!(
            error
                .to_string()
                .contains("at least one relay and one git server"),
            "unexpected refusal: {error}",
        );

        let error = require_reachable_repair_hosting(
            &["https://git.example.com/x.git".to_string()],
            &[],
            false,
        )
        .expect_err("relay-less public repair must be refused");
        assert!(
            error.to_string().contains("at least one relay"),
            "unexpected refusal: {error}",
        );

        let error =
            require_reachable_repair_hosting(&[], &[relay("wss://relay.example.com")], false)
                .expect_err("git-server-less repair must be refused");
        assert!(
            error.to_string().contains("at least one git server"),
            "unexpected refusal: {error}",
        );
    }

    #[test]
    fn a_private_repair_without_a_relay_hint_keeps_its_specific_guidance() {
        let error = require_reachable_repair_hosting(
            &["https://git.example.com/x.git".to_string()],
            &[],
            true,
        )
        .expect_err("relay-less private repair must be refused");
        assert!(
            error
                .to_string()
                .contains("a private repository announcement requires a relay hint"),
            "unexpected refusal: {error}",
        );
    }
}
