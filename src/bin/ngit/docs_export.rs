//! Deterministic, source-owned CLI metadata for documentation builds.
//!
//! Stable Clap reflection exposes effective conflicts and explicit argument
//! groups. Conditional requirements are deliberately outside schema v1 because
//! Clap does not expose them through its stable reflection API.

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Arg, ArgAction, Command, CommandFactory, builder::ValueRange};
use serde::Serialize;

use crate::cli::Cli;

pub const INTERNAL_COMMAND: &str = "__docs-export";
const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct DocsExport {
    schema_version: u32,
    capabilities: SchemaCapabilities,
    product: Product,
    command: CommandDoc,
}

#[derive(Serialize)]
struct SchemaCapabilities {
    argument_conflicts: bool,
    argument_groups: bool,
    conditional_requirements: bool,
}

#[derive(Serialize)]
struct Product {
    id: &'static str,
    name: &'static str,
    version: &'static str,
    description: &'static str,
    homepage: &'static str,
    source_repository: &'static str,
    source_commit: Option<&'static str>,
}

#[derive(Serialize)]
struct CommandDoc {
    id: String,
    name: String,
    path: Vec<String>,
    usage: String,
    about: Option<String>,
    long_about: Option<String>,
    before_help: Option<String>,
    after_help: Option<String>,
    aliases: Vec<AliasDoc>,
    hidden: bool,
    subcommand_required: bool,
    arg_required_else_help: bool,
    args: Vec<ArgDoc>,
    groups: Vec<ArgGroupDoc>,
    subcommands: Vec<Self>,
}

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct ArgDoc {
    id: String,
    name: String,
    kind: &'static str,
    short: Option<char>,
    long: Option<String>,
    aliases: Vec<ArgAliasDoc>,
    help: Option<String>,
    long_help: Option<String>,
    value_names: Vec<String>,
    action: &'static str,
    value_cardinality: Cardinality,
    repeatable: bool,
    required: bool,
    global: bool,
    hidden: bool,
    exclusive: bool,
    conflicts: Vec<String>,
    positional_index: Option<usize>,
    env: Option<String>,
    defaults: Vec<String>,
    possible_values: Vec<PossibleValueDoc>,
    value_delimiter: Option<char>,
    value_terminator: Option<String>,
    require_equals: bool,
    allow_hyphen_values: bool,
    allow_negative_numbers: bool,
    trailing_var_arg: bool,
    last: bool,
}

#[derive(Serialize)]
struct ArgGroupDoc {
    id: String,
    name: String,
    arguments: Vec<String>,
    required: bool,
    multiple: bool,
}

#[derive(Serialize)]
struct Cardinality {
    min: usize,
    /// `None` means that Clap accepts an unbounded number of values.
    max: Option<usize>,
}

#[derive(Serialize)]
struct AliasDoc {
    name: String,
    visible: bool,
}

#[derive(Serialize)]
struct ArgAliasDoc {
    name: String,
    kind: &'static str,
    visible: bool,
}

#[derive(Serialize)]
struct PossibleValueDoc {
    name: String,
    aliases: Vec<String>,
    help: Option<String>,
    hidden: bool,
}

pub fn write_stdout() -> Result<()> {
    let export = build();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    serde_json::to_writer_pretty(&mut writer, &export)
        .context("failed to serialize CLI documentation")?;
    writer
        .write_all(b"\n")
        .context("failed to write CLI documentation")?;
    Ok(())
}

fn build() -> DocsExport {
    let mut command = Cli::command();
    // Clap fills in inherited globals, positional indexes, generated flags,
    // and complete bin names during this pass. Export only after it so the
    // artifact describes the same command model the parser uses.
    command.build();

    DocsExport {
        schema_version: SCHEMA_VERSION,
        capabilities: SchemaCapabilities {
            argument_conflicts: true,
            argument_groups: true,
            // Clap 4 has no stable reflection API for `Arg::requires`.
            conditional_requirements: false,
        },
        product: Product {
            id: env!("CARGO_PKG_NAME"),
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            description: env!("CARGO_PKG_DESCRIPTION"),
            homepage: env!("CARGO_PKG_HOMEPAGE"),
            source_repository: env!("CARGO_PKG_REPOSITORY"),
            // Release automation may inject this. When it is unavailable the
            // docs packager pins the commit alongside the exported artifact.
            source_commit: option_env!("NGIT_DOCS_SOURCE_COMMIT"),
        },
        command: command_doc(&command, &[command.get_name().to_string()]),
    }
}

