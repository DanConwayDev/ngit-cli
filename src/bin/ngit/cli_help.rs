//! Workflow guidance shown by `--help`, and the layout it is shown in.
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
//!
//! Agents are the main readers of long help, so [`command`] reshapes the
//! derived command tree before clap renders it: `--help` uses clap's dense
//! one-line-per-option layout with the full descriptions and notes folded
//! in, and global options other than `--json` are listed once, at the top
//! level. The derived tree itself (`Cli::command()`) is left untouched for
//! the docs export.

use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgAction, Command, CommandFactory, FromArgMatches};

use crate::cli::Cli;

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

/// Shown by every subcommand in place of the global options, which only the
/// top-level help lists.
pub const GLOBAL_OPTIONS_POINTER: &str =
    "Other global options (--repo, --signer, ...) are listed by `ngit --help`.";

/// The one global option every subcommand keeps listing: agents read
/// output through it, and an option missing from a command's own help
/// tends to be left off.
const ALWAYS_LISTED_GLOBAL: &str = "json";

const HELP_FLAG_ID: &str = "help";

/// The command tree as the terminal presents it.
///
/// clap's long layout puts every description on its own lines, separated by
/// blank lines, and cannot be switched off for `--help`. Its short layout is
/// dense, so both help flags render that one. When `args` ask for long help
/// (`--help` or the `help` subcommand), each command's long description,
/// long option descriptions and workflow notes first replace their short
/// counterparts, so the dense layout loses no content.
///
/// Parsing is unchanged: global options stay accepted at every position.
/// They are only hidden from subcommand help.
pub fn command<T: AsRef<OsStr>>(args: &[T]) -> Command {
    let requested = |name: &str| {
        args.iter()
            .skip(1)
            .map(AsRef::as_ref)
            .take_while(|arg| *arg != OsStr::new("--"))
            .any(|arg| arg == OsStr::new(name))
    };
    let long = requested("--help") || requested("help");

    let mut root = Cli::command()
        // Unlimited: clap moves every description to its own line as soon as
        // one would not fit beside its option.
        .term_width(0)
        .disable_help_flag(true)
        .arg(
            Arg::new(HELP_FLAG_ID)
                .short('h')
                .long("help")
                .global(true)
                .action(ArgAction::HelpShort)
                .help("Print a summary with -h, or full help and workflow notes with --help"),
        );
    if long {
        root = fold_long_help(root);
    }
    // clap does not propagate a global option to a subcommand that already
    // has an argument of that name, which then passes its own on instead.
    let hidden_globals: Vec<Arg> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set() && arg.get_id() != HELP_FLAG_ID)
        .map(|arg| arg.clone().hide(arg.get_id() != ALWAYS_LISTED_GLOBAL))
        .collect();
    root.mut_subcommands(|subcommand| {
        let inherited: Vec<Arg> = hidden_globals
            .iter()
            .filter(|global| {
                subcommand
                    .get_arguments()
                    .all(|arg| arg.get_id() != global.get_id())
            })
            .cloned()
            .collect();
        shape_subcommand(subcommand.args(inherited), long)
    })
}

fn shape_subcommand(mut command: Command, long: bool) -> Command {
    // A parent lists each subcommand by its one-line `about`, so the
    // command's own help shows its description, wrapped, as `before_help`.
    let description = match command.get_long_about() {
        Some(long_about) if long => Some(long_about),
        _ => command.get_about(),
    };
    let description = description.map(|description| wrap(&description.to_string()));
    let mut about = "";
    if let Some(description) = description {
        command = command.before_help(description);
        about = "{before-help}";
    }
    if long {
        command = fold_long_help(command.long_about(None));
    }
    // `--version` reports the same version everywhere, so only the top level
    // lists it.
    command = command.disable_version_flag(true).arg(
        Arg::new("version")
            .short('V')
            .long("version")
            .action(ArgAction::Version)
            .hide(true),
    );
    command = command.help_template(format!(
        "{about}{{usage-heading}} {{usage}}\n\n{{all-args}}\n\n{GLOBAL_OPTIONS_POINTER}{{after-help}}"
    ));
    command.mut_subcommands(|subcommand| shape_subcommand(subcommand, long))
}

/// Wraps prose at 80 columns, the width the notes are written for. Indented
/// lines are left alone unless they are list items: an example command must
/// stay copyable.
fn wrap(text: &str) -> String {
    const WIDTH: usize = 80;
    let mut wrapped = Vec::new();
    for line in text.lines() {
        let body = line.trim_start();
        let indent = line.len() - body.len();
        let marker = body
            .split_once(' ')
            .map(|(first, _)| first)
            .filter(|first| *first == "-" || first.ends_with('.') && first.len() <= 3)
            .map_or(0, |first| first.len() + 1);
        if line.chars().count() <= WIDTH || (indent > 0 && marker == 0) {
            wrapped.push(line.to_string());
            continue;
        }
        let mut current = " ".repeat(indent);
        for word in body.split(' ') {
            if current.trim().is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.chars().count() > WIDTH {
                wrapped.push(std::mem::replace(
                    &mut current,
                    format!("{}{word}", " ".repeat(indent + marker)),
                ));
            } else {
                current.push(' ');
                current.push_str(word);
            }
        }
        wrapped.push(current);
    }
    wrapped.join("\n")
}

