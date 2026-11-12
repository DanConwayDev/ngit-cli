//! Workflow guidance shown by `--help`.
//!
//! The repository skill for coding agents is a small bootstrap that sends
//! agents here, so the rules and pitfalls of each workflow live beside the
//! command that owns it rather than in a document that ages separately from
//! the binary. The top-level long help maps tasks to command groups; group
//! help owns a workflow, including its plain-git steps; leaf help owns
//! arguments and output contracts.
//!
//! The text files are written for an 80-column terminal and rendered as-is.
//! When a feature changes, update the matching file here, not the skill.

pub const OVERVIEW: &str = include_str!("help/overview.txt");

pub const PR: &str = include_str!("help/pr.txt");
pub const ISSUE: &str = include_str!("help/issue.txt");
pub const REPO: &str = include_str!("help/repo.txt");
pub const CI: &str = include_str!("help/ci.txt");
pub const ACCOUNT: &str = include_str!("help/account.txt");
pub const SYNC: &str = include_str!("help/sync.txt");
pub const RELEASE: &str = include_str!("help/release.txt");
pub const CONTAINER: &str = include_str!("help/container.txt");
pub const NSITE: &str = include_str!("help/nsite.txt");
pub const SKILL: &str = include_str!("help/skill.txt");

pub const CI_STATUS: &str = include_str!("help/ci_status.txt");
pub const CI_INSPECT: &str = include_str!("help/ci_inspect.txt");
pub const CI_SECRETS: &str = include_str!("help/ci_secrets.txt");
pub const CI_SERVICE_STATUS: &str = include_str!("help/ci_service_status.txt");
pub const CI_RERUN: &str = include_str!("help/ci_rerun.txt");

pub const PR_POINTER: &str = "Workflow: `ngit pr --help` covers opening and updating PRs (including with `git push`), stacks, review and merging.";
pub const REPO_POINTER: &str =
    "Workflow: `ngit repo --help` covers publishing, hosting and membership.";
pub const CONTAINER_POINTER: &str = "Workflow: `ngit container --help` covers layouts, tags, merge versus replace and JSON results.";
pub const NSITE_POINTER: &str = "Workflow: `ngit nsite --help` covers what is uploaded, configuration, retries and JSON results.";

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::*;
    use crate::cli::Cli;

    /// Every text and the command path whose `--help` must show it.
    const NOTES: &[(&[&str], &str)] = &[
        (&[], OVERVIEW),
        (&["pr"], PR),
        (&["issue"], ISSUE),
        (&["repo"], REPO),
        (&["ci"], CI),
        (&["account"], ACCOUNT),
        (&["sync"], SYNC),
        (&["release"], RELEASE),
        (&["container"], CONTAINER),
        (&["nsite"], NSITE),
        (&["skill"], SKILL),
        (&["ci", "status"], CI_STATUS),
        (&["ci", "inspect"], CI_INSPECT),
        (&["ci", "secrets"], CI_SECRETS),
        (&["ci", "service", "status"], CI_SERVICE_STATUS),
        (&["ci", "rerun"], CI_RERUN),
        (&["send"], PR_POINTER),
        (&["pr", "send"], PR_POINTER),
        (&["merge"], PR_POINTER),
        (&["pr", "merge"], PR_POINTER),
        (&["init"], REPO_POINTER),
        (&["container", "publish"], CONTAINER_POINTER),
        (&["nsite", "publish"], NSITE_POINTER),
    ];

    fn help(path: &[&str], flag: &str) -> String {
        let mut args = vec!["ngit"];
        args.extend(path);
        args.push(flag);
        let Err(error) = Cli::try_parse_from(args) else {
            panic!("{flag} selected an operation at {path:?}");
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{path:?}");
        error.to_string()
    }

    #[test]
    fn long_help_shows_workflow_notes_verbatim() {
        for (path, notes) in NOTES {
            assert!(
                help(path, "--help").contains(notes.trim_end()),
                "`--help` at {path:?} does not show its notes as written"
            );
        }
    }

    #[test]
    fn short_help_stays_compact() {
        for (path, notes) in NOTES {
            let first_line = notes.lines().next().unwrap();
            assert!(
                !help(path, "-h").contains(first_line),
                "`-h` at {path:?} shows workflow notes"
            );
        }
        assert!(help(&[], "-h").contains("`ngit --help` ends with a workflow map"));
    }

    #[test]
    fn notes_fit_an_80_column_terminal() {
        for (path, notes) in NOTES {
            for line in notes.lines() {
                if notes.lines().count() > 1 {
                    assert!(
                        line.chars().count() <= 80,
                        "line too long in notes for {path:?}: {line}"
                    );
                }
                assert_eq!(line, line.trim_end(), "trailing space at {path:?}");
            }
        }
    }

    /// The bootstrap skill sends agents to `ngit --help`, so its map must
    /// name help that exists and carries workflow notes.
    #[test]
    fn workflow_map_routes_to_help_with_notes() {
        let command = Cli::command();
        let mut routed = Vec::new();
        for line in OVERVIEW.lines() {
            let Some((_, target)) = line.rsplit_once("  ngit ") else {
                continue;
            };
            let Some(name) = target.strip_suffix(" --help") else {
                continue;
            };
            let subcommand = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("workflow map names unknown command `{name}`"));
            assert!(
                subcommand.get_after_long_help().is_some(),
                "`ngit {name} --help` has no workflow notes"
            );
            routed.push(name.to_string());
        }
        for (path, _) in NOTES {
            if let [group] = path {
                let owner = command.find_subcommand(group).unwrap();
                if owner.has_subcommands() || *group == "sync" {
                    assert!(
                        routed.iter().any(|name| name == group),
                        "workflow map omits `ngit {group} --help`"
                    );
                }
            }
        }
        assert!(OVERVIEW.contains("ngit --customize"));
    }

    /// Workflows performed with plain git have no command of their own, so
    /// the map and the owning group must both describe them.
    #[test]
    fn git_native_pr_workflow_is_discoverable() {
        assert!(OVERVIEW.contains("`git push` of a `pr/` branch"));
        assert!(PR.contains("git push -u origin pr/my-feature"));
        assert!(PR.contains("push options cannot carry newlines"));
        assert!(ISSUE.contains("Auto-resolve from commits"));
    }

    /// The setup reminder sends maintainers to `ngit skill --help` for the
    /// instruction to copy, so the notes must show it exactly as
    /// `ngit skill install` appends it and name what silences the reminder.
    #[test]
    fn skill_help_shows_the_instruction_that_install_appends() {
        let instruction = ngit::agent_guidance::HELP_INSTRUCTION;
        let indented: String = instruction
            .lines()
            .map(|line| format!("    {line}\n"))
            .collect();
        assert!(SKILL.contains(&indented));
        for marker in ngit::agent_guidance::HELP_INSTRUCTION_MARKERS {
            assert!(instruction.contains(marker));
            assert!(SKILL.contains(&format!("`{marker}`")));
        }
        // The reminder no longer names the opt-out itself.
        assert!(SKILL.contains("ngit skill opt-out --local"));
    }
}
