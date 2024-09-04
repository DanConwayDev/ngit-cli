use anyhow::{Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password};
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
        let mut input = Input::with_theme(&self.theme);
        input.with_prompt(parms.prompt).allow_empty(parms.optional);
        if !parms.default.is_empty() {
            input.default(parms.default);
        }
        Ok(input.interact_text()?)
    }
    fn password(&self, parms: PromptPasswordParms) -> Result<String> {
        let mut p = Password::with_theme(&self.theme);
        p.with_prompt(parms.prompt);
        if parms.confirm {
            p.with_confirmation("confirm password", "passwords didnt match...");
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
        let mut choice = dialoguer::Select::with_theme(&self.theme);
        choice
            .with_prompt(parms.prompt)
            .report(parms.report)
            .items(&parms.choices);
        if let Some(default) = parms.default {
            if std::env::var("NGITTEST").is_err() {
                choice.default(default);
            }
        }
        choice.interact().context("failed to get choice")
    }
    fn multi_choice(&self, parms: PromptMultiChoiceParms) -> Result<Vec<usize>> {
        // the colorful theme is not very clear so falling back to default
        let mut choice = dialoguer::MultiSelect::default();
        choice
            .with_prompt(parms.prompt)
            .report(parms.report)
            .items(&parms.choices);
        if let Some(defaults) = parms.defaults {
            choice.defaults(&defaults);
        }
        choice.interact().context("failed to get choice")
    }
}

#[derive(Default)]
pub struct PromptInputParms {
    pub prompt: String,
    pub default: String,
    pub optional: bool,
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
}

#[derive(Default)]
pub struct PromptPasswordParms {
    pub prompt: String,
    pub confirm: bool,
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

#[derive(Default)]
pub struct PromptChoiceParms {
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: Option<usize>,
    pub report: bool,
}

impl PromptChoiceParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self.report = true;
        self
    }

    // pub fn dont_report(mut self) -> Self {
    //     self.report = false;
    //     self
    // }
    pub fn with_choices(mut self, choices: Vec<String>) -> Self {
        self.choices = choices;
        self
    }

    pub fn with_default(mut self, index: usize) -> Self {
        self.default = Some(index);
        self
    }
}

#[derive(Default)]
pub struct PromptMultiChoiceParms {
    pub prompt: String,
    pub choices: Vec<String>,
    pub defaults: Option<Vec<bool>>,
    pub report: bool,
}

impl PromptMultiChoiceParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
        self.report = true;
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