/// Moves a command's notes and long option descriptions into the fields
/// the short layout renders, leaving nothing that would make clap choose
/// its long layout.
fn fold_long_help(mut command: Command) -> Command {
    if let Some(notes) = command.get_after_long_help().cloned() {
        command = command.after_help(notes).after_long_help(None);
    }
    command.mut_args(fold_long_arg_help)
}

fn fold_long_arg_help(mut arg: Arg) -> Arg {
    if arg.is_hide_set() {
        return arg;
    }
    if let Some(help) = arg.get_long_help() {
        // One paragraph per line: a blank line inside a dense option list
        // reads as the end of the list.
        let help = help.ansi().to_string().replace("\n\n", "\n");
        arg = arg.help(help).long_help(None);
    }
    // Only the long layout describes each possible value.
    let values = arg.get_possible_values();
    if !arg.is_hide_possible_values_set()
        && values
            .iter()
            .any(|value| !value.is_hide_set() && value.get_help().is_some())
    {
        let mut help = arg
            .get_help()
            .map(|help| help.ansi().to_string())
            .unwrap_or_default();
        let defaults = arg.get_default_values();
        if !defaults.is_empty() && !arg.is_hide_default_value_set() {
            let defaults: Vec<_> = defaults
                .iter()
                .map(|value| value.to_string_lossy())
                .collect();
            help = format!("{help} [default: {}]", defaults.join(" "));
            arg = arg.hide_default_value(true);
        }
        help.push_str("\nPossible values:");
        for value in values.iter().filter(|value| !value.is_hide_set()) {
            help.push_str("\n- ");
            help.push_str(value.get_name());
            if let Some(description) = value.get_help() {
                help.push_str(": ");
                help.push_str(&description.ansi().to_string());
            }
        }
        arg = arg.help(help).hide_possible_values(true);
    }
    arg
}

/// Parses `args` like `Cli::try_parse_from`, rendering help and usage errors
/// from the terminal tree.
pub fn try_parse_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let mut command = command(&args);
    let matches = command.try_get_matches_from_mut(args)?;
    Cli::from_arg_matches(&matches).map_err(|error| error.format(&mut command))
}

