//! Plain, append-only Blossom progress for non-interactive stderr.
//!
//! indicatif draws nothing when stderr is not a terminal (log files, CI,
//! scripts, coding agents) and the bar renderer is hidden entirely for
//! `--json`. Without this log, a long upload is silent from the signer prompt
//! until the final replication summary and is indistinguishable from a stall.

use std::{
    collections::HashMap,
    io::Write,
    sync::Mutex,
    time::{Duration, Instant},
};

use indicatif::HumanBytes;
use ngit::blossom::{BlossomProgressEvent, BlossomServerStatus};
use reqwest::Url;

/// Report bytes sent at least every this many percent of the body.
const PERCENT_STEP: u64 = 10;
/// Report bytes sent at least this often while a body is still transferring.
const REPORT_INTERVAL: Duration = Duration::from_secs(15);

struct PlainUpload {
    total_bytes: u64,
    sent: u64,
    next_percent: u64,
    last_report: Instant,
}

pub(super) struct BlossomProgressLines {
    out: Mutex<Box<dyn Write + Send>>,
    uploads: Mutex<HashMap<(String, String), PlainUpload>>,
    report_interval: Duration,
}

impl BlossomProgressLines {
    pub(super) fn stderr() -> Self {
        Self::with_writer(Box::new(std::io::stderr()), REPORT_INTERVAL)
    }

    pub(super) fn with_writer(out: Box<dyn Write + Send>, report_interval: Duration) -> Self {
        Self {
            out: Mutex::new(out),
            uploads: Mutex::new(HashMap::new()),
            report_interval,
        }
    }

    fn line(&self, message: &str) {
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Progress is best effort: a closed stderr must not fail publication.
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    fn placement_line(&self, filename: &str, server: &Url, message: &str) {
        self.line(&format!(
            "Blossom: {filename} -> {}: {message}",
            server_label(server)
        ));
    }

    /// `batch_scope` is the batch numbering after any outer sequential-file
    /// scope has been applied; it is used only for batch start/finish lines.
    pub(super) fn update(&self, event: &BlossomProgressEvent, batch_scope: (usize, usize)) {
        match event {
            BlossomProgressEvent::PresenceChecksStarted { blobs, servers, .. } => {
                self.line(&format!(
                    "Blossom: checking existing copies of {blobs} blob(s) on {} server(s)",
                    servers.len()
                ));
            }
            // Per-check results are summarised by PresenceChecksFinished, and
            // the signer reports its own authorization prompt.
            BlossomProgressEvent::PresenceCheckFinished { .. }
            | BlossomProgressEvent::AuthorizationStarted { .. } => {}
            BlossomProgressEvent::PresenceChecksFinished { missing } => {
                self.line(&format!(
                    "Blossom: existing copies checked; {missing} upload(s) needed"
                ));
            }
            BlossomProgressEvent::UploadBatchStarted {
                blobs,
                filenames,
                placements,
                bytes,
                ..
            } => {
                let (batch, batches) = batch_scope;
                let subject = if *blobs == 1 {
                    format!(
                        "file {batch}/{batches}: {}",
                        filenames.first().map_or("1 file", String::as_str)
                    )
                } else {
                    format!("group {batch}/{batches}: {blobs} files")
                };
                self.line(&format!(
                    "Blossom upload {subject} ({}); {placements} upload(s) to perform",
                    HumanBytes(*bytes)
                ));
            }
            BlossomProgressEvent::UploadRequestStarted {
                filename,
                server,
                attempt,
                max_attempts,
                total_bytes,
                ..
            } => self.upload_started(filename, server, *attempt, *max_attempts, *total_bytes),
            BlossomProgressEvent::UploadedBytes {
                filename,
                server,
                bytes,
                ..
            } => self.uploaded_bytes(filename, server, *bytes),
            BlossomProgressEvent::UploadBodyFinished {
                filename, server, ..
            } => {
                let sent = self
                    .uploads
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&(filename.clone(), server.to_string()))
                    .map_or(0, |upload| upload.sent);
                self.placement_line(
                    filename,
                    server,
                    &format!("{} sent; awaiting server response", HumanBytes(sent)),
                );
            }
            BlossomProgressEvent::VerificationStarted {
                filename, server, ..
            } => self.placement_line(filename, server, "verifying stored blob"),
            BlossomProgressEvent::RetryScheduled {
                filename,
                server,
                next_attempt,
                max_attempts,
                ..
            } => self.placement_line(
                filename,
                server,
                &format!("waiting to retry (attempt {next_attempt}/{max_attempts})"),
            ),
            BlossomProgressEvent::PlacementFinished {
                filename,
                server,
                status,
                message,
            } => self.placement_finished(filename, server, *status, message.as_deref()),
            BlossomProgressEvent::UploadBatchFinished { .. } => {
                let (batch, batches) = batch_scope;
                self.line(&format!("Blossom upload {batch}/{batches} finished"));
            }
        }
    }

