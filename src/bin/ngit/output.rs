use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use nostr::prelude::{EventId, RelayUrl, ToBech32, nip19::Nip19Event};
use serde::Serialize;

static JSON_MODE: AtomicBool = AtomicBool::new(false);
static JSON_OUTPUT: LazyLock<Mutex<Option<serde_json::Value>>> = LazyLock::new(|| Mutex::new(None));

pub fn set_json_mode(enabled: bool) {
    JSON_MODE.store(enabled, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

pub fn term() -> console::Term {
    if is_json() {
        console::Term::stderr()
    } else {
        console::Term::stdout()
    }
}

pub fn set<T: Serialize>(value: T) -> Result<()> {
    set_value(serde_json::to_value(value)?);
    Ok(())
}

pub fn set_value(value: serde_json::Value) {
    *JSON_OUTPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
}

pub fn event_id_to_nevent(event_id: EventId, relay: Option<&RelayUrl>) -> String {
    Nip19Event {
        event_id,
        relays: relay.cloned().into_iter().collect(),
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_else(|_| event_id.to_hex())
}

pub fn set_event(action: &str, entity: &str, event_id: EventId, relay: Option<&RelayUrl>) {
    set_value(serde_json::json!({
        "status": "ok",
        "action": action,
        "entity": entity,
        "id": event_id_to_nevent(event_id, relay),
    }));
}

pub fn finish_success() {
    let value = JSON_OUTPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| serde_json::json!({ "status": "ok" }));
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    std::println!("{rendered}");
}

/// Emit the stored document, then exit with `code`.
///
/// For a command whose refusal is part of its output rather than a failure to
/// produce it — `ngit ci status --require-ci-trust` — `main`'s error path
/// would replace the document with `{"status":"error"}` and lose the runs that
/// explain the refusal.
pub fn finish_and_exit(code: i32) -> ! {
    if is_json() {
        finish_success();
    }
    std::process::exit(code)
}

pub fn finish_error(error: &anyhow::Error) {
    let mut value = JSON_OUTPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| {
            serde_json::json!({
                "status": "error",
                "error": format!("{error:#}"),
            })
        });
    if let Some(category) = error
        .downcast_ref::<ngit::cli_interactor::CliError>()
        .and_then(ngit::cli_interactor::CliError::category)
    {
        if let Some(document) = value.as_object_mut() {
            document.insert(
                "category".to_string(),
                serde_json::Value::String(category.to_string()),
            );
        }
    }
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    std::println!("{rendered}");
}

// Keep human-facing command output visible in JSON mode without allowing it to
// precede or follow the final machine-readable document on stdout. The remote
// helper module is declared before these macros, so its protocol stdout is not
// affected.
macro_rules! println {
    () => {{
        if crate::output::is_json() {
            std::eprintln!();
        } else {
            std::println!();
        }
    }};
    ($($arg:tt)*) => {{
        if crate::output::is_json() {
            std::eprintln!($($arg)*);
        } else {
            std::println!($($arg)*);
        }
    }};
}

macro_rules! print {
    ($($arg:tt)*) => {{
        if crate::output::is_json() {
            std::eprint!($($arg)*);
        } else {
            std::print!($($arg)*);
        }
    }};
}
