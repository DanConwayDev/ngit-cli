use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::{self, Read},
    process::{Command, ExitStatus, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow};
use console::Term;

use super::Repo;

/// Whether Git, rather than libgit2, should handle this URL.
///
/// Git dispatches unknown `<scheme>://` URLs to `git-remote-<scheme>` and
/// supports the explicit `<scheme>::<address>` helper form. Keeping that
/// dispatch in Git gives ngit every remote-helper capability without copying
/// Git's helper state machine.
pub(crate) fn handles_url(url: &str) -> bool {
    helper_scheme(url).is_some()
}

/// Whether a user-supplied PR server URL is a custom Git clone URL rather
/// than a GRASP base URL. ngit accepts `ws://` and `wss://` as GRASP bases,
/// even though Git could theoretically dispatch those schemes to helpers.
pub(crate) fn is_direct_pr_clone_url(url: &str) -> bool {
    handles_url(url)
        && !dispatch_scheme(url)
            .is_some_and(|scheme| matches!(scheme.to_ascii_lowercase().as_str(), "ws" | "wss"))
}

pub(crate) fn list(repo: &Repo, url: &str) -> Result<HashMap<String, String>> {
    let output = run_git(repo, ["ls-remote", "--symref", "--", url], None)?;
    ensure_success("list", url, &output)?;
    parse_ls_remote(std::str::from_utf8(&output.stdout).context("Git returned non-UTF-8 refs")?)
}

pub(crate) fn fetch(repo: &Repo, url: &str, oids: &[String], term: &Term) -> Result<()> {
    let mut args = vec![
        OsString::from("fetch"),
        OsString::from("--no-tags"),
        OsString::from("--no-write-fetch-head"),
        OsString::from("--no-recurse-submodules"),
        OsString::from("--"),
        OsString::from(url),
    ];
    args.extend(oids.iter().map(OsString::from));

    let output = run_git(repo, args, Some(term))?;
    ensure_success("fetch", url, &output)
}

pub(crate) fn push(
    repo: &Repo,
    url: &str,
    refspecs: &[String],
    term: &Term,
    push_options: &[&str],
) -> Result<HashMap<String, Option<String>>> {
    let mut args = vec![
        OsString::from("push"),
        OsString::from("--porcelain"),
        OsString::from("--no-verify"),
    ];
    args.extend(
        push_options
            .iter()
            .map(|option| OsString::from(format!("--push-option={option}"))),
    );
    args.push(OsString::from("--"));
    args.push(OsString::from(url));
    args.extend(refspecs.iter().map(OsString::from));

    let output = run_git(repo, args, Some(term))?;
    let stdout =
        std::str::from_utf8(&output.stdout).context("Git returned non-UTF-8 push status")?;
    let updates = parse_push_porcelain(stdout, refspecs);
    if output.status.success() || updates.values().any(Option::is_some) {
        Ok(updates)
    } else {
        Err(command_error("push", url, &output))
    }
}

struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_git<I, S>(repo: &Repo, args: I, term: Option<&Term>) -> Result<GitOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let git_dir = repo.git_repo.path();
    let current_dir = repo.git_repo.workdir().unwrap_or(git_dir);
    let mut command = Command::new("git");
    command
        // A transport operation should not trigger unrelated background work.
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(args)
        .current_dir(current_dir)
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_COMMON_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(worktree) = repo.git_repo.workdir() {
        command.env("GIT_WORK_TREE", worktree);
    } else {
        command.env_remove("GIT_WORK_TREE");
    }

    let mut child = command
        .spawn()
        .context("failed to run Git for the remote helper; ensure git is installed and on PATH")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture Git stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture Git stderr")?;
    let stdout_reader = thread::spawn(move || read_pipe(stdout, None));
    let stderr_reader = thread::spawn({
        let term = term.cloned();
        move || read_pipe(stderr, term)
    });

    let status = child
        .wait()
        .context("failed waiting for Git remote helper")?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("Git stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("Git stderr reader panicked"))??;
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe(mut pipe: impl Read, term: Option<Term>) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        if let Some(term) = &term {
            term.write_str(&String::from_utf8_lossy(&buffer[..read]))?;
            term.flush()?;
        }
    }
    Ok(output)
}

fn ensure_success(operation: &str, url: &str, output: &GitOutput) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(operation, url, output))
    }
}