    fn upload_started(
        &self,
        filename: &str,
        server: &Url,
        attempt: usize,
        max_attempts: usize,
        total_bytes: u64,
    ) {
        self.uploads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (filename.to_owned(), server.to_string()),
                PlainUpload {
                    total_bytes,
                    sent: 0,
                    next_percent: PERCENT_STEP,
                    last_report: Instant::now(),
                },
            );
        let attempt = if attempt > 1 {
            format!(" (attempt {attempt}/{max_attempts})")
        } else {
            String::new()
        };
        self.placement_line(
            filename,
            server,
            &format!("uploading {}{attempt}", HumanBytes(total_bytes)),
        );
    }

    fn placement_finished(
        &self,
        filename: &str,
        server: &Url,
        status: BlossomServerStatus,
        message: Option<&str>,
    ) {
        self.uploads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(filename.to_owned(), server.to_string()));
        let detail = message.map_or_else(String::new, |message| format!(": {message}"));
        self.placement_line(
            filename,
            server,
            &format!("{}{detail}", finished_label(status)),
        );
    }

    fn uploaded_bytes(&self, filename: &str, server: &Url, bytes: u64) {
        let report = {
            let mut uploads = self
                .uploads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(upload) = uploads.get_mut(&(filename.to_owned(), server.to_string())) else {
                return;
            };
            upload.sent = upload.sent.saturating_add(bytes);
            let percent = upload
                .sent
                .saturating_mul(100)
                .checked_div(upload.total_bytes)
                .unwrap_or(100)
                .min(100);
            let crossed_step = percent >= upload.next_percent;
            let interval_elapsed = upload.last_report.elapsed() >= self.report_interval;
            // The body-finished line reports completion, so skip a 100% line.
            if percent < 100 && (crossed_step || interval_elapsed) {
                upload.next_percent = (percent / PERCENT_STEP + 1) * PERCENT_STEP;
                upload.last_report = Instant::now();
                Some(format!(
                    "{}/{} sent ({percent}%)",
                    HumanBytes(upload.sent),
                    HumanBytes(upload.total_bytes)
                ))
            } else {
                None
            }
        };
        if let Some(report) = report {
            self.placement_line(filename, server, &report);
        }
    }
}

fn server_label(server: &Url) -> &str {
    server
        .as_str()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
}

fn finished_label(status: BlossomServerStatus) -> &'static str {
    match status {
        BlossomServerStatus::Stored => "uploaded and verified",
        BlossomServerStatus::AlreadyPresent => "already stored",
        BlossomServerStatus::Failed => "failed",
        BlossomServerStatus::Unknown => "unavailable",
        BlossomServerStatus::NotAttempted => "not attempted",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn lines_with_interval(interval: Duration) -> (BlossomProgressLines, Captured) {
        let captured = Captured::default();
        (
            BlossomProgressLines::with_writer(Box::new(captured.clone()), interval),
            captured,
        )
    }

    fn start(lines: &BlossomProgressLines, server: &Url, total_bytes: u64) {
        lines.update(
            &BlossomProgressEvent::UploadRequestStarted {
                batch: 1,
                batches: 1,
                filename: "app.tar.gz".to_owned(),
                server: server.clone(),
                attempt: 1,
                max_attempts: 3,
                total_bytes,
                additional_bytes: 0,
            },
            (1, 1),
        );
    }

    fn send(lines: &BlossomProgressLines, server: &Url, bytes: u64) {
        lines.update(
            &BlossomProgressEvent::UploadedBytes {
                batch: 1,
                filename: "app.tar.gz".to_owned(),
                server: server.clone(),
                bytes,
            },
            (1, 1),
        );
    }

    #[test]
    fn reports_each_placement_from_start_to_finish() {
        let (lines, captured) = lines_with_interval(Duration::from_secs(3600));
        let server = Url::parse("https://blossom.example/").unwrap();
        lines.update(
            &BlossomProgressEvent::UploadBatchStarted {
                batch: 1,
                batches: 1,
                blobs: 1,
                filenames: vec!["app.tar.gz".to_owned()],
                placements: 1,
                confirmed: 0,
                unavailable: 0,
                bytes: 1000,
            },
            (2, 3),
        );
        start(&lines, &server, 1000);
        send(&lines, &server, 1000);
        lines.update(
            &BlossomProgressEvent::UploadBodyFinished {
                batch: 1,
                batches: 1,
                filename: "app.tar.gz".to_owned(),
                server: server.clone(),
                attempt: 1,
                max_attempts: 3,
                idle_timeout_secs: 30,
            },
            (1, 1),
        );
        lines.update(
            &BlossomProgressEvent::PlacementFinished {
                filename: "app.tar.gz".to_owned(),
                server: server.clone(),
                status: BlossomServerStatus::Failed,
                message: Some("HTTP 500".to_owned()),
            },
            (1, 1),
        );

        let output = captured.lines();
        assert_eq!(output.len(), 4, "{output:#?}");
        assert!(output[0].contains("file 2/3: app.tar.gz"));
        assert!(output[1].contains("app.tar.gz -> blossom.example: uploading"));
        assert!(output[2].contains("sent; awaiting server response"));
        assert!(output[3].contains("failed: HTTP 500"));
    }

    #[test]
    fn rate_limits_bytes_sent_lines_to_percentage_steps() {
        let (lines, captured) = lines_with_interval(Duration::from_secs(3600));
        let server = Url::parse("https://blossom.example/").unwrap();
        start(&lines, &server, 1000);
        for _ in 0..99 {
            send(&lines, &server, 10);
        }

        let progress = captured
            .lines()
            .into_iter()
            .filter(|line| line.contains("%)"))
            .collect::<Vec<_>>();
        assert_eq!(progress.len(), 9, "{progress:#?}");
        assert!(progress[0].contains("(10%)"));
        assert!(progress[8].contains("(90%)"));
    }

    #[test]
    fn reports_slow_uploads_on_an_interval() {
        let (lines, captured) = lines_with_interval(Duration::ZERO);
        let server = Url::parse("https://blossom.example/").unwrap();
        start(&lines, &server, 1000);
        send(&lines, &server, 1);
        send(&lines, &server, 1);

        let output = captured.lines();
        assert_eq!(output.len(), 3, "{output:#?}");
        assert!(output[2].contains("2 B/1000 B sent (0%)"));
    }
}
