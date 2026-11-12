use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};

static JSON_STREAM: AtomicBool = AtomicBool::new(false);
pub fn start_stream() {
    JSON_STREAM.store(true, Ordering::Relaxed);
}
pub fn is_stream() -> bool {
    JSON_STREAM.load(Ordering::Relaxed)
}

pub async fn write_json_line(value: &serde_json::Value) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&bytes).await?;
    stdout.flush().await?;
    Ok(())
}

use anyhow::Result;
pub use ngit::output_mode::{is_json, set_json_mode};
use nostr::prelude::{EventId, RelayUrl, ToBech32, nip19::Nip19Event};
use serde::Serialize;

static JSON_OUTPUT: LazyLock<Mutex<Option<serde_json::Value>>> = LazyLock::new(|| Mutex::new(None));

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
        "command_status": "ok",
        "action": action,
        "entity": entity,
        "id": event_id_to_nevent(event_id, relay),
    }));
}

pub fn finish_success() {
    if is_stream() {
        return;
    }
    let value = JSON_OUTPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| serde_json::json!({ "command_status": "ok" }));
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    std::println!("{rendered}");
}

/// Emit the stored document, then exit with `code`.
///
/// For a command whose refusal is part of its output rather than a failure to
/// produce it — `ngit ci status --require-ci-trust` — `main`'s error path
/// would replace the document with `{"command_status":"error"}` and lose the
/// runs that explain the refusal.
pub fn finish_and_exit(code: i32) -> ! {
    if is_json() {
        finish_success();
    }
    std::process::exit(code)
}

/// A shared error shape for failed commands and partial informational results.
pub fn error_document(error: &anyhow::Error) -> serde_json::Value {
    let mut value = serde_json::json!({"command_status": "error", "error": format!("{error:#}")});
    if let Some(lookup) = error.downcast_ref::<ngit::repo_lookup::RepositoryLookupError>() {
        value["category"] = lookup.category.into();
        value["details"] = lookup.details.clone();
    }
    value
}

pub fn finish_error(error: &anyhow::Error) {
    if is_stream() {
        ngit::reporting::diagnostic(
            "error",
            "stream_stopped",
            &format!("{error:#}"),
            serde_json::json!({}),
        );
        return;
    }
    let mut value = JSON_OUTPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| error_document(error));
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
    if let Some(lookup) = error.downcast_ref::<ngit::repo_lookup::RepositoryLookupError>() {
        if let Some(document) = value.as_object_mut() {
            document.insert("category".to_string(), lookup.category.into());
            document.insert("details".to_string(), lookup.details.clone());
        }
    }
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    std::println!("{rendered}");
}

// Human result summaries are redundant once the command has a JSON document.
// Earlier operational messages remain visible as structured diagnostics.
pub fn human_message(message: &str) {
    let has_result = JSON_OUTPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    if !has_result {
        ngit::reporting::message("info", "command_message", message);
    }
}

// Keep human-facing command output visible in JSON mode without allowing it to
// precede or follow the final machine-readable document on stdout. The remote
// helper module is declared before these macros, so its protocol stdout is not
// affected.
macro_rules! println {
    () => {{
        if crate::output::is_json() {
            // JSON has no blank presentation lines.
        } else {
            std::println!();
        }
    }};
    ($($arg:tt)*) => {{
        if crate::output::is_json() {
            crate::output::human_message(&format!($($arg)*));
        } else {
            std::println!($($arg)*);
        }
    }};
}

macro_rules! print {
    ($($arg:tt)*) => {{
        if crate::output::is_json() {
            crate::output::human_message(&format!($($arg)*));
        } else {
            std::print!($($arg)*);
        }
    }};
}

// Modules declared before output (notably the Git remote helper) retain their
// protocol-specific output. This fallback is for command diagnostics, not UI.
macro_rules! eprintln {
    () => {{ if !crate::output::is_json() { std::eprintln!(); } }};
    ($($arg:tt)*) => {{
        ngit::reporting::message("info", "command_diagnostic", &format!($($arg)*));
    }};
}
