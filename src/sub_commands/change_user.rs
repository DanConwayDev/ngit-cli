

use clap::Args;
use dialoguer::{Select, theme::ColorfulTheme, Confirm, Password};
use nostr::{Keys, prelude::{FromSkStr}};

use crate::{config::{load_config, save_conifg}};

#[derive(Args)]
pub struct ChangeUserSubCommand {
}

pub fn change_user(_sub_command_args: &ChangeUserSubCommand) {

    let mut cfg = load_config();

    if cfg.private_key.is_some() {
        if !Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("overwrite existing?")
        .default(false)
        .interact()
        .unwrap() {
            return;
        }
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .items(&vec!["enter existing private key", "generate new keys"])
        .default(0)
        .with_prompt("no keys are stored")
        .interact().unwrap();

    let key = match selection {
        0 => {
            let mut prompt = "secret key (nsec, hex, etc)";
            loop {
                let pk: String = Password::with_theme(&ColorfulTheme::default())
                    .with_prompt(prompt)
                        .interact()
                        .unwrap();
                match Keys::from_sk_str(&pk) {
                    Ok(key) => { break key; },
                    Err(_e) => { prompt = "error interpeting secret key. try again with nsec, hex, etc"; },
                }
            }
        }
        _ => Keys::generate(),
    };
    cfg.private_key = Some(key.secret_key().unwrap());

    if cfg.default_admin_group_event_serialized.is_some() {
        if !Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("remove default admin group? If not permissions on new repositories can only be changed by the previous user.")
        .default(true)
        .interact()
        .unwrap() {
            cfg.default_admin_group_event_serialized = None;
        }
    }

    save_conifg(&cfg);
    println!("private key updated")
}
