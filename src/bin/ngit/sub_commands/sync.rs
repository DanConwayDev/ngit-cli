use std::{collections::HashMap, str::FromStr};

use anyhow::{Context, Result, bail};
use console::Term;
use git2::Oid;
use ngit::{
    client::{
        Client, Connect, Params, fetching_with_report, get_repo_ref_from_cache,
        get_state_from_cache, send_events,
    },
    fetch::fetch_from_git_server,
    git::{
        Repo, RepoActions, get_git_config_item,
        nostr_url::{CloneUrl, NostrUrlDecoded},
    },
    list::{get_ahead_behind, list_from_remotes},
    login::existing::load_existing_login,
    push::push_to_remote,
    repo_ref::{
        format_grasp_server_url_as_relay_url, get_repo_coordinates_when_remote_unknown,
        is_grasp_server_clone_url,
    },
    repo_state::RepoState,
    utils::{get_short_git_server_name, join_with_and},
};
use nostr_sdk::RelayUrl;

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    /// optionally just sync a specific reference. eg main or v1.5.2
    #[clap(short, long)]
    pub(crate) ref_name: Option<String>,
    /// force push updates and delete refs from non-grasp git servers
    #[arg(long, action)]
    force: bool,
    /// trust git server(s) that are fast-forward ahead of nostr state and
    /// update nostr state to match; only applies to clean fast-forwards —
    /// diverged refs must be resolved manually
    #[arg(short = 't', long, action)]
    trust_server: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("failed to find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    // Read the optional semicolon-separated list of trusted git-server domains
    // from git config (local or global).  When a git server's hostname matches
    // one of these entries, `ngit sync` will automatically trust it and update
    // nostr state without requiring `--trust-server`.
    //
    // Example: git config --global nostr.trust-server-domains
    // 'github.com;codeberg.org'
    let trusted_domains: Vec<String> =
        get_git_config_item(&Some(&git_repo), "nostr.trust-server-domains")
            .unwrap_or(None)
            .map(|v| {
                v.split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default();

    let full_ref_name = if let Some(ref_name) = &args.ref_name {
        if ref_name.starts_with("refs/") {
            if git_repo.git_repo.find_reference(ref_name).is_ok() {
                Some(ref_name.clone())
            } else {
                bail!("could not find reference {ref_name}");
            }
        } else if git_repo
            .git_repo
            .find_reference(&format!("refs/tags/{ref_name}"))
            .is_ok()
        {
            Some(format!("refs/tags/{ref_name}"))
        } else if git_repo
            .git_repo
            .find_reference(&format!("refs/heads/{ref_name}"))
            .is_ok()
        {
            Some(format!("refs/heads/{ref_name}"))
        } else {
            bail!("could not find reference {ref_name}");
        }
    } else {
        None
    };

    let mut client = Client::new(Params::with_git_config_relay_defaults(&Some(&git_repo)));

    let (nostr_remote_name, decoded_nostr_url) = git_repo
        .get_first_nostr_remote_when_in_ngit_binary()
        .await.context("failed to list git remotes")?
        .context("no `nostr://` remote detected. `ngit sync` must be run from a repo with a nostr remote")?;

    let repo_coordinate = get_repo_coordinates_when_remote_unknown(&git_repo, &client).await?;

    let fetch_report = fetching_with_report(git_repo_path, &client, &repo_coordinate).await?;

    let repo_ref = get_repo_ref_from_cache(Some(git_repo_path), &repo_coordinate).await?;

    let mut nostr_state = get_state_from_cache(Some(git_repo_path), &repo_ref).await?;

    // When --force is given, rebuild and republish the state event even if
    // nothing has changed.  This lets users repair repos whose state event is
    // missing ^{} peeled refs for annotated tags (or any other corruption)
    // without needing to push a new ref.  A fresh event is signed (new
    // created_at) and broadcast to all repo relays and the user's write relays.
    if args.force {
        let (signer, user_ref, _) = load_existing_login(
            &Some(&git_repo),
            &None,
            &None,
            &None,
            Some(&client),
            false, // not silent — we need the user to authenticate if required
            false, // prompt_for_password
            false, // fetch_profile_updates
        )
        .await
        .context("authentication required to republish state; run 'ngit account login' first")?;
        client.set_signer(signer.clone()).await;
        // Backfill any missing ^{} peeled refs before rebuilding — the existing
        // state event may predate the fix that started storing them.
        let mut state = nostr_state.state.clone();
        let tag_refs: Vec<(String, String)> = state
            .iter()
            .filter(|(k, _)| k.starts_with("refs/tags/") && !k.ends_with("^{}"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (ref_name, tag_oid) in tag_refs {
            let peeled_key = format!("{ref_name}^{{}}");
            if state.contains_key(&peeled_key) {
                continue;
            }
            if let Ok(oid) = git2::Oid::from_str(&tag_oid) {
                if git_repo
                    .git_repo
                    .find_object(oid, Some(git2::ObjectType::Tag))
                    .is_ok()
                {
                    if let Ok(commit_oid) = git_repo.get_commit_or_tip_of_reference(&ref_name) {
                        state.insert(peeled_key, commit_oid.to_string());
                    }
                }
            }
        }
        let new_state = RepoState::build(repo_ref.identifier.clone(), state, &signer).await?;
        send_events(
            &client,
            Some(git_repo_path),
            vec![new_state.event],
            user_ref.relays.write(),
            repo_ref.relays.clone(),
            true,
            false,
        )
        .await?;
        println!("state event republished");
    }

    // Publish the current state event to any grasp server relays that are
    // missing it or have a stale version.  Grasp servers reject git pushes
    // unless the state event is already present on their relay, so we must
    // do this before attempting any git push.
    //
    // We use the per-relay state events captured during the fetch rather than
    // the local database, because the database only stores the canonical latest
    // event and cannot tell us what each individual relay holds.
    let grasp_relays_needing_state: Vec<RelayUrl> = repo_ref
        .git_server
        .iter()
        .filter(|url| is_grasp_server_clone_url(url))
        .filter_map(|url| {
            format_grasp_server_url_as_relay_url(url)
                .ok()
                .and_then(|relay_str| RelayUrl::parse(&relay_str).ok())
        })
        .filter(|relay_url| {
            // Include this relay if it was absent from the fetch results, had
            // no state event, or had a state event older than the canonical one.
            match fetch_report.state_per_relay.get(relay_url) {
                // relay wasn't queried, or returned no state event
                None | Some(None) => true,
                Some(Some(relay_event)) => relay_event.id != nostr_state.event.id,
            }
        })
        .collect();

    // relay URL -> whether the state event was successfully published to it.
    // Only populated for grasp relays that needed the state event; grasp
    // relays that already had the current state event are considered succeeded.
    let mut grasp_relay_publish_results: HashMap<String, bool> = HashMap::new();

    if !grasp_relays_needing_state.is_empty() {
        // Attempt to load an existing login silently so the signer is
        // available for NIP-42 auth if a relay requests it.  We do not
        // prompt the user, do not fetch profile updates, and ignore any
        // failure — the events are already signed so publishing works
        // without a signer.
        if let Ok((signer, _, _)) = load_existing_login(
            &Some(&git_repo),
            &None,
            &None,
            &None,
            Some(&client),
            true,  // silent
            false, // prompt_for_password
            false, // fetch_profile_updates
        )
        .await
        {
            client.set_signer(signer.clone()).await;
        }
        // Send only to the specific grasp relays that are missing or have a
        // stale state event — no user write relays.
        if let Ok(results) = send_events(
            &client,
            Some(git_repo_path),
            vec![nostr_state.event.clone()],
            vec![], // no user write relays
            grasp_relays_needing_state,
            true,
            false,
        )
        .await
        {
            for (relay_url, succeeded) in results {
                grasp_relay_publish_results.insert(relay_url, succeeded);
            }
        }
    }

    let term = console::Term::stderr();

    let remote_states = list_from_remotes(
        &term,
        &git_repo,
        &repo_ref.git_server,
        &decoded_nostr_url,
        Some(&nostr_state),
    )
    .await;

    let missing_refs =
        fetch_missing_refs(&git_repo, &nostr_state, &remote_states, &decoded_nostr_url);

    let (ahead_refs, diverging_refs) = find_ahead_and_diverging_refs(
        &git_repo,
        &nostr_state,
        &remote_states,
        &decoded_nostr_url,
        &term,
    );

    if !ahead_refs.is_empty() {
        term.write_line("git server(s) ahead of nostr state:")?;
        for r in &ahead_refs {
            let short_ref = r
                .ref_name
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.ref_name);
            let short_server = get_short_git_server_name(&r.source_url);
            term.write_line(&format!(
                "  {short_server} is {} commit{} ahead on {short_ref}",
                r.commits_ahead,
                if r.commits_ahead == 1 { "" } else { "s" },
            ))?;
        }

        // Partition ahead refs into those whose server domain is in the
        // trusted list (auto-trusted) and those that require --trust-server.
        let (trusted_ahead, untrusted_ahead): (Vec<&AheadRef>, Vec<&AheadRef>) = ahead_refs
            .iter()
            .partition(|r| is_url_domain_trusted(&r.source_url, &trusted_domains));

        let refs_to_trust: Vec<&AheadRef> = if args.trust_server {
            // --trust-server covers everything
            ahead_refs.iter().collect()
        } else {
            trusted_ahead
        };

        if !refs_to_trust.is_empty() {
            match load_existing_login(
                &Some(&git_repo),
                &None,
                &None,
                &None,
                Some(&client),
                false,
                false,
                false,
            )
            .await
            {
                Ok((signer, user_ref, _)) => {
                    client.set_signer(signer.clone()).await;

                    // Build the updated state map.
                    let mut new_state_map = nostr_state.state.clone();
                    for r in &refs_to_trust {
                        new_state_map.insert(r.ref_name.clone(), r.ahead_oid.clone());
                    }

                    match RepoState::build(repo_ref.identifier.clone(), new_state_map, &signer)
                        .await
                    {
                        Ok(new_state) => {
                            let publish_result = send_events(
                                &client,
                                Some(git_repo_path),
                                vec![new_state.event.clone()],
                                user_ref.relays.write(),
                                repo_ref.relays.clone(),
                                true,
                                false,
                            )
                            .await;

                            if publish_result.is_ok() {
                                term.write_line("nostr state updated")?;
                            } else {
                                term.write_line("WARNING: failed to publish updated nostr state")?;
                            }

                            // Update the nostr-remote tracking refs so the
                            // push loop below can push the new commits to all
                            // other servers using normal fast-forward refspecs.
                            for r in &refs_to_trust {
                                let tracking_name = r
                                    .ref_name
                                    .strip_prefix("refs/heads/")
                                    .or_else(|| r.ref_name.strip_prefix("refs/tags/"))
                                    .unwrap_or(&r.ref_name);
                                let tracking_refname =
                                    format!("refs/remotes/{nostr_remote_name}/{tracking_name}");
                                if let Ok(oid) = git2::Oid::from_str(&r.ahead_oid) {
                                    let _ = git_repo.git_repo.reference(
                                        &tracking_refname,
                                        oid,
                                        true,
                                        "ngit sync: update tracking ref from ahead server",
                                    );
                                }
                            }

                            // After updating the state the grasp-relay
                            // publish results are stale.  Clearing the map
                            // means the push loop will attempt all grasp
                            // servers without the safety-skip logic; if a
                            // server's relay didn't receive the new state the
                            // push will simply be rejected and reported.
                            grasp_relay_publish_results.clear();

                            nostr_state = new_state;
                        }
                        Err(e) => {
                            term.write_line(&format!(
                                "WARNING: failed to build updated nostr state: {e}"
                            ))?;
                        }
                    }
                }
                Err(_) => {
                    term.write_line(
                        "cannot update nostr state: not logged in — run 'ngit account login' first",
                    )?;
                }
            }
        }

        // Report any remaining untrusted-ahead refs that were not covered by
        // --trust-server.
        let remaining_untrusted: Vec<&AheadRef> = if args.trust_server {
            vec![] // --trust-server already handled everything
        } else {
            untrusted_ahead
        };
        if !remaining_untrusted.is_empty() {
            term.write_line(
                "run `ngit sync --trust-server` to update nostr state from ahead git server(s)",
            )?;
            if trusted_domains.is_empty() {
                term.write_line(
                    "  tip: set `nostr.trust-server-domains` in git config to auto-trust servers by domain",
                )?;
            }
        }
    }

    for d in &diverging_refs {
        let short_ref = d
            .ref_name
            .strip_prefix("refs/heads/")
            .unwrap_or(&d.ref_name);
        let short_server = get_short_git_server_name(&d.source_url);
        term.write_line(&format!(
            "{short_server} has diverged on {short_ref} ({} ahead, {} behind nostr state) \
             — --trust-server cannot fix this",
            d.commits_ahead, d.commits_behind,
        ))?;
        term.write_line(&format!(
            "  to adopt server state: git fetch {} {} && git push {} +{}",
            d.source_url, short_ref, nostr_remote_name, short_ref,
        ))?;
    }

    // Build a set of (url, ref_name) pairs for servers that are ahead of nostr
    // state.  These refs must not be pushed: the server already has commits
    // that nostr state doesn't, so pushing the (older) nostr-state tracking
    // ref would attempt a non-fast-forward downgrade and fail.
    let ahead_ref_skip: std::collections::HashSet<(&str, &str)> = ahead_refs
        .iter()
        .map(|r| (r.source_url.as_str(), r.ref_name.as_str()))
        .collect();

    for (url, (remote_state, is_grasp_server)) in &remote_states {
        let remote_name = get_short_git_server_name(url);
        let mut refspecs = vec![];
        // delete ref from remote
        let mut not_deleted = vec![];
        for remote_ref_name in remote_state.keys() {
            // skip peeled-tag dereference markers — not real refs
            if remote_ref_name.ends_with("^{}") {
                continue;
            }
            // skip unspecified refs
            if let Some(full_ref_name) = &full_ref_name {
                if remote_ref_name != full_ref_name {
                    continue;
                }
            }
            if (!remote_ref_name.starts_with("refs/heads/pr/")
                && (remote_ref_name.starts_with("refs/heads/")
                    || remote_ref_name.starts_with("refs/tags/")))
                && !nostr_state
                    .state
                    .keys()
                    .any(|nostr_ref| nostr_ref.eq(remote_ref_name))
            {
                if *is_grasp_server || args.force {
                    // delete branches / tags not on nostr
                    refspecs.push(format!(":{remote_ref_name}"));
                } else {
                    not_deleted.push(remote_ref_name);
                }
            }
        }
        // add or update ref on remote
        let mut not_updated = vec![];
        for nostr_ref_name in nostr_state.state.keys() {
            // skip peeled-tag dereference markers (e.g. refs/tags/v1.0.0^{})
            // — these are not real git refs and cannot appear in refspecs
            if nostr_ref_name.ends_with("^{}") {
                continue;
            }
            // skip unspecified refs
            if let Some(full_ref_name) = &full_ref_name {
                if nostr_ref_name != full_ref_name {
                    continue;
                }
            }
            // skip refs missing locally
            if missing_refs.contains(nostr_ref_name) {
                continue;
            }
            // skip refs where this server is ahead of nostr state — pushing
            // the (older) tracking ref would attempt a non-fast-forward
            // downgrade; the user must run with --trust-server first
            if ahead_ref_skip.contains(&(url.as_str(), nostr_ref_name.as_str())) {
                continue;
            }
            // strip refs/heads/ or refs/tags/ prefix to get the tracking ref segment
            // e.g. refs/heads/master -> master, refs/tags/v1.0.0 -> v1.0.0
            let tracking_ref_name = nostr_ref_name
                .strip_prefix("refs/heads/")
                .or_else(|| nostr_ref_name.strip_prefix("refs/tags/"))
                .unwrap_or(nostr_ref_name.as_str());
            if invalid_nostr_state_ref(nostr_ref_name) {
                // ensure nostr_state only supports refs/heads and refs/tags/
                // and not refs/heads/prs/*
            } else if let Some(remote_ref_value) = remote_state.get(nostr_ref_name) {
                // update ref
                let force_required = {
                    if let Ok((ahead, _)) =
                        get_ahead_behind(&git_repo, nostr_ref_name, remote_ref_value)
                    {
                        !ahead.is_empty()
                    } else {
                        true
                    }
                };
                if nostr_state
                    .state
                    .get(nostr_ref_name)
                    .is_none_or(|nostr_ref_value| nostr_ref_value.eq(remote_ref_value))
                {
                    // no action if ref in sync
                } else if remote_ref_value.starts_with("ref ") && !(args.force || *is_grasp_server)
                {
                    // dont try and sync push symbolic refs
                } else if !force_required {
                    refspecs.push(format!(
                        "refs/remotes/{nostr_remote_name}/{tracking_ref_name}:{nostr_ref_name}",
                    ));
                } else if *is_grasp_server || args.force {
                    refspecs.push(format!(
                        "+refs/remotes/{nostr_remote_name}/{tracking_ref_name}:{nostr_ref_name}",
                    ));
                } else {
                    not_updated.push(nostr_ref_name);
                }
            } else {
                // add missing refs
                refspecs.push(format!(
                    "refs/remotes/{nostr_remote_name}/{tracking_ref_name}:{nostr_ref_name}",
                ));
            }
        }

        // Skip grasp servers whose relay did not receive the state event —
        // they would reject the git push anyway.
        if (*is_grasp_server || is_grasp_server_clone_url(url))
            && !grasp_relay_publish_results.is_empty()
        {
            if let Ok(relay_url) = format_grasp_server_url_as_relay_url(url) {
                if grasp_relay_publish_results
                    .get(&relay_url)
                    .is_some_and(|succeeded| !succeeded)
                {
                    term.write_line(&format!(
                        "WARNING: skipping {remote_name} - state event failed to reach its relay"
                    ))?;
                    continue;
                }
            }
        }

        if refspecs.is_empty() {
            let has_ahead_refs = ahead_refs.iter().any(|r| r.source_url == *url);
            if !not_updated.is_empty() || !not_deleted.is_empty() {
                term.write_line(&format!("{remote_name} in sync excluding"))?;
            } else if !has_ahead_refs {
                term.write_line(&format!("{remote_name} already in sync"))?;
            }
            // if the server is ahead, we already reported it above — no
            // additional message needed here
            // report already in sync
        } else {
            match push_to_remote(
                &git_repo,
                url,
                &decoded_nostr_url,
                &refspecs,
                &term,
                *is_grasp_server || is_grasp_server_clone_url(url),
                &[],
            ) {
                Err(error) => {
                    term.write_line(&format!(
                        "error pushing updates to {remote_name}: error: {error}"
                    ))?;
                }
                Ok(updated_refs) => {
                    if updated_refs.values().all(std::option::Option::is_none) {
                        if *is_grasp_server || args.force {
                            term.write_line(&format!("{remote_name} sync completed"))?;
                            // TODO we only know if there was an error but not
                            // if it rejected any
                            // updates
                        } else {
                            // we should report on refs not force pushed
                            term.write_line(&format!("{remote_name} sync completed"))?;
                        }
                    } else {
                        term.write_line(&format!(
                            "{remote_name} sync completed but not all changes were accepted"
                        ))?;
                    }
                    for name in &not_deleted {
                        term.write_line(&format!("  - {name} not deleted"))?;
                    }
                    for name in &not_updated {
                        term.write_line(&format!("  - {name} not updated due to conflicts"))?;
                    }
                    if !not_updated.is_empty() || !not_deleted.is_empty() {
                        term.write_line("run `ngit sync --force` to delete refs or overwrite conflicts and potentially lose work")?;
                    }
                }
            }
        }
    }

    if !missing_refs.is_empty() {
        println!(
            "skipped the following refs as could not find them locally or on any git servers: {}",
            join_with_and(&missing_refs)
        );
    }
    Ok(())
}

/// A git server ref that is strictly ahead of the current nostr state (no
/// divergence).  The nostr-state commit is an ancestor of `ahead_oid`, so
/// bringing the state up to `ahead_oid` is a pure fast-forward.
struct AheadRef {
    /// Fully-qualified ref name, e.g. `refs/heads/main`
    ref_name: String,
    /// The commit OID the ahead server has for this ref
    ahead_oid: String,
    /// Which git-server URL reported this ahead state
    source_url: String,
    /// How many commits ahead of the nostr state
    commits_ahead: usize,
}

/// A git server ref that has diverged from the nostr state — both the server
/// and nostr state have commits the other does not.  Cannot be resolved with
/// `--trust-server`; the user must fetch the commits directly and force-push
/// to the nostr remote after reviewing them.
struct DivergingRef {
    /// Fully-qualified ref name, e.g. `refs/heads/main`
    ref_name: String,
    /// Which git-server URL reported this diverged state
    source_url: String,
    /// Commits the server has that nostr state does not
    commits_ahead: usize,
    /// Commits nostr state has that the server does not
    commits_behind: usize,
}

/// Scan `remote_states` for refs where a git server differs from the nostr
/// state.  Returns two vecs:
///
/// - **ahead**: refs where a server is strictly fast-forward ahead of nostr
///   state (nostr-state commit is an ancestor of the server's commit with no
///   divergence).  Safe to adopt with `--trust-server`.
/// - **diverging**: refs where the server and nostr state have each made
///   commits the other does not have.  Must be resolved manually.
///
/// When a server's OID is not available locally the function attempts to fetch
/// it from that server so that the ahead/behind comparison can be made.
///
/// If multiple servers are ahead on the same ref but their commits have
/// diverged from each other the ref is skipped to avoid guessing.
fn find_ahead_and_diverging_refs(
    git_repo: &Repo,
    nostr_state: &RepoState,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
    decoded_nostr_url: &NostrUrlDecoded,
    term: &console::Term,
) -> (Vec<AheadRef>, Vec<DivergingRef>) {
    // ref_name -> [(ahead_oid, source_url, commits_ahead)]
    let mut per_ref: HashMap<String, Vec<(String, String, usize)>> = HashMap::new();
    let mut diverging: Vec<DivergingRef> = vec![];

    for (url, (remote_state, _)) in remote_states {
        for (ref_name, remote_oid) in remote_state {
            // Only branches; exclude peeled-tag markers and PR branches
            if !ref_name.starts_with("refs/heads/")
                || ref_name.ends_with("^{}")
                || ref_name.starts_with("refs/heads/pr/")
            {
                continue;
            }
            // Skip symbolic refs
            if remote_oid.starts_with("ref ") {
                continue;
            }

            let nostr_oid = match nostr_state.state.get(ref_name) {
                Some(v) if !v.starts_with("ref ") => v.as_str(),
                _ => continue,
            };

            if nostr_oid == remote_oid {
                continue; // already in sync
            }

            // Try ahead/behind; if the remote OID is not local, fetch it first.
            let check = get_ahead_behind(git_repo, nostr_oid, remote_oid).or_else(|_| {
                let _ = fetch_from_git_server(
                    git_repo,
                    &[remote_oid.to_string()],
                    url,
                    decoded_nostr_url,
                    term,
                    is_grasp_server_clone_url(url),
                );
                get_ahead_behind(git_repo, nostr_oid, remote_oid)
            });

            if let Ok((ahead, behind)) = check {
                if !ahead.is_empty() && behind.is_empty() {
                    // server is strictly fast-forward ahead
                    per_ref.entry(ref_name.clone()).or_default().push((
                        remote_oid.clone(),
                        url.clone(),
                        ahead.len(),
                    ));
                } else if !ahead.is_empty() && !behind.is_empty() {
                    // true divergence — server and nostr state have each made
                    // commits the other does not have
                    diverging.push(DivergingRef {
                        ref_name: ref_name.clone(),
                        source_url: url.clone(),
                        commits_ahead: ahead.len(),
                        commits_behind: behind.len(),
                    });
                }
            }
        }
    }

    // For each ref pick the furthest-ahead consistent candidate.
    let mut ahead_result = vec![];
    'outer: for (ref_name, mut candidates) in per_ref {
        if candidates.is_empty() {
            continue;
        }

        // Sort descending by commits_ahead so candidates[0] is furthest ahead.
        candidates.sort_by(|a, b| b.2.cmp(&a.2));

        let (best_oid, best_url, best_count) = &candidates[0];

        // Every other candidate must be a (strict) ancestor of best_oid.
        for (other_oid, _, _) in candidates.iter().skip(1) {
            if other_oid == best_oid {
                continue;
            }
            // get_ahead_behind(base=other, latest=best) → behind should be empty
            // if other is an ancestor of best.
            match get_ahead_behind(git_repo, other_oid, best_oid) {
                Ok((_, behind)) if behind.is_empty() => {} // other ⊆ best, fine
                _ => {
                    // Servers disagree – skip this ref rather than guessing.
                    let _ = term.write_line(&format!(
                        "skipping {}: servers disagree on ahead commits",
                        ref_name.strip_prefix("refs/heads/").unwrap_or(&ref_name)
                    ));
                    continue 'outer;
                }
            }
        }

        ahead_result.push(AheadRef {
            ref_name,
            ahead_oid: best_oid.clone(),
            source_url: best_url.clone(),
            commits_ahead: *best_count,
        });
    }

    (ahead_result, diverging)
}

fn invalid_nostr_state_ref(ref_name: &str) -> bool {
    ref_name.ends_with("^{}")
        || ref_name.starts_with("refs/heads/pr/")
        || (!ref_name.starts_with("refs/heads/") && !ref_name.starts_with("refs/tags/"))
}

/// Returns `true` when the hostname of `url` matches any entry in
/// `trusted_domains` (case-insensitive).  An empty `trusted_domains` slice
/// always returns `false`.
fn is_url_domain_trusted(url: &str, trusted_domains: &[String]) -> bool {
    if trusted_domains.is_empty() {
        return false;
    }
    let host = CloneUrl::from_str(url)
        .map(|u| u.domain().to_lowercase())
        .unwrap_or_default();
    if host.is_empty() {
        return false;
    }
    trusted_domains.iter().any(|d| d == &host)
}

fn identify_missing_refs(git_repo: &Repo, state: &HashMap<String, String>) -> Vec<String> {
    let mut missing_oids = vec![];
    for tip in state.values() {
        if let Ok(exist) = git_repo.does_commit_exist(tip) {
            let oid_exists_as_tag = Oid::from_str(tip).is_ok_and(|tip| {
                git_repo
                    .git_repo
                    .find_object(tip, Some(git2::ObjectType::Tag))
                    .is_ok()
            });

            if !exist && !oid_exists_as_tag {
                missing_oids.push(tip.to_string());
            }
        }
    }
    missing_oids
}

/// returns refs that are still missing
fn fetch_missing_refs(
    git_repo: &Repo,
    nostr_state: &RepoState,
    remote_states: &HashMap<String, (HashMap<String, String>, bool)>,
    nostr_url_decoded: &NostrUrlDecoded,
) -> Vec<String> {
    let mut tried_remotes: Vec<String> = vec![];
    let required_oids = identify_missing_refs(git_repo, &nostr_state.state);
    if !required_oids.is_empty() {
        println!("fetching git data missing locally");
    }
    loop {
        let required_oids = identify_missing_refs(git_repo, &nostr_state.state);
        let mut oids_on_remote: HashMap<String, Vec<String>> = HashMap::new();
        if !required_oids.is_empty() {
            for (url, (state, _)) in remote_states {
                if tried_remotes.contains(url) {
                    continue;
                }
                for oid in &required_oids {
                    if state.values().any(|v| v.eq(oid)) {
                        oids_on_remote
                            .entry(url.to_string())
                            .or_default()
                            .push(oid.clone());
                    }
                }
            }
        }
        if let Some((url, oids)) = oids_on_remote.iter().max_by_key(|(url, vec)| {
            if tried_remotes.contains(url) {
                0
            } else {
                vec.len()
            }
        }) {
            if oids.is_empty() || tried_remotes.contains(url) {
                break;
            }
            tried_remotes.push(url.clone());
            let _ = fetch_from_git_server(
                git_repo,
                oids,
                url,
                nostr_url_decoded,
                &Term::stdout(),
                is_grasp_server_clone_url(url),
            );
        } else {
            break;
        }
    }

    let still_missing_oids = identify_missing_refs(git_repo, &nostr_state.state);
    if still_missing_oids.is_empty() {
        vec![]
    } else {
        let missing_refs: Vec<String> = nostr_state
            .state
            .iter()
            .filter_map(|(key, value)| {
                if still_missing_oids.contains(value) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();
        println!(
            "could not find refs on repo git servers: {}",
            join_with_and(&missing_refs)
        );
        missing_refs
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ngit::{client::STATE_KIND, git::Repo};
    use nostr::{
        Kind,
        nips::{nip01::Coordinate, nip19::Nip19Coordinate},
    };
    use test_helpers::GitTestRepo;

    use super::*;

    /// Minimal in-process git repository fixture for these unit tests
    /// (previously imported from the `test_utils` crate). The binary crate
    /// can't see the lib's `#[cfg(test)]` helpers, so we keep a tiny copy
    /// here. Only the surface area exercised by these tests is implemented.
    mod test_helpers {
        use std::{
            env::current_dir,
            fs,
            path::PathBuf,
            sync::atomic::{AtomicU64, Ordering},
        };

        use anyhow::{Context, Result};
        use git2::{Branch, Oid, RepositoryInitOptions, Signature, Time};
        use once_cell::sync::Lazy;

        fn joe_signature() -> Signature<'static> {
            Signature::new("Joe Bloggs", "joe.bloggs@pm.me", &Time::new(0, 0)).unwrap()
        }

        fn unique_suffix() -> String {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            static SEED: Lazy<u64> = Lazy::new(|| {
                let b = Box::new(0u8);
                let addr = (&*b as *const u8) as u64;
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                addr ^ nanos ^ (std::process::id() as u64)
            });
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            format!("{:016x}{:08x}", *SEED, n)
        }

        pub struct GitTestRepo {
            pub dir: PathBuf,
            pub git_repo: git2::Repository,
        }

        impl Default for GitTestRepo {
            fn default() -> Self {
                // sync.rs tests don't care about nostr.repo config, so a plain
                // git repo on `main` is sufficient.
                Self::new("main").unwrap()
            }
        }

        impl GitTestRepo {
            pub fn new(main_branch_name: &str) -> Result<Self> {
                let path = current_dir()?.join(format!("tmpgit-{}", unique_suffix()));
                let git_repo = git2::Repository::init_opts(
                    &path,
                    RepositoryInitOptions::new()
                        .initial_head(main_branch_name)
                        .mkpath(true),
                )?;
                git_repo.config()?.set_bool("diff.mnemonicPrefix", false)?;
                Ok(Self {
                    dir: path,
                    git_repo,
                })
            }

            fn initial_commit(&self) -> Result<Oid> {
                let oid = self.git_repo.index()?.write_tree()?;
                let tree = self.git_repo.find_tree(oid)?;
                let commit_oid = self.git_repo.commit(
                    Some("HEAD"),
                    &joe_signature(),
                    &joe_signature(),
                    "Initial commit",
                    &tree,
                    &[],
                )?;
                Ok(commit_oid)
            }

            pub fn populate(&self) -> Result<Oid> {
                self.initial_commit()?;
                fs::write(self.dir.join("t1.md"), "some content")?;
                self.stage_and_commit("add t1.md")?;
                fs::write(self.dir.join("t2.md"), "some content1")?;
                self.stage_and_commit("add t2.md")
            }

            pub fn stage_and_commit(&self, message: &str) -> Result<Oid> {
                let prev_oid = self.git_repo.head().unwrap().peel_to_commit()?;
                let mut index = self.git_repo.index()?;
                index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
                index.write()?;
                let oid = self.git_repo.commit(
                    Some("HEAD"),
                    &joe_signature(),
                    &joe_signature(),
                    message,
                    &self.git_repo.find_tree(index.write_tree()?)?,
                    &[&prev_oid],
                )?;
                Ok(oid)
            }

            pub fn create_branch(&'_ self, branch_name: &str) -> Result<Branch<'_>> {
                self.git_repo
                    .branch(branch_name, &self.git_repo.head()?.peel_to_commit()?, false)
                    .context("could not create branch")
            }

            pub fn checkout(&self, ref_name: &str) -> Result<Oid> {
                let (object, reference) = self.git_repo.revparse_ext(ref_name)?;
                self.git_repo.checkout_tree(&object, None)?;
                match reference {
                    Some(gref) => self.git_repo.set_head(gref.name().unwrap()),
                    None => self.git_repo.set_head_detached(object.id()),
                }?;
                Ok(self.git_repo.head()?.peel_to_commit()?.id())
            }
        }

        impl Drop for GitTestRepo {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.dir);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn dummy_nostr_state(state: HashMap<String, String>) -> RepoState {
        let keys = nostr::Keys::generate();
        let event = nostr::event::EventBuilder::new(STATE_KIND, "")
            .tags(vec![nostr::Tag::identifier("test")])
            .sign_with_keys(&keys)
            .unwrap();
        RepoState {
            identifier: "test".to_string(),
            state,
            event,
        }
    }

    fn dummy_decoded_url() -> NostrUrlDecoded {
        let keys = nostr::Keys::generate();
        NostrUrlDecoded {
            original_string: String::new(),
            coordinate: Nip19Coordinate {
                coordinate: Coordinate {
                    kind: Kind::Custom(30617),
                    public_key: keys.public_key(),
                    identifier: "test".to_string(),
                },
                relays: vec![],
            },
            protocol: None,
            ssh_key_file: None,
            nip05: None,
        }
    }

    fn remote_states_from(
        url: &str,
        refs: Vec<(&str, &str)>,
        is_grasp: bool,
    ) -> HashMap<String, (HashMap<String, String>, bool)> {
        let mut m = HashMap::new();
        let mut state = HashMap::new();
        for (k, v) in refs {
            state.insert(k.to_string(), v.to_string());
        }
        m.insert(url.to_string(), (state, is_grasp));
        m
    }

    // -----------------------------------------------------------------------
    // invalid_nostr_state_ref (existing tests)
    // -----------------------------------------------------------------------

    // Regression test: annotated-tag peeled refs (ending with ^{}) must be
    // treated as invalid nostr state refs so they are never used to build
    // git refspecs.  Before the fix, these passed through and caused git2 to
    // reject the push with "invalid refspec refs/remotes/origin/v1.4.4^{}:…".
    #[test]
    fn annotated_tag_peeled_ref_is_invalid() {
        assert!(
            invalid_nostr_state_ref("refs/tags/v1.4.4^{}"),
            "peeled annotated-tag ref must be invalid"
        );
        assert!(
            invalid_nostr_state_ref("refs/tags/v1.0.0^{}"),
            "peeled annotated-tag ref must be invalid"
        );
    }

    #[test]
    fn pr_ref_is_invalid() {
        assert!(
            invalid_nostr_state_ref("refs/heads/pr/42"),
            "PR branch refs must be invalid"
        );
    }

    #[test]
    fn arbitrary_non_heads_non_tags_ref_is_invalid() {
        assert!(
            invalid_nostr_state_ref("refs/notes/commits"),
            "refs outside heads/tags must be invalid"
        );
        assert!(invalid_nostr_state_ref("HEAD"), "HEAD must be invalid");
    }

    #[test]
    fn normal_branch_and_tag_refs_are_valid() {
        assert!(
            !invalid_nostr_state_ref("refs/heads/main"),
            "normal branch must be valid"
        );
        assert!(
            !invalid_nostr_state_ref("refs/heads/feature/foo"),
            "feature branch must be valid"
        );
        assert!(
            !invalid_nostr_state_ref("refs/tags/v1.4.4"),
            "lightweight tag must be valid"
        );
        assert!(
            !invalid_nostr_state_ref("refs/tags/v2.0.0-rc1"),
            "release-candidate tag must be valid"
        );
    }

    // -----------------------------------------------------------------------
    // find_ahead_refs
    // -----------------------------------------------------------------------

    #[test]
    fn server_in_sync_returns_empty() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        let oid_a = test_repo.populate().unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), oid_a.to_string());
            m
        });
        let remote_states = remote_states_from(
            "https://github.com/user/repo.git",
            vec![("refs/heads/main", &oid_a.to_string())],
            false,
        );
        let term = console::Term::stderr();
        let result = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );
        assert!(
            result.0.is_empty(),
            "in-sync server should not appear as ahead"
        );
        assert!(result.1.is_empty(), "in-sync server should not diverge");
    }

    #[test]
    fn server_strictly_ahead_is_detected() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        let oid_a = test_repo.populate().unwrap();

        // Add one more commit so that the "server" is at oid_b (one ahead).
        std::fs::write(test_repo.dir.join("extra.md"), "extra").unwrap();
        let oid_b = test_repo.stage_and_commit("add extra.md").unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), oid_a.to_string());
            m
        });
        let remote_states = remote_states_from(
            "https://github.com/user/repo.git",
            vec![("refs/heads/main", &oid_b.to_string())],
            false,
        );
        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );

        assert_eq!(ahead.len(), 1, "exactly one ref should be detected ahead");
        assert_eq!(ahead[0].ref_name, "refs/heads/main");
        assert_eq!(ahead[0].ahead_oid, oid_b.to_string());
        assert_eq!(ahead[0].commits_ahead, 1);
        assert!(
            diverging.is_empty(),
            "strictly ahead server should not diverge"
        );
    }

    #[test]
    fn server_behind_nostr_state_not_detected_as_ahead() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        let oid_a = test_repo.populate().unwrap();

        // Add commit so nostr is at oid_b; server stays at oid_a (behind).
        std::fs::write(test_repo.dir.join("extra.md"), "extra").unwrap();
        let oid_b = test_repo.stage_and_commit("add extra.md").unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            // Nostr state is at oid_b (the newer commit)
            m.insert("refs/heads/main".to_string(), oid_b.to_string());
            m
        });
        let remote_states = remote_states_from(
            "https://github.com/user/repo.git",
            vec![("refs/heads/main", &oid_a.to_string())],
            false,
        );
        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );
        assert!(
            ahead.is_empty(),
            "server behind nostr state should not appear as ahead"
        );
        assert!(
            diverging.is_empty(),
            "server behind nostr state should not appear as diverging"
        );
    }

    #[test]
    fn diverged_server_not_detected_as_ahead() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        test_repo.populate().unwrap();

        // Create a diverged branch and get its tip — this simulates the
        // server's commit not being an ancestor of the nostr state commit.
        test_repo.create_branch("diverge").unwrap();
        test_repo.checkout("diverge").unwrap();
        std::fs::write(test_repo.dir.join("server.md"), "server side").unwrap();
        let server_oid = test_repo
            .stage_and_commit("diverge: server commit")
            .unwrap();

        // Nostr state lives on main, which also moved forward independently.
        test_repo.checkout("main").unwrap();
        std::fs::write(test_repo.dir.join("nostr.md"), "nostr side").unwrap();
        let nostr_oid = test_repo.stage_and_commit("nostr commit").unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), nostr_oid.to_string());
            m
        });
        // Server reports main at server_oid (diverged)
        let remote_states = remote_states_from(
            "https://github.com/user/repo.git",
            vec![("refs/heads/main", &server_oid.to_string())],
            false,
        );
        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );
        assert!(
            ahead.is_empty(),
            "diverged server commit should not be detected as strictly ahead"
        );
        assert_eq!(diverging.len(), 1, "diverged server should be reported");
        assert_eq!(diverging[0].ref_name, "refs/heads/main");
        assert_eq!(diverging[0].commits_ahead, 1);
        assert_eq!(diverging[0].commits_behind, 1);
    }

    #[test]
    fn two_servers_ahead_consistent_takes_furthest() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        let oid_a = test_repo.populate().unwrap();

        std::fs::write(test_repo.dir.join("b.md"), "b").unwrap();
        let oid_b = test_repo.stage_and_commit("b").unwrap();

        std::fs::write(test_repo.dir.join("c.md"), "c").unwrap();
        let oid_c = test_repo.stage_and_commit("c").unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), oid_a.to_string());
            m
        });

        // Server 1 is at oid_b (1 ahead), server 2 is at oid_c (2 ahead).
        // oid_b is an ancestor of oid_c so there is no conflict.
        let mut remote_states: HashMap<String, (HashMap<String, String>, bool)> = HashMap::new();
        let mut s1 = HashMap::new();
        s1.insert("refs/heads/main".to_string(), oid_b.to_string());
        remote_states.insert("https://github.com/user/repo.git".to_string(), (s1, false));
        let mut s2 = HashMap::new();
        s2.insert("refs/heads/main".to_string(), oid_c.to_string());
        remote_states.insert(
            "https://codeberg.org/user/repo.git".to_string(),
            (s2, false),
        );

        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );

        assert_eq!(ahead.len(), 1, "should detect one ahead ref");
        assert_eq!(
            ahead[0].ahead_oid,
            oid_c.to_string(),
            "should pick the furthest commit"
        );
        assert_eq!(ahead[0].commits_ahead, 2);
        assert!(
            diverging.is_empty(),
            "consistent servers should not diverge"
        );
    }

    #[test]
    fn two_servers_diverged_from_each_other_skipped() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        test_repo.populate().unwrap();

        // Server 1 is on a different diverged path
        test_repo.create_branch("s1branch").unwrap();
        test_repo.checkout("s1branch").unwrap();
        std::fs::write(test_repo.dir.join("s1.md"), "s1").unwrap();
        let oid_s1 = test_repo.stage_and_commit("s1 commit").unwrap();

        // Server 2 is also ahead but diverged from server 1
        test_repo.checkout("main").unwrap();
        test_repo.create_branch("s2branch").unwrap();
        test_repo.checkout("s2branch").unwrap();
        std::fs::write(test_repo.dir.join("s2.md"), "s2").unwrap();
        let oid_s2 = test_repo.stage_and_commit("s2 commit").unwrap();

        // Nostr state at the common base (last commit of populate)
        let base_oid = test_repo
            .git_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), base_oid.to_string());
            m
        });

        let mut remote_states: HashMap<String, (HashMap<String, String>, bool)> = HashMap::new();
        let mut s1 = HashMap::new();
        s1.insert("refs/heads/main".to_string(), oid_s1.to_string());
        remote_states.insert(
            "https://server1.example.com/repo.git".to_string(),
            (s1, false),
        );
        let mut s2 = HashMap::new();
        s2.insert("refs/heads/main".to_string(), oid_s2.to_string());
        remote_states.insert(
            "https://server2.example.com/repo.git".to_string(),
            (s2, false),
        );

        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );

        assert!(
            ahead.is_empty(),
            "when servers disagree (diverged), ref should be skipped from ahead"
        );
        // Both servers are FF ahead of nostr (just not of each other), so
        // neither is a true divergence from nostr state — diverging is empty.
        assert!(
            diverging.is_empty(),
            "servers that are FF-ahead of nostr but diverged from each other \
             should not appear in diverging vec"
        );
    }

    #[test]
    fn symbolic_and_pr_refs_ignored() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        let oid_a = test_repo.populate().unwrap();
        std::fs::write(test_repo.dir.join("b.md"), "b").unwrap();
        let oid_b = test_repo.stage_and_commit("b").unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), oid_a.to_string());
            m
        });

        // Simulate a server that reports a symbolic HEAD and a PR ref ahead
        let mut srv_state = HashMap::new();
        srv_state.insert("HEAD".to_string(), "ref: refs/heads/main".to_string());
        srv_state.insert("refs/heads/pr/1".to_string(), oid_b.to_string());
        // Also a valid branch at oid_b to make sure the test exercises filtering
        // (it should still find main is NOT ahead here - server main still at oid_a)
        srv_state.insert("refs/heads/main".to_string(), oid_a.to_string());
        let mut remote_states: HashMap<String, (HashMap<String, String>, bool)> = HashMap::new();
        remote_states.insert(
            "https://github.com/user/repo.git".to_string(),
            (srv_state, false),
        );

        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );
        assert!(
            ahead.is_empty(),
            "symbolic refs and PR refs must be ignored"
        );
        assert!(
            diverging.is_empty(),
            "symbolic refs and PR refs must not appear as diverging"
        );
    }

    #[test]
    fn single_server_diverged_is_reported() {
        let test_repo = GitTestRepo::default();
        let git_repo = Repo::from_path(&test_repo.dir).unwrap();
        test_repo.populate().unwrap();

        // Server diverged: branch off the populate base and make a different commit
        test_repo.create_branch("server_side").unwrap();
        test_repo.checkout("server_side").unwrap();
        std::fs::write(test_repo.dir.join("server.md"), "server side").unwrap();
        let server_oid = test_repo.stage_and_commit("server commit").unwrap();

        // Nostr state lives on main, which also moved forward independently
        test_repo.checkout("main").unwrap();
        std::fs::write(test_repo.dir.join("nostr.md"), "nostr side").unwrap();
        let nostr_oid = test_repo.stage_and_commit("nostr commit").unwrap();

        let nostr_state = dummy_nostr_state({
            let mut m = HashMap::new();
            m.insert("refs/heads/main".to_string(), nostr_oid.to_string());
            m
        });
        let remote_states = remote_states_from(
            "https://github.com/user/repo.git",
            vec![("refs/heads/main", &server_oid.to_string())],
            false,
        );

        let term = console::Term::stderr();
        let (ahead, diverging) = find_ahead_and_diverging_refs(
            &git_repo,
            &nostr_state,
            &remote_states,
            &dummy_decoded_url(),
            &term,
        );

        assert!(ahead.is_empty(), "diverged server must not appear as ahead");
        assert_eq!(diverging.len(), 1, "diverged server should be reported");
        assert_eq!(diverging[0].ref_name, "refs/heads/main");
        assert_eq!(diverging[0].source_url, "https://github.com/user/repo.git");
        assert_eq!(diverging[0].commits_ahead, 1, "server is 1 ahead of nostr");
        assert_eq!(
            diverging[0].commits_behind, 1,
            "server is 1 behind nostr state"
        );
    }

    // -----------------------------------------------------------------------
    // is_url_domain_trusted
    // -----------------------------------------------------------------------

    #[test]
    fn trusted_domain_matches_exact_hostname() {
        let domains = vec!["github.com".to_string(), "codeberg.org".to_string()];
        assert!(is_url_domain_trusted(
            "https://github.com/user/repo.git",
            &domains
        ));
        assert!(is_url_domain_trusted(
            "https://codeberg.org/user/repo.git",
            &domains
        ));
    }

    #[test]
    fn untrusted_domain_not_matched() {
        let domains = vec!["github.com".to_string()];
        assert!(!is_url_domain_trusted(
            "https://evil.example.com/user/repo.git",
            &domains
        ));
    }

    #[test]
    fn empty_trusted_domains_never_matches() {
        assert!(!is_url_domain_trusted(
            "https://github.com/user/repo.git",
            &[]
        ));
    }

    #[test]
    fn domain_matching_is_case_insensitive() {
        let domains = vec!["GitHub.COM".to_lowercase()];
        assert!(is_url_domain_trusted(
            "https://GITHUB.COM/user/repo.git",
            &domains
        ));
    }
}