/// Parses the process arguments, printing help or a usage error and exiting
/// as `Cli::parse` does.
pub fn parse() -> Cli {
    try_parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit())
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

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

    /// Help as the binary prints it: rendered from the terminal tree, not
    /// from the derived parser.
    fn help_error(args: &[&str]) -> clap::Error {
        let Err(error) = try_parse_from(args) else {
            panic!("{args:?} selected an operation");
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{args:?}");
        error
    }

    fn help(path: &[&str], flag: &str) -> String {
        let mut args = vec!["ngit"];
        args.extend(path);
        args.push(flag);
        help_error(&args).to_string()
    }

    /// The path of every command a user can ask for help on.
    fn command_paths() -> Vec<Vec<String>> {
        fn walk(command: &Command, path: &[String], paths: &mut Vec<Vec<String>>) {
            paths.push(path.to_vec());
            for subcommand in command.get_subcommands() {
                let mut path = path.to_vec();
                path.push(subcommand.get_name().to_string());
                walk(subcommand, &path, paths);
            }
        }
        let mut paths = Vec::new();
        walk(&Cli::command(), &[], &mut paths);
        paths
    }

    #[test]
    fn help_lists_options_without_blank_lines_between_them() {
        for path in command_paths() {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            for flag in ["-h", "--help"] {
                let help = help(&path, flag);
                let last = if path.is_empty() {
                    "--version"
                } else {
                    "--help"
                };
                let options: Vec<&str> = help
                    .lines()
                    .skip_while(|line| *line != "Options:")
                    .skip(1)
                    .take_while(|line| !line.is_empty())
                    .collect();
                assert!(
                    options.last().is_some_and(|line| line.contains(last)),
                    "`{flag}` at {path:?} separates its options: {options:?}"
                );
                for line in help.lines() {
                    assert_eq!(line, line.trim_end(), "trailing space at {path:?}");
                }
            }
        }
    }

    #[test]
    fn global_options_are_listed_once_at_the_top_level() {
        assert!(GLOBAL_OPTIONS_POINTER.chars().count() <= 80);
        for flag in ["-h", "--help"] {
            let top = help(&[], flag);
            for option in ["--json", "--repo", "--signer"] {
                assert!(top.contains(&format!("  {option} ")), "{flag}: {option}");
            }
            for option in ["--repo", "--signer"] {
                assert!(GLOBAL_OPTIONS_POINTER.contains(option));
            }
            assert!(!top.contains(GLOBAL_OPTIONS_POINTER));
        }
        for path in command_paths().into_iter().filter(|path| !path.is_empty()) {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            for flag in ["-h", "--help"] {
                let help = help(&path, flag);
                assert!(help.contains(GLOBAL_OPTIONS_POINTER), "{path:?}");
                let listed = |option: &str| {
                    help.lines()
                        .skip_while(|line| *line != "Options:")
                        .take_while(|line| !line.is_empty())
                        .any(|line| line.split_whitespace().any(|word| word == option))
                };
                for option in ["--repo", "--signer", "--nsec"] {
                    assert!(!listed(option), "{path:?} repeats {option}");
                }
                // Agents read output through --json, so it stays in view.
                assert!(listed("--json"), "{path:?} does not list --json");
                assert!(listed("--help"), "{path:?}");
                assert!(!listed("--version"), "{path:?}");
            }
        }
    }

    #[test]
    fn hidden_global_options_still_parse_at_every_position() {
        for args in [
            [
                "ngit", "--json", "--repo", "up", "--signer", "me", "issue", "list",
            ],
            [
                "ngit", "issue", "--json", "--repo", "up", "--signer", "me", "list",
            ],
            [
                "ngit", "issue", "list", "--json", "--repo", "up", "--signer", "me",
            ],
        ] {
            let cli = try_parse_from(args).unwrap();
            assert!(cli.json, "{args:?}");
            assert_eq!(cli.repo.as_deref(), Some("up"), "{args:?}");
            assert_eq!(cli.signer.as_deref(), Some("me"), "{args:?}");
        }
        assert!(try_parse_from(["ngit", "issue", "list", "--json", "-i"]).is_err());
    }

    /// Detail that only clap's long layout used to show stays on the command
    /// that owns it, and out of the summary.
    #[test]
    fn long_descriptions_stay_on_the_owning_command() {
        for (path, detail) in [
            (&["ci", "rerun"][..], "Repeat for other jobs."),
            (
                &["pr", "merge"],
                "run without an ID while on a `pr/` branch",
            ),
            (
                &["ci", "status"],
                "- auto: Include tails for every conclusion except success",
            ),
            (&["ci", "status"], "JSON output [default: auto]\n"),
        ] {
            assert!(help(path, "--help").contains(detail), "{path:?}: {detail}");
            assert!(!help(path, "-h").contains(detail), "{path:?}: {detail}");
        }
        // A group lists its subcommands by their one-line summaries.
        assert!(!help(&["pr"], "--help").contains("run without an ID while on"));
    }

    #[test]
    fn help_subcommand_matches_long_help() {
        for path in [&[][..], &["pr"], &["ci", "status"], &["pr", "merge"]] {
            let mut args = vec!["ngit", "help"];
            args.extend(path);
            assert_eq!(
                help_error(&args).to_string(),
                help(path, "--help"),
                "{path:?}"
            );
        }
    }

    #[test]
    fn command_descriptions_are_wrapped_for_80_columns() {
        for path in command_paths().into_iter().filter(|path| !path.is_empty()) {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            for flag in ["-h", "--help"] {
                let help = help(&path, flag);
                for line in help.lines().take_while(|line| !line.starts_with("Usage:")) {
                    assert!(
                        line.chars().count() <= 80 || line.starts_with(' '),
                        "`{flag}` at {path:?} has a long description line: {line}"
                    );
                }
            }
        }
        assert_eq!(
            wrap(&format!("  2. {}", "word ".repeat(20).trim_end())),
            format!(
                "  2. {}\n     {}",
                "word ".repeat(15).trim_end(),
                "word ".repeat(5).trim_end()
            )
        );
        let command = format!("  git push {}", "-o option=value ".repeat(6).trim_end());
        assert_eq!(wrap(&command), command);
    }

    #[test]
    fn version_is_accepted_but_listed_only_at_the_top_level() {
        assert!(help(&[], "-h").contains("-V, --version"));
        for args in [["ngit", "pr", "-V"], ["ngit", "pr", "--version"]] {
            let error = try_parse_from(args).err().unwrap();
            assert_eq!(error.kind(), ErrorKind::DisplayVersion);
            assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
        }
    }

    #[test]
    fn usage_errors_keep_the_usage_line_and_help_hint() {
        let error = try_parse_from(["ngit", "pr", "list", "--bogus"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        let message = error.to_string();
        assert!(message.contains("Usage: ngit pr list"));
        assert!(message.contains("try '--help'"));
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
    /// The global option descriptions are one line each; what they used to
    /// spell out lives in the conventions every agent reads.
    #[test]
    fn overview_carries_the_global_option_contracts() {
        assert!(OVERVIEW.contains("go to stderr as compact JSON Lines"));
        assert!(OVERVIEW.contains("git and subprocess output may still be plain text"));
        assert!(OVERVIEW.contains("overrides\n    `nostr.repo`, branch tracking"));
        let command = Cli::command();
        for global in command.get_arguments().filter(|arg| arg.is_global_set()) {
            assert!(
                global.get_long_help().is_none(),
                "global option {} has detail only `ngit --help` would show",
                global.get_id()
            );
        }
    }

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