fn command_doc(command: &Command, path: &[String]) -> CommandDoc {
    let id = semantic_command_id(path);
    let mut usage_command = command.clone();
    let usage = usage_command.render_usage().to_string();

    let visible_aliases: Vec<_> = command.get_visible_aliases().collect();
    let mut aliases: Vec<_> = command
        .get_all_aliases()
        .map(|alias| AliasDoc {
            name: alias.to_string(),
            visible: visible_aliases.contains(&alias),
        })
        .collect();
    aliases.sort_by(|left, right| left.name.cmp(&right.name));

    let args = command
        .get_arguments()
        .map(|argument| arg_doc(command, argument, &id))
        .collect();
    let groups = group_docs(command, &id);
    let subcommands = command
        .get_subcommands()
        // Clap synthesizes `help`; it is parser infrastructure rather than a
        // source-owned command that needs its own reference page.
        .filter(|subcommand| subcommand.get_name() != "help")
        .map(|subcommand| {
            let mut subcommand_path = path.to_vec();
            subcommand_path.push(subcommand.get_name().to_string());
            command_doc(subcommand, &subcommand_path)
        })
        .collect();

    CommandDoc {
        id,
        name: command.get_name().to_string(),
        path: path.to_vec(),
        usage,
        about: styled(command.get_about()),
        long_about: styled(command.get_long_about()),
        before_help: styled(command.get_before_help()),
        after_help: styled(command.get_after_help()),
        aliases,
        hidden: command.is_hide_set(),
        subcommand_required: command.is_subcommand_required_set(),
        arg_required_else_help: command.is_arg_required_else_help_set(),
        args,
        groups,
        subcommands,
    }
}

fn arg_doc(command: &Command, argument: &Arg, command_id: &str) -> ArgDoc {
    let name = argument.get_id().as_str();
    let action = argument.get_action();
    let value_range = argument.get_num_args().unwrap_or_else(|| {
        if action.takes_values() {
            ValueRange::SINGLE
        } else {
            ValueRange::EMPTY
        }
    });
    let max = value_range
        .max_values()
        .ne(&usize::MAX)
        .then(|| value_range.max_values());

    ArgDoc {
        id: format!("{command_id}.argument.{name}"),
        name: name.to_string(),
        kind: if argument.get_index().is_some() {
            "positional"
        } else if action.takes_values() {
            "option"
        } else {
            "flag"
        },
        short: argument.get_short(),
        long: argument.get_long().map(ToString::to_string),
        aliases: arg_aliases(argument),
        help: styled(argument.get_help()),
        long_help: styled(argument.get_long_help()),
        value_names: argument
            .get_value_names()
            .unwrap_or_default()
            .iter()
            .map(ToString::to_string)
            .collect(),
        action: action_name(action),
        value_cardinality: Cardinality {
            min: value_range.min_values(),
            max,
        },
        repeatable: matches!(action, ArgAction::Append | ArgAction::Count),
        required: argument.is_required_set(),
        global: argument.is_global_set(),
        hidden: argument.is_hide_set(),
        exclusive: argument.is_exclusive_set(),
        conflicts: arg_conflicts(command, argument, command_id),
        positional_index: argument.get_index(),
        env: argument
            .get_env()
            .map(|value| value.to_string_lossy().into_owned()),
        defaults: argument
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect(),
        possible_values: possible_values(argument),
        value_delimiter: argument.get_value_delimiter(),
        value_terminator: argument.get_value_terminator().map(ToString::to_string),
        require_equals: argument.is_require_equals_set(),
        allow_hyphen_values: argument.is_allow_hyphen_values_set(),
        allow_negative_numbers: argument.is_allow_negative_numbers_set(),
        trailing_var_arg: argument.is_trailing_var_arg_set(),
        last: argument.is_last_set(),
    }
}

fn arg_conflicts(command: &Command, argument: &Arg, command_id: &str) -> Vec<String> {
    let argument_id = argument.get_id();
    let mut conflicts: Vec<_> = command
        .get_arguments()
        .filter(|candidate| candidate.get_id() != argument_id)
        .filter(|candidate| {
            command
                .get_arg_conflicts_with(argument)
                .iter()
                .any(|conflict| conflict.get_id() == candidate.get_id())
                || command
                    .get_arg_conflicts_with(candidate)
                    .iter()
                    .any(|conflict| conflict.get_id() == argument_id)
        })
        .map(|conflict| semantic_arg_id(command_id, conflict.get_id().as_str()))
        .collect();
    conflicts.sort();
    conflicts.dedup();
    conflicts
}

