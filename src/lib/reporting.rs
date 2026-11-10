//! Compact machine progress, independent of terminal rendering.
use std::{
    io::Write,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};

static STDERR: LazyLock<Arc<Reporter>> =
    LazyLock::new(|| Arc::new(Reporter::new(Box::new(std::io::stderr()))));

/// A writer and sampling budget shared by concurrent operations.
struct Reporter {
    writer: Mutex<Box<dyn Write + Send>>,
    next: AtomicU64,
    last_sample: Mutex<Option<Instant>>,
}

impl Reporter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
            next: AtomicU64::new(1),
            last_sample: Mutex::new(None),
        }
    }

    fn emit(&self, record: &Value) {
        let Ok(mut bytes) = serde_json::to_vec(record) else {
            return;
        };
        bytes.push(b'\n');
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Observability must not turn a successful publication into a failure.
        let _ = writer.write_all(&bytes);
        let _ = writer.flush();
    }

    fn sample_at(&self, now: Instant) -> bool {
        let mut last = self
            .last_sample
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(10)) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// One invocation-local operation. Metadata is sent only at the start.
/// No payloads, credentials or arbitrary command arguments belong in records.
pub struct Progress {
    reporter: Arc<Reporter>,
    id: String,
    enabled: bool,
    last: Option<Value>,
    finished: bool,
    started: Instant,
}

impl Progress {
    pub fn new(operation: &str, details: Value) -> Self {
        Self::with_reporter(
            STDERR.clone(),
            operation,
            details,
            crate::output_mode::is_json() && !crate::output_mode::is_quiet(),
        )
    }

    fn with_reporter(
        reporter: Arc<Reporter>,
        operation: &str,
        details: Value,
        enabled: bool,
    ) -> Self {
        let id = format!("p{}", reporter.next.fetch_add(1, Ordering::Relaxed));
        if enabled {
            reporter.emit(&json!({"type":"progress", "schema_version":1,
                "id":id, "operation":operation, "state":"started", "details":details}));
        }
        Self {
            reporter,
            id,
            enabled,
            last: None,
            finished: false,
            started: Instant::now(),
        }
    }

    /// Emit semantic changes promptly; repeated identical changes are
    /// suppressed.
    pub fn update(&mut self, state: &str, details: Value) {
        let record = json!({"type":"progress", "id":self.id, "state":state, "details":details});
        if self.enabled && self.last.as_ref() != Some(&record) {
            self.reporter.emit(&record);
            self.last = Some(record);
        }
    }

    /// Routine aggregate counters share one ten-second budget across
    /// operations.
    pub fn sample(&mut self, details: Value) {
        if self.enabled
            && self.started.elapsed() >= Duration::from_secs(10)
            && self.reporter.sample_at(Instant::now())
        {
            self.update("progress", details);
        }
    }

    pub fn finish(&mut self, state: &str, details: Value) {
        self.update(state, details);
        self.finished = true;
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if !self.finished {
            self.update("interrupted", json!({}));
        }
    }
}

/// Diagnostics remain visible in quiet mode. Callers select stable codes.
pub fn diagnostic(level: &str, code: &str, message: &str, details: Value) {
    STDERR.emit(&json!({"type":"diagnostic", "schema_version":1,
        "level":level, "code":code, "message":message, "details":details}));
}

/// Required interaction data must be emitted even with --quiet.
pub fn action_required(action: &str, details: Value) {
    STDERR.emit(&json!({"type":"action_required", "schema_version":1,
        "action":action, "details":details}));
}

/// Transitional diagnostic boundary for messages without structured domain
/// data. Progress renderers should use `Progress` instead of serializing UI
/// text.
pub fn message(level: &str, code: &str, message: &str) {
    if crate::output_mode::is_json() {
        let message = console::strip_ansi_codes(message);
        if !message.trim().is_empty() {
            diagnostic(level, code, &message, json!({}));
        }
    } else {
        std::eprintln!("{message}");
    }
}

#[macro_export]
macro_rules! report_message {
    ($level:expr, $code:expr, $($arg:tt)*) => {
        $crate::reporting::message($level, $code, &format!($($arg)*))
    };
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    pub(crate) fn progress(operation: &str) -> (Progress, Arc<Mutex<Vec<u8>>>) {
        let capture = Capture::default();
        let reporter = Arc::new(Reporter::new(Box::new(capture.clone())));
        (
            Progress::with_reporter(reporter, operation, json!({}), true),
            capture.0,
        )
    }
    pub(crate) fn records(bytes: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
        String::from_utf8(bytes.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    #[test]
    fn concurrent_records_remain_complete_and_unique() {
        let capture = Capture::default();
        let reporter = Arc::new(Reporter::new(Box::new(capture.clone())));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let reporter = reporter.clone();
                scope.spawn(move || {
                    let mut progress = Progress::with_reporter(reporter, "test", json!({}), true);
                    progress.finish("completed", json!({}));
                });
            }
        });
        let records = records(&capture.0);
        assert_eq!(records.len(), 16);
        let ids: std::collections::HashSet<_> = records
            .iter()
            .filter(|r| r["state"] == "started")
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 8);
    }
    #[test]
    fn broken_observer_does_not_fail_the_operation() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let mut op = Progress::with_reporter(
            Arc::new(Reporter::new(Box::new(Broken))),
            "test",
            json!({}),
            true,
        );
        op.finish("completed", json!({}));
    }
    #[test]
    fn records_are_compact_deduplicated_and_metadata_is_not_repeated() {
        let capture = Capture::default();
        let reporter = Arc::new(Reporter::new(Box::new(capture.clone())));
        let mut op = Progress::with_reporter(reporter, "test", json!({"name":"line\nname"}), true);
        op.update("waiting", json!({"count":1}));
        op.update("waiting", json!({"count":1}));
        op.finish("completed", json!({"count":2}));
        drop(op);
        let bytes = capture.0.lock().unwrap();
        let records: Vec<Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["details"]["name"], "line\nname");
        assert!(records[1].get("operation").is_none());
        assert_eq!(records[0]["id"], records[2]["id"]);
    }
    #[test]
    fn sampling_is_bounded_but_completion_is_immediate() {
        let capture = Capture::default();
        let reporter = Arc::new(Reporter::new(Box::new(capture.clone())));
        let now = Instant::now();
        assert!(reporter.sample_at(now));
        for n in 0..100 {
            assert!(!reporter.sample_at(now + Duration::from_millis(n)));
        }
        assert!(reporter.sample_at(now + Duration::from_secs(10)));
        let mut op = Progress::with_reporter(reporter, "test", json!({}), true);
        op.finish("failed", json!({"reason":"offline"}));
        assert_eq!(
            capture
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|b| **b == b'\n')
                .count(),
            2
        );
    }
    #[test]
    fn quiet_operations_emit_nothing_and_abandoned_operations_are_not_successful() {
        let capture = Capture::default();
        let reporter = Arc::new(Reporter::new(Box::new(capture.clone())));
        drop(Progress::with_reporter(
            reporter.clone(),
            "quiet",
            json!({}),
            false,
        ));
        assert!(capture.0.lock().unwrap().is_empty());
        drop(Progress::with_reporter(
            reporter,
            "cancelled",
            json!({}),
            true,
        ));
        assert!(
            String::from_utf8(capture.0.lock().unwrap().clone())
                .unwrap()
                .contains("interrupted")
        );
    }
}