fn command_error(operation: &str, url: &str, output: &GitOutput) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let hints = failure_hints(url);
    let message = if detail.is_empty() {
        format!("Git failed to {operation} {url}")
    } else {
        format!("Git failed to {operation} {url}: {detail}")
    };
    if hints.is_empty() {
        anyhow!(message)
    } else {
        anyhow!("{message}. Hint: {}", hints.join("; "))
    }
}

fn failure_hints(url: &str) -> Vec<String> {
    let scheme = dispatch_scheme(url);
    let mut hints = Vec::new();
    if let Some(scheme) = scheme {
        hints.push(format!(
            "Git's protocol.{scheme}.allow setting controls whether this transport may run"
        ));
        if uses_external_helper_program(url, scheme) {
            hints.push(format!(
                "Git dispatches {scheme} URLs to git-remote-{scheme}; ensure that executable is installed and on PATH"
            ));
            if scheme.eq_ignore_ascii_case("htree") {
                hints.push(
                    "install the htree helper with `cargo install git-remote-htree`".to_string(),
                );
            }
        }
    }
    hints
}

fn parse_ls_remote(output: &str) -> Result<HashMap<String, String>> {
    let mut state = HashMap::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let (value, name) = line
            .split_once('\t')
            .with_context(|| format!("could not parse Git ls-remote line: {line}"))?;
        if name.ends_with("^{}") {
            continue;
        }
        if let Some(target) = value.strip_prefix("ref: ") {
            state.insert(name.to_string(), format!("ref: {target}"));
        } else if !state
            .get(name)
            .is_some_and(|value| value.starts_with("ref: "))
        {
            state.insert(name.to_string(), value.to_string());
        }
    }
    Ok(state)
}

fn parse_push_porcelain(output: &str, refspecs: &[String]) -> HashMap<String, Option<String>> {
    let mut updates = refspecs
        .iter()
        .filter_map(|refspec| destination_ref(refspec).map(|dst| (dst.to_string(), None)))
        .collect::<HashMap<_, _>>();

    for line in output.lines() {
        let mut fields = line.split('\t');
        let Some(flag) = fields.next().and_then(|field| field.chars().next()) else {
            continue;
        };
        if !matches!(flag, ' ' | '+' | '-' | '*' | '!' | '=') {
            continue;
        }
        let Some(destination) = fields
            .next()
            .and_then(|refs| refs.rsplit_once(':'))
            .map(|(_, destination)| destination)
        else {
            continue;
        };
        let summary = fields.next().unwrap_or("push rejected").trim();
        updates.insert(
            destination.to_string(),
            (flag == '!').then(|| summary.to_string()),
        );
    }
    updates
}

fn destination_ref(refspec: &str) -> Option<&str> {
    refspec
        .strip_prefix('+')
        .unwrap_or(refspec)
        .rsplit_once(':')
        .map(|(_, destination)| destination)
        .filter(|destination| !destination.is_empty())
}

fn helper_scheme(url: &str) -> Option<&str> {
    let (scheme, suffix) = scheme_and_suffix(url)?;
    let explicit_helper = suffix.starts_with(':');

    if explicit_helper
        || !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git" | "ftp"
        )
    {
        Some(scheme)
    } else {
        None
    }
}

fn dispatch_scheme(url: &str) -> Option<&str> {
    scheme_and_suffix(url).map(|(scheme, _)| scheme)
}

fn uses_external_helper_program(url: &str, scheme: &str) -> bool {
    scheme_and_suffix(url).is_some_and(|(_, suffix)| suffix.starts_with(':'))
        || !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "file" | "ssh+git" | "git+ssh"
        )
}