fn group_docs(command: &Command, command_id: &str) -> Vec<ArgGroupDoc> {
    let mut groups: Vec<_> = command
        .get_groups()
        .map(|group| {
            let name = group.get_id().as_str();
            let mut arguments: Vec<_> = group
                .get_args()
                .map(|argument| semantic_arg_id(command_id, argument.as_str()))
                .collect();
            arguments.sort();

            let mut group = group.clone();
            ArgGroupDoc {
                id: format!("{command_id}.group.{name}"),
                name: name.to_string(),
                arguments,
                required: group.is_required_set(),
                multiple: group.is_multiple(),
            }
        })
        .collect();
    groups.sort_by(|left, right| left.id.cmp(&right.id));
    groups
}

fn arg_aliases(argument: &Arg) -> Vec<ArgAliasDoc> {
    let visible_long = argument.get_visible_aliases().unwrap_or_default();
    let mut aliases: Vec<_> = argument
        .get_all_aliases()
        .unwrap_or_default()
        .into_iter()
        .map(|alias| ArgAliasDoc {
            name: alias.to_string(),
            kind: "long",
            visible: visible_long.contains(&alias),
        })
        .collect();
    let visible_short = argument.get_visible_short_aliases().unwrap_or_default();
    aliases.extend(
        argument
            .get_all_short_aliases()
            .unwrap_or_default()
            .into_iter()
            .map(|alias| ArgAliasDoc {
                name: alias.to_string(),
                kind: "short",
                visible: visible_short.contains(&alias),
            }),
    );
    aliases.sort_by(|left, right| {
        (left.kind, left.name.as_str()).cmp(&(right.kind, right.name.as_str()))
    });
    aliases
}

fn possible_values(argument: &Arg) -> Vec<PossibleValueDoc> {
    argument
        .get_possible_values()
        .into_iter()
        .map(|possible| {
            let name = possible.get_name();
            let mut aliases: Vec<_> = possible
                .get_name_and_aliases()
                .filter(|alias| *alias != name)
                .map(ToString::to_string)
                .collect();
            aliases.sort();
            PossibleValueDoc {
                name: name.to_string(),
                aliases,
                help: styled(possible.get_help()),
                hidden: possible.is_hide_set(),
            }
        })
        .collect()
}

fn semantic_command_id(path: &[String]) -> String {
    format!(
        "ngit.command.{}",
        path.iter().skip(1).cloned().collect::<Vec<_>>().join(".")
    )
    .trim_end_matches('.')
    .to_string()
}

fn semantic_arg_id(command_id: &str, argument_name: &str) -> String {
    format!("{command_id}.argument.{argument_name}")
}

fn action_name(action: &ArgAction) -> &'static str {
    match action {
        ArgAction::Set => "set",
        ArgAction::Append => "append",
        ArgAction::SetTrue => "set_true",
        ArgAction::SetFalse => "set_false",
        ArgAction::Count => "count",
        ArgAction::Help => "help",
        ArgAction::HelpShort => "help_short",
        ArgAction::HelpLong => "help_long",
        ArgAction::Version => "version",
        _ => "other",
    }
}

