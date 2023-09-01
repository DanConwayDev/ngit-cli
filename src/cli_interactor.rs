use anyhow::{bail, Result};
use dialoguer::{theme::ColorfulTheme, Input};
#[cfg(test)]
use mockall::*;

#[derive(Default)]
pub struct Interactor {
    theme: ColorfulTheme,
}

#[cfg_attr(test, automock)]
pub trait InteractorPrompt {
    fn input(&self, parms: PromptInputParms) -> Result<String>;
}
impl InteractorPrompt for Interactor {
    fn input(&self, parms: PromptInputParms) -> Result<String> {
        let input: String = Input::with_theme(&self.theme)
            .with_prompt(parms.prompt)
            .interact_text()?;
        Ok(input)
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
