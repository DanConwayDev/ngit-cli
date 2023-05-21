use confy::ConfyError;
use dialoguer::{theme::ColorfulTheme, Input, MultiSelect, Confirm};
use nostr::{EventId, prelude::{Nip19Event, FromBech32}};

use crate::config::{MyConfig, save_conifg};
/// Renders a dialoguer multi select prompt with a free-form option
pub fn multi_select_with_add(
    proposed:Vec<String>,
    selected:Vec<bool>,
    prompt: &str,
    add_prompt: &str,
) -> Vec<String> {
    // add option with add_prompt
    let mut options:Vec<String> = proposed.clone();
    options.push(add_prompt.to_string());
    let mut options_selected = selected.clone();
    options_selected.push(false);
    // present options
    let chosen : Vec<usize> = MultiSelect::new()
        .with_prompt(prompt)
        .items(&options)
        .defaults(&options_selected)
        .report(false)
        .interact()
        .unwrap();
    // reduce options list
    let mut new_proposed: Vec<String> = [].to_vec();
    for (i, _el) in proposed.iter().enumerate() {
        if chosen.contains(&i) {
            new_proposed.push(proposed[i].clone())
        }
    }
    let mut new_selected: Vec<bool> = vec![true;new_proposed.len()];
    // if add_prompt selected
    let last = chosen.last();
    if last == None || *last.unwrap() == options.len() - 1 {
        // get user to input new item
        let new_relay: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt(add_prompt)
            .report(false)
            .interact_text()
            .unwrap();
        // prepare new proposed options
        // if new item is not blank add it as a selected option
        if new_relay.len() > 0 {
            new_proposed.push(new_relay);
            new_selected.push(true);
        }
        // re run selection
        return multi_select_with_add(
            new_proposed,
            new_selected,
            prompt,
            add_prompt,
        )
    }
    else {
        let mut items: Vec<String> = [].to_vec();
        for i in chosen {
            items.push(options[i].clone());
        }
        println!("{}: {:?}",prompt,items);
        return items;
    }
}

pub fn select_relays(cfg:&mut MyConfig, selected_defaults:&Vec<String>) -> Result<Vec<String>,ConfyError> {
    // set default relays (selected by default)
    let default_relays =
        if selected_defaults.is_empty() { cfg.default_relays.clone() }
        else { selected_defaults.clone() };
    // set full proposed list
    let mut proposed_relays = default_relays.clone();
    // add config defaults to proposed unless duplicate
    for s in &cfg.default_relays {
        if !(proposed_relays.iter().any(|df| s.eq(df))) {
            proposed_relays.push(s.clone());
        }
    }
    // add example options to proposed list unless duplicate
    for s in vec![
        String::from("wss://relay.damus.io"),
        String::from("wss://nostr.wine"),
        String::from("wss://nos.lol"),
    ] {
        if !(proposed_relays.iter().any(|df| s.eq(df))) {
            proposed_relays.push(s.clone());
        }
    }
    // select only cli attribute relays or thie first in the proposed list
    // this does the same thing but which is more idiumatic?
    // let mut selected: Vec<bool> = vec![];
    // for i in 0..proposed_relays.len() {
    //     selected.push(i < relays.len());
    // }
    let selected: Vec<bool> = proposed_relays
        .iter()
        .enumerate()
        .map(|r| r.0 < default_relays.len() ).collect();

    // get user relay selection 
    let relay_selection: Vec<String> = crate::cli_helpers::multi_select_with_add(
        proposed_relays,
        selected,
        "Relays",
        "Other Relay"
    );

    // offer to save as default config
    if relay_selection.ne(&cfg.default_relays) {
        if Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Save relays as ngit default?")
        .default(true)
        .interact()
        .unwrap() {
            cfg.default_relays = relay_selection.clone();
            save_conifg(&cfg);
        }
    }
    Ok(relay_selection)
}

pub fn valid_event_id_from_input(
    proposed_event_id: Option<String>,
    prompt:&String,
) -> EventId {
    let mut string_param = proposed_event_id.clone();
    loop {
        string_param = match string_param {
            None => {
                let response: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt(prompt.clone())
                    .report(true)
                    .interact_text()
                    .unwrap();
                Some(response)
            }
            Some(ref s) => { Some(s.clone()) },
        };

        let _valid_id = match Nip19Event::from_bech32(&string_param.clone().unwrap()) {
            Ok(n19) => { break n19.event_id }
            Err(_) => {
                match EventId::from_bech32(&string_param.clone().unwrap()) {
                    Ok(id) => { break id }
                    Err(_) => {
                        match EventId::from_hex(&string_param.clone().unwrap()) {
                            Ok(id) => { break id }
                            Err(_) => {
                                println!("not a valid nevent, note or hex string. try again.");
                                string_param = None;
                                continue;
                            }
                        }
                    }
                }
            }
        };
    }
}