fn styled(value: Option<&clap::builder::StyledStr>) -> Option<String> {
    value.map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    use super::*;

    fn json() -> String {
        serde_json::to_string_pretty(&build()).unwrap()
    }

    fn command_at_path<'a>(root: &'a Value, path: &[&str]) -> &'a Value {
        let mut command = root;
        for name in path {
            command = command["subcommands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["name"] == *name)
                .unwrap_or_else(|| panic!("command {name} missing"));
        }
        command
    }

    fn collect_command_ids(command: &Value, ids: &mut HashSet<String>) {
        assert!(ids.insert(command["id"].as_str().unwrap().to_string()));
        assert!(
            command["subcommands"]
                .as_array()
                .unwrap()
                .iter()
                .all(|subcommand| subcommand["name"] != "help")
        );
        for subcommand in command["subcommands"].as_array().unwrap() {
            collect_command_ids(subcommand, ids);
        }
    }

    #[test]
    fn export_is_deterministic() {
        assert_eq!(json(), json());
    }

    #[test]
    fn export_has_stable_schema_and_clap_derived_usage() {
        let export: Value = serde_json::from_str(&json()).unwrap();
        assert_eq!(export["schema_version"], SCHEMA_VERSION);
        assert_eq!(
            export["capabilities"],
            serde_json::json!({
                "argument_conflicts": true,
                "argument_groups": true,
                "conditional_requirements": false
            })
        );
        assert_eq!(export["product"]["id"], "ngit");
        assert_eq!(export["product"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(export["command"]["id"], "ngit.command");
        assert_eq!(export["command"]["path"], serde_json::json!(["ngit"]));

        let pr_list = command_at_path(&export["command"], &["pr", "list"]);
        assert_eq!(pr_list["id"], "ngit.command.pr.list");
        assert_eq!(pr_list["path"], serde_json::json!(["ngit", "pr", "list"]));
        assert!(pr_list["usage"].as_str().unwrap().contains("ngit pr list"));
        assert!(pr_list["args"].as_array().unwrap().iter().any(|argument| {
            argument["long"] == "status"
                && argument["defaults"] == serde_json::json!(["open,draft"])
                && argument["value_cardinality"] == serde_json::json!({"min": 1, "max": 1})
        }));
        assert!(pr_list["args"].as_array().unwrap().iter().any(|argument| {
            argument["long"] == "json"
                && argument["global"] == true
                && argument["action"] == "set_true"
        }));

        let ci_status = command_at_path(&export["command"], &["ci", "status"]);
        assert!(
            ci_status["long_about"]
                .as_str()
                .unwrap()
                .contains("Each job line includes its provider-published log URL when present.")
        );
        assert!(
            ci_status["long_about"].as_str().unwrap().contains(
                "Signed per-job log tails are included for non-successful jobs by default"
            )
        );
        assert!(
            ci_status["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|argument| {
                    argument["long"] == "log-tail"
                        && argument["defaults"] == serde_json::json!(["auto"])
                        && argument["possible_values"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|value| value["name"].as_str().unwrap())
                            .eq(["auto", "all", "none"])
                })
        );

        let pr_view = command_at_path(&export["command"], &["pr", "view"]);
        assert!(pr_view["args"].as_array().unwrap().iter().any(|argument| {
            argument["long"] == "log-tail" && argument["defaults"] == serde_json::json!(["auto"])
        }));

        let release = command_at_path(&export["command"], &["release"]);
        assert!(
            release["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|alias| alias == &serde_json::json!({"name": "releases", "visible": false}))
        );

        let nsite_publish = command_at_path(&export["command"], &["nsite", "publish"]);
        assert_eq!(nsite_publish["id"], "ngit.command.nsite.publish");
        assert!(
            nsite_publish["usage"]
                .as_str()
                .unwrap()
                .contains("ngit nsite publish")
        );

        let container = command_at_path(&export["command"], &["container"]);
        assert!(
            container["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|alias| alias == &serde_json::json!({"name": "oci", "visible": true}))
        );
        let publish = command_at_path(&export["command"], &["container", "publish"]);
        assert!(publish["args"].as_array().unwrap().iter().any(|argument| {
            argument["long"] == "blossom-server" && argument["required"] == false
        }));

        let root_args = export["command"]["args"].as_array().unwrap();
        let defaults = root_args
            .iter()
            .find(|argument| argument["name"] == "defaults")
            .unwrap();
        assert_eq!(
            defaults["conflicts"],
            serde_json::json!(["ngit.command.argument.interactive"])
        );
        let interactive = root_args
            .iter()
            .find(|argument| argument["name"] == "interactive")
            .unwrap();
        assert_eq!(
            interactive["conflicts"],
            serde_json::json!([
                "ngit.command.argument.defaults",
                "ngit.command.argument.json"
            ])
        );

        let skill_opt_out = command_at_path(&export["command"], &["skill", "opt-out"]);
        assert!(
            skill_opt_out["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|group| {
                    group["arguments"]
                        == serde_json::json!([
                            "ngit.command.skill.opt-out.argument.global",
                            "ngit.command.skill.opt-out.argument.local"
                        ])
                        && group["required"] == true
                        && group["multiple"] == false
                })
        );

        let mut ids = HashSet::new();
        collect_command_ids(&export["command"], &mut ids);
    }
}
