use std::fmt;

use anyhow::{Context, Result, bail};
use console::Style;
use dialoguer::{
    Confirm, Input, Password,
    theme::{ColorfulTheme, Theme},
};
use indicatif::TermLike;
#[cfg(test)]
use mockall::*;

/// Sentinel error type indicating the error has already been printed to stderr.
///
/// When this propagates up to `main()`, it signals "already printed styled
/// output to stderr, don't double-print". This is the same pattern clap uses
/// internally.
#[derive(Debug)]
pub struct CliError;

impl fmt::Display for CliError {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Empty display — the error message was already printed to stderr
        Ok(())
    }
}

impl std::error::Error for CliError {}

/// Print a styled CLI error to stderr and return an `anyhow::Error` wrapping
/// [`CliError`].
///
/// - `message`: the main error text (printed after the red `error:` prefix)
/// - `details`: flag/description pairs shown as gray indented lines (for
///   multiple missing fields). Descriptions are aligned to the longest flag.
/// - `suggestions`: command suggestions shown in yellow
///
/// This function does NOT call `process::exit()`. It prints to stderr and
/// returns an error that the caller should propagate with `?` or `return Err`.
pub fn cli_error(message: &str, details: &[(&str, &str)], suggestions: &[&str]) -> anyhow::Error {
    let dim = Style::new().for_stderr().color256(247);

    eprint!(
        "{} {}",
        console::style("error:").for_stderr().red(),
        message
    );
    if details.is_empty() {
        eprintln!();
    } else {
        let max_flag_len = details
            .iter()
            .map(|(flag, _)| flag.len())
            .max()
            .unwrap_or(0);
        eprintln!();
        for (flag, desc) in details {
            eprintln!(
                "  {:width$}  {}",
                dim.apply_to(flag),
                dim.apply_to(desc),
                width = max_flag_len
            );
        }
    }

    if !suggestions.is_empty() {
        eprintln!();
        for cmd in suggestions {
            eprintln!(
                "{}",
                console::style(format!("    {cmd}")).for_stderr().yellow(),
            );
        }
    }

    CliError.into()
}

#[derive(Default)]
pub struct Interactor {
    theme: ColorfulTheme,
    non_interactive: bool,
}

impl Interactor {
    pub fn new(non_interactive: bool) -> Self {
        Self {
            theme: ColorfulTheme::default(),
            non_interactive,
        }
    }

    /// Returns true if running in non-interactive mode (the default).
    /// Interactive mode is only enabled when NGIT_INTERACTIVE_MODE env var is
    /// set (via -i flag).
    pub fn is_non_interactive() -> bool {
        std::env::var("NGIT_INTERACTIVE_MODE").is_err()
    }
}

