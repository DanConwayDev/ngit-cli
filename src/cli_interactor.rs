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
}
impl InteractorPrompt for Interactor {
    fn input(&self, parms: PromptInputParms) -> Result<String> {
        let input: String = Input::with_theme(&self.theme)
            .with_prompt(parms.prompt)
            .interact_text()?;
        Ok(input)
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
        dialoguer::Select::with_theme(&self.theme)
            .with_prompt(parms.prompt)
            .report(parms.report)
            .items(&parms.choices)
            .interact()
            .context("failed to get choice")
    }
}

#[derive(Default)]
pub struct PromptInputParms {
    pub prompt: String,
}

impl PromptInputParms {
    pub fn with_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.prompt = prompt.into();
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
}
