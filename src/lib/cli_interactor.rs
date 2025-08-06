use anyhow::{Context, Result};
use dialoguer::{Confirm, Input, Password, theme::ColorfulTheme};
use indicatif::TermLike;
#[cfg(test)]
use mockall::*;

#[derive(Default)]
pub struct Interactor {
    theme: ColorfulTheme,
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
        let confirm: bool = Confirm::with_theme(&self.theme)
            .with_prompt(params.prompt)
            .default(params.default)
            .interact()?;
        Ok(confirm)
    }
    fn choice(&self, parms: PromptChoiceParms) -> Result<usize> {
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

pub struct PromptInputParms {
    pub prompt: String,
    pub default: String,
    pub report: bool,
    pub optional: bool,
}

impl Default for PromptInputParms {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            default: String::new(),
            optional: false,
            report: true,
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
        current_defaults.push(current_choices.len() == 1); // 'add another' should not be selected by default

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