#[cfg_attr(test, automock)]
pub trait InteractorPrompt {
    fn input(&self, parms: PromptInputParms) -> Result<String>;
    fn password(&self, parms: PromptPasswordParms) -> Result<String>;
    fn confirm(&self, params: PromptConfirmParms) -> Result<bool>;
    fn choice(&self, params: PromptChoiceParms) -> Result<usize>;
    fn multi_choice(&self, params: PromptMultiChoiceParms) -> Result<Vec<usize>>;
}
impl InteractorPrompt for Interactor {
    fn input(&self, parms: PromptInputParms) -> Result<String> {
        if self.non_interactive || Self::is_non_interactive() {
            if parms.optional || !parms.default.is_empty() {
                return Ok(parms.default);
            }
            let flag_hint = parms
                .flag_name
                .as_ref()
                .map(|f| format!(" (provide {} or use -i/-d)", f))
                .unwrap_or_else(|| " (use -i for interactive mode or -d for defaults)".to_string());
            bail!(
                "interactive input required but running in non-interactive mode: {}{}",
                parms.prompt,
                flag_hint
            );
        }
        let mut input = Input::with_theme(&self.theme)
            .with_prompt(parms.prompt)
            .allow_empty(parms.optional)
            .report(parms.report);
        if !parms.default.is_empty() {
            input = input.default(parms.default);
        }
        Ok(input.interact_text()?)
    }
    fn password(&self, parms: PromptPasswordParms) -> Result<String> {
        if self.non_interactive || Self::is_non_interactive() {
            bail!(
                "password input required but running in non-interactive mode: {}",
                parms.prompt
            );
        }
        let mut p = Password::with_theme(&self.theme)
            .with_prompt(parms.prompt)
            .report(parms.report);
        if parms.confirm {
            p = p.with_confirmation("confirm password", "passwords didnt match...");
        }
        let pass: String = p.interact()?;
        Ok(pass)
    }
    fn confirm(&self, params: PromptConfirmParms) -> Result<bool> {
        if self.non_interactive || Self::is_non_interactive() {
            return Ok(params.default);
        }
        let confirm: bool = Confirm::with_theme(&self.theme)
            .with_prompt(params.prompt)
            .default(params.default)
            .interact()?;
        Ok(confirm)
    }
    fn choice(&self, parms: PromptChoiceParms) -> Result<usize> {
        if self.non_interactive || Self::is_non_interactive() {
            if let Some(default) = parms.default {
                return Ok(default);
            }
            bail!(
                "interactive choice required but running in non-interactive mode: {}",
                parms.prompt
            );
        }
        let mut choice = dialoguer::Select::with_theme(&self.theme)
            .with_prompt(parms.prompt)
            .report(parms.report)
            .items(&parms.choices);
        if let Some(default) = parms.default {
            if std::env::var("NGITTEST").is_err() {
                choice = choice.default(default);
            }
        }
        choice.interact().context("failed to get choice")
    }
    fn multi_choice(&self, parms: PromptMultiChoiceParms) -> Result<Vec<usize>> {
        if self.non_interactive || Self::is_non_interactive() {
            if let Some(defaults) = &parms.defaults {
                return Ok(defaults
                    .iter()
                    .enumerate()
                    .filter(|(_, &selected)| selected)
                    .map(|(i, _)| i)
                    .collect());
            }
            return Ok(vec![]); // Empty selection if no defaults
        }
        // the colorful theme is not very clear so falling back to default
        let mut choice = dialoguer::MultiSelect::default()
            .with_prompt(parms.prompt)
            .report(parms.report)
            .items(&parms.choices);
        if let Some(defaults) = parms.defaults {
            choice = choice.defaults(&defaults);
        }
        choice.interact().context("failed to get choice")
    }
}

/// Parameters for interactive input prompts.
///
/// Supports both interactive and non-interactive modes:
/// - Interactive mode (NGIT_INTERACTIVE_MODE set): prompts user
/// - Non-interactive mode (default): returns default value or errors
///
/// The `flag_name` field improves error messages by telling users
/// which CLI flag would provide the missing value.
pub struct PromptInputParms {
    pub prompt: String,
    pub default: String,
    pub report: bool,
    pub optional: bool,
    pub flag_name: Option<String>,
}

impl Default for PromptInputParms {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            default: String::new(),
            optional: false,
            report: true,
            flag_name: None,
        }
    }
}

impl PromptInputParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self
    }
    pub fn with_default<S: Into<String>>(mut self, default: S) -> Self {
        self.default = default.into();
        self
    }
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    pub fn dont_report(mut self) -> Self {
        self.report = false;
        self
    }

    pub fn with_flag_name<S: Into<String>>(mut self, flag_name: S) -> Self {
        self.flag_name = Some(flag_name.into());
        self
    }
}

pub struct PromptPasswordParms {
    pub prompt: String,
    pub confirm: bool,
    pub report: bool,
}

impl Default for PromptPasswordParms {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            confirm: false,
            report: true,
        }
    }
}

impl PromptPasswordParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self
    }
    pub const fn with_confirm(mut self) -> Self {
        self.confirm = true;
        self
    }
    pub fn dont_report(mut self) -> Self {
        self.report = false;
        self
    }
}

#[derive(Default)]
pub struct PromptConfirmParms {
    pub prompt: String,
    pub default: bool,
}

impl PromptConfirmParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self
    }
    pub fn with_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }
}

pub struct PromptChoiceParms {
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: Option<usize>,
    pub report: bool,
}

impl Default for PromptChoiceParms {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            choices: vec![],
            default: None,
            report: true,
        }
    }
}

impl PromptChoiceParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self
    }

    pub fn dont_report(mut self) -> Self {
        self.report = false;
        self
    }

    pub fn with_choices(mut self, choices: Vec<String>) -> Self {
        self.choices = choices;
        self
    }

    pub fn with_default(mut self, index: usize) -> Self {
        self.default = Some(index);
        self
    }
}

pub struct PromptMultiChoiceParms {
    pub prompt: String,
    pub choices: Vec<String>,
    pub defaults: Option<Vec<bool>>,
    pub report: bool,
}

impl Default for PromptMultiChoiceParms {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            choices: vec![],
            defaults: None,
            report: true,
        }
    }
}