fn scheme_and_suffix(url: &str) -> Option<(&str, &str)> {
    let (scheme, suffix) = url.split_once(':')?;
    (valid_scheme(scheme) && (suffix.starts_with(':') || suffix.starts_with("//")))
        .then_some((scheme, suffix))
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        process::Command,
    };

    use anyhow::{Context, Result, ensure};
    use nostr::{
        PublicKey,
        nips::{nip01::Coordinate, nip19::Nip19Coordinate},
    };
    use tempfile::TempDir;

    use super::*;
    use crate::git::nostr_url::NostrUrlDecoded;

    struct Repositories {
        _temp: TempDir,
        source_path: PathBuf,
        remote_path: PathBuf,
        destination: Repo,
        oid: String,
    }

    impl Repositories {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let source_path = temp.path().join("source");
            let remote_path = temp.path().join("remote.git");
            let destination_path = temp.path().join("destination");

            run(
                temp.path(),
                ["init", "-q", "-b", "main", path(&source_path)],
            )?;
            run(&source_path, ["config", "user.name", "ngit test"])?;
            run(
                &source_path,
                ["config", "user.email", "ngit-test@example.invalid"],
            )?;
            run(
                &source_path,
                ["commit", "-q", "--allow-empty", "-m", "initial"],
            )?;
            let oid = run(&source_path, ["rev-parse", "HEAD"])?;

            run(temp.path(), ["init", "-q", "--bare", path(&remote_path)])?;
            run(
                &source_path,
                ["push", "-q", path(&remote_path), "main:refs/heads/main"],
            )?;
            run(
                temp.path(),
                [
                    "--git-dir",
                    path(&remote_path),
                    "symbolic-ref",
                    "HEAD",
                    "refs/heads/main",
                ],
            )?;

            run(
                temp.path(),
                ["init", "-q", "-b", "main", path(&destination_path)],
            )?;
            run(
                &destination_path,
                ["config", "protocol.ext.allow", "always"],
            )?;
            run(&source_path, ["config", "protocol.ext.allow", "always"])?;

            Ok(Self {
                _temp: temp,
                source_path,
                remote_path,
                destination: Repo::from_path(&destination_path)?,
                oid,
            })
        }

        fn helper_url(&self) -> String {
            format!("ext::%S {}", self.remote_path.display())
        }
    }

    fn path(path: &Path) -> &str {
        path.to_str().expect("test paths are UTF-8")
    }

    fn run<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
        let output = Command::new("git").current_dir(cwd).args(args).output()?;
        ensure!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    fn decoded_nostr_url() -> NostrUrlDecoded {
        NostrUrlDecoded {
            original_string: String::new(),
            coordinate: Nip19Coordinate {
                coordinate: Coordinate {
                    identifier: "remote-helper-test".to_string(),
                    public_key: PublicKey::parse(
                        "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                    )
                    .expect("valid public key"),
                    kind: nostr::Kind::GitRepoAnnouncement,
                },
                relays: vec![],
            },
            protocol: None,
            ssh_key_file: None,
            nip05: None,
        }
    }

    #[test]
    fn recognizes_urls_that_need_git_dispatch() {
        for url in [
            "htree://self/project",
            "ext::%S /tmp/project.git",
            "file:///tmp/project.git",
            "git+ssh://example.com/project.git",
            "custom.v1+git://example/project",
            "custom://[::1]/project",
        ] {
            assert!(handles_url(url), "{url}");
        }

        for url in [
            "https://example.com/project.git",
            "http://example.com/project.git",
            "ssh://example.com/project.git",
            "git://example.com/project.git",
            "ftp://example.com/project.git",
            "git@example.com:project.git",
            "/tmp/project.git",
            "1invalid://project",
        ] {
            assert!(!handles_url(url), "{url}");
        }
    }

    #[test]
    fn distinguishes_custom_clone_urls_from_websocket_grasp_bases() {
        assert!(is_direct_pr_clone_url("htree://self/project"));
        assert!(is_direct_pr_clone_url("ext::%S /tmp/project.git"));
        assert!(!is_direct_pr_clone_url("ws://relay.example.com"));
        assert!(!is_direct_pr_clone_url("wss://relay.example.com"));
    }

    #[test]
    fn lists_refs_and_preserves_symbolic_head_through_external_helper() -> Result<()> {
        let repos = Repositories::new()?;
        let term = Term::buffered_stderr();

        let state = crate::list::list_from_remote(
            &term,
            &repos.destination,
            &repos.helper_url(),
            &decoded_nostr_url(),
            false,
        )?;

        assert_eq!(
            state.get("HEAD").map(String::as_str),
            Some("ref: refs/heads/main")
        );
        assert_eq!(
            state.get("refs/heads/main").map(String::as_str),
            Some(repos.oid.as_str())
        );
        Ok(())
    }

    #[test]
    fn fetches_a_raw_object_without_creating_refs_or_fetch_head() -> Result<()> {
        let repos = Repositories::new()?;
        let term = Term::buffered_stderr();

        crate::fetch::fetch_from_git_server(
            &repos.destination,
            std::slice::from_ref(&repos.oid),
            &repos.helper_url(),
            &decoded_nostr_url(),
            &term,
            false,
        )?;

        let oid = git2::Oid::from_str(&repos.oid)?;
        assert!(repos.destination.git_repo.find_commit(oid).is_ok());
        assert!(repos.destination.git_repo.references()?.next().is_none());
        assert!(
            !repos
                .destination
                .git_repo
                .path()
                .join("FETCH_HEAD")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn pushes_and_reports_each_destination_ref_through_external_helper() -> Result<()> {
        let repos = Repositories::new()?;
        let source = Repo::from_path(&repos.source_path)?;
        let term = Term::buffered_stderr();
        let refspec = "HEAD:refs/heads/helper-push".to_string();

        let updates = crate::push::push_to_remote(
            &source,
            &repos.helper_url(),
            &decoded_nostr_url(),
            std::slice::from_ref(&refspec),
            &term,
            false,
            &[],
        )?;

        assert_eq!(
            updates,
            HashMap::from([("refs/heads/helper-push".to_string(), None)])
        );
        let pushed = run(
            repos._temp.path(),
            [
                "--git-dir",
                path(&repos.remote_path),
                "rev-parse",
                "refs/heads/helper-push",
            ],
        )?;
        assert_eq!(pushed, repos.oid);
        Ok(())
    }

    #[test]
    fn returns_per_ref_rejection_instead_of_losing_git_push_status() -> Result<()> {
        let repos = Repositories::new()?;
        run(
            &repos.source_path,
            ["commit", "-q", "--allow-empty", "-m", "remote advances"],
        )?;
        run(
            &repos.source_path,
            [
                "push",
                "-q",
                path(&repos.remote_path),
                "main:refs/heads/main",
            ],
        )?;
        run(&repos.source_path, ["reset", "-q", "--hard", &repos.oid])?;
        let source = Repo::from_path(&repos.source_path)?;

        let updates = crate::push::push_to_remote(
            &source,
            &repos.helper_url(),
            &decoded_nostr_url(),
            &["HEAD:refs/heads/main".to_string()],
            &Term::buffered_stderr(),
            false,
            &[],
        )?;

        let error = updates
            .get("refs/heads/main")
            .and_then(Option::as_deref)
            .context("main push should be rejected")?;
        assert!(error.contains("non-fast-forward"));
        Ok(())
    }

    #[test]
    fn honors_git_protocol_policy() -> Result<()> {
        let repos = Repositories::new()?;
        let workdir = repos.destination.git_repo.workdir().context("workdir")?;
        run(workdir, ["config", "protocol.ext.allow", "never"])?;

        let error = list(&repos.destination, &repos.helper_url()).unwrap_err();

        assert!(error.to_string().contains("protocol.ext.allow"));
        Ok(())
    }

    #[test]
    fn names_the_missing_helper_in_errors() -> Result<()> {
        let repos = Repositories::new()?;

        let error = list(&repos.destination, "ngit-missing-helper://example/project").unwrap_err();

        assert!(error.to_string().contains("git-remote-ngit-missing-helper"));
        Ok(())
    }

    #[test]
    fn htree_failure_hint_names_the_install_command() {
        let hints = failure_hints("htree://self/project").join(" ");

        assert!(hints.contains("git-remote-htree"));
        assert!(hints.contains("cargo install git-remote-htree"));
    }

    #[test]
    fn parses_per_ref_push_rejections() {
        let output = concat!(
            "To example\n",
            "!\tHEAD:refs/heads/main\t[rejected] (non-fast-forward)\n",
            "*\tHEAD:refs/heads/new\t[new branch]\n",
            "Done\n",
        );
        let updates = parse_push_porcelain(
            output,
            &[
                "HEAD:refs/heads/main".to_string(),
                "HEAD:refs/heads/new".to_string(),
            ],
        );

        assert_eq!(
            updates.get("refs/heads/main").and_then(Option::as_deref),
            Some("[rejected] (non-fast-forward)")
        );
        assert_eq!(updates.get("refs/heads/new"), Some(&None));
    }
}
