use anyhow::{Result, bail};
use ngit::cli_interactor::PromptInputParms;

use crate::cli::{AccountCommands, Cli, Commands, IssueCommands, PrCommands};

/// Complete required composition fields before login, signing or publication.
/// Explicit values are never replaced, including values that fail validation.
pub fn complete(
    cli: &mut Cli,
    mut input: impl FnMut(PromptInputParms) -> Result<String>,
) -> Result<()> {
    let field = match cli.command.as_mut() {
        Some(Commands::Account(args)) => match &mut args.account_command {
            AccountCommands::Create(args) => Some((&mut args.name, "display name", "--name")),
            _ => None,
        },
        Some(Commands::Issue(args)) => match &mut args.issue_command {
            IssueCommands::Comment { body, .. } => Some((body, "comment body", "--body")),
            IssueCommands::SetSubject { subject, .. } => {
                Some((subject, "new subject", "--subject"))
            }
            IssueCommands::SetCoverNote { body, .. } => Some((body, "cover note", "--body")),
            IssueCommands::Label { labels, .. } => {
                return complete_labels(labels, cli.interactive, input);
            }
            _ => None,
        },
        Some(Commands::Pr(args)) => match &mut args.pr_command {
            PrCommands::Comment { body, .. } => Some((body, "comment body", "--body")),
            PrCommands::SetSubject { subject, .. } => Some((subject, "new subject", "--subject")),
            PrCommands::SetCoverNote { body, .. } => Some((body, "cover note", "--body")),
            PrCommands::Label { labels, .. } => {
                return complete_labels(labels, cli.interactive, input);
            }
            _ => None,
        },
        _ => None,
    };
    if let Some((value, prompt, flag)) = field {
        if value.is_none() {
            if !cli.interactive {
                bail!("provide `{flag}`, or use `--interactive` to enter it");
            }
            *value = Some(input(
                PromptInputParms::default()
                    .with_prompt(prompt)
                    .with_flag_name(flag),
            )?);
        }
        if value.as_ref().is_none_or(|value| value.trim().is_empty()) {
            bail!("{prompt} cannot be empty; provide `{flag}` or enter it at the prompt");
        }
    }
    Ok(())
}

fn complete_labels(
    labels: &mut Vec<String>,
    interactive: bool,
    mut input: impl FnMut(PromptInputParms) -> Result<String>,
) -> Result<()> {
    if labels.is_empty() {
        if !interactive {
            bail!("provide `--label`, or use `--interactive` to enter labels");
        }
        *labels = input(
            PromptInputParms::default()
                .with_prompt("labels (comma-separated)")
                .with_flag_name("--label"),
        )?
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect();
    }
    if labels.is_empty() || labels.iter().any(|label| label.trim().is_empty()) {
        bail!("enter at least one non-empty label, or provide `--label`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn commands() -> Vec<(Vec<&'static str>, &'static str)> {
        let mut commands = vec![(vec!["account", "create"], "--name")];
        for kind in ["issue", "pr"] {
            for (action, flag) in [
                ("comment", "--body"),
                ("set-subject", "--subject"),
                ("set-cover-note", "--body"),
                ("label", "--label"),
            ] {
                commands.push((vec![kind, action, "deadbeef"], flag));
            }
        }
        commands
    }

    #[test]
    fn required_fields_prompt_only_when_missing_and_interactive() -> Result<()> {
        for (args, flag) in commands() {
            let mut argv = vec!["ngit"];
            argv.extend(args);
            let mut noninteractive = Cli::try_parse_from(&argv)?;
            assert!(
                complete(&mut noninteractive, |_| panic!(
                    "non-interactive input must not prompt"
                ))
                .is_err()
            );
            argv.push("--interactive");
            let mut cli = Cli::try_parse_from(&argv)?;
            let mut calls = 0;
            complete(&mut cli, |params| {
                calls += 1;
                assert_eq!(params.flag_name.as_deref(), Some(flag));
                Ok("entered".into())
            })?;
            assert_eq!(calls, 1);
            // The resolved field is retained if completion is called again.
            complete(&mut cli, |_| panic!("resolved values must not prompt"))?;
            for interactive in [true, false] {
                let mut explicit = argv.clone();
                if !interactive {
                    explicit.pop();
                }
                explicit.extend([flag, "supplied"]);
                let mut cli = Cli::try_parse_from(explicit)?;
                complete(&mut cli, |_| panic!("explicit values must not prompt"))?;
            }
        }
        Ok(())
    }

    #[test]
    fn blank_and_cancelled_input_abort_completion() -> Result<()> {
        for (args, flag) in commands() {
            let mut argv = vec!["ngit", "--interactive"];
            argv.extend(args);
            for answer in [Ok("  ".into()), Err(anyhow::anyhow!("cancelled"))] {
                let mut cli = Cli::try_parse_from(&argv)?;
                let mut answer = Some(answer);
                assert!(complete(&mut cli, |_| answer.take().unwrap()).is_err());
                assert!(
                    answer.is_none(),
                    "a leading --interactive must enable the prompt"
                );
            }
            argv.extend([flag, "  "]);
            let mut cli = Cli::try_parse_from(argv)?;
            assert!(complete(&mut cli, |_| panic!("explicit blanks must not prompt")).is_err());
        }
        let mut labels = vec![];
        complete_labels(&mut labels, true, |_| Ok("bug, help wanted, ".into()))?;
        assert_eq!(labels, ["bug", "help wanted"]);
        Ok(())
    }
}