impl PromptMultiChoiceParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self
    }

    pub fn dont_report(mut self) -> Self {
        self.report = false;
        self
    }

    pub fn with_choices(mut self, choices: Vec<String>) -> Self {
        self.choices = choices;
        self
    }

    pub fn with_defaults(mut self, defaults: Vec<bool>) -> Self {
        self.defaults = Some(defaults);
        self
    }
}

pub fn multi_select_with_custom_value<F>(
    prompt: &str,
    custom_choice_prompt: &str,
    mut choices: Vec<String>,
    mut defaults: Vec<bool>,
    validate_choice: F,
) -> Result<Vec<String>>
where
    F: Fn(&str) -> Result<String>,
{
    let mut selected_choices = vec![];

    // Loop to allow users to add more choices
    loop {
        // Add 'add another' option at the end of the choices
        let mut current_choices = choices.clone();
        current_choices.push(if current_choices.is_empty() {
            "add".to_string()
        } else {
            "add another".to_string()
        });

        // Create default selections based on the provided defaults
        let mut current_defaults = defaults.clone();
        current_defaults.push(false); // 'add'/'add another' should not be selected by default

        // Prompt for selections
        let selected_indices: Vec<usize> = Interactor::default().multi_choice(
            PromptMultiChoiceParms::default()
                .with_prompt(prompt)
                .dont_report()
                .with_choices(current_choices.clone())
                .with_defaults(current_defaults),
        )?;

        // Collect selected choices
        selected_choices.clear(); // Clear previous selections to update
        for &index in &selected_indices {
            if index < choices.len() {
                // Exclude 'add another' option
                selected_choices.push(choices[index].clone());
            }
        }

        // Check if 'add another' was selected
        if selected_indices.contains(&(choices.len())) {
            // Last index is 'add another'
            let mut new_choice: String;
            loop {
                new_choice = Interactor::default().input(
                    PromptInputParms::default()
                        .with_prompt(custom_choice_prompt)
                        .dont_report()
                        .optional(),
                )?;

                if new_choice.is_empty() {
                    break;
                }
                // Validate the new choice
                match validate_choice(&new_choice) {
                    Ok(valid_choice) => {
                        new_choice = valid_choice; // Use the fixed version of the input
                        break; // Valid choice, exit the loop
                    }
                    Err(err) => {
                        // Inform the user about the validation error
                        println!("Error: {err}");
                    }
                }
            }

            // Add the new choice to the choices vector
            if !new_choice.is_empty() {
                choices.push(new_choice.clone()); // Add new choice to the end of the list
                selected_choices.push(new_choice); // Automatically select the new choice
                defaults.push(true); // Set the new choice as selected by default
            }
        } else {
            // Exit the loop if 'add another' was not selected
            break;
        }
    }

    Ok(selected_choices)
}

pub fn show_multi_input_prompt_success(label: &str, values: &[String]) {
    let values_str: Vec<&str> = values.iter().map(std::string::String::as_str).collect();
    eprintln!("{}", {
        let mut s = String::new();
        let _ = ColorfulTheme::default().format_multi_select_prompt_selection(
            &mut s,
            label,
            &values_str,
        );
        s
    });
}

#[derive(Debug, Default)]
pub struct Printer {
    printed_lines: Vec<String>,
}
impl Printer {
    pub fn println(&mut self, line: String) {
        eprintln!("{line}");
        self.printed_lines.push(line);
    }
    pub fn println_with_custom_formatting(
        &mut self,
        line: String,
        line_without_formatting: String,
    ) {
        eprintln!("{line}");
        self.printed_lines.push(line_without_formatting);
    }
    pub fn printlns(&mut self, lines: Vec<String>) {
        for line in lines {
            self.println(line);
        }
    }
    pub fn clear_all(&mut self) {
        let term = console::Term::stderr();
        let _ = term.clear_last_lines(count_lines_per_msg_vec(
            term.width(),
            &self.printed_lines,
            0,
        ));
        self.printed_lines.drain(..);
    }
}

pub fn count_lines_per_msg(width: u16, msg: &str, prefix_len: usize) -> usize {
    if width == 0 {
        return 1;
    }
    // ((msg_len+prefix) / width).ceil() implemented using Integer Arithmetic
    ((msg.chars().count() + prefix_len) + (width - 1) as usize) / width as usize
}

pub fn count_lines_per_msg_vec(width: u16, msgs: &[String], prefix_len: usize) -> usize {
    msgs.iter()
        .map(|msg| count_lines_per_msg(width, msg, prefix_len))
        .sum()
}
