use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use ngit::{
    blossom::{BlossomPresenceStatus, BlossomProgress, BlossomProgressEvent, BlossomServerStatus},
    output_mode::{is_quiet, is_verbose},
};
use reqwest::Url;

const BLOSSOM_UPLOAD_ROW_TEMPLATE: &str = "     [{elapsed_precise}] {prefix} [{bar:22.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} {msg}";
const BLOSSOM_PHASE_ROW_TEMPLATE: &str = "   {spinner} [{elapsed_precise}] {prefix} — {msg}";
const BLOSSOM_FINISHED_ROW_TEMPLATE: &str = "     [{elapsed_precise}] {prefix} — {msg}";

pub(crate) struct BlossomUploadProgress {
    multi: MultiProgress,
    heading: ProgressBar,
    render_to_stderr: bool,
    draw_target_attached: AtomicBool,
    verbose: bool,
    heading_style: ProgressStyle,
    presence_style: ProgressStyle,
    presence_server_style: ProgressStyle,
    upload_style: ProgressStyle,
    phase_style: ProgressStyle,
    finished_style: ProgressStyle,
    presence: Mutex<BlossomPresenceActivity>,
    activity: Mutex<BlossomActivity>,
}

#[derive(Default)]
struct BlossomPresenceActivity {
    servers: HashMap<String, BlossomPresenceServerActivity>,
}

struct BlossomPresenceServerActivity {
    bar: ProgressBar,
    already_stored: usize,
    needs_upload: usize,
    metadata_differences: usize,
    failed: usize,
    skipped: usize,
}

#[derive(Clone, Copy)]
struct BlossomPresenceCounts {
    checked: usize,
    confirmed: usize,
    missing: usize,
    metadata_differences: usize,
    failed: usize,
    skipped: usize,
}

impl BlossomPresenceServerActivity {
    fn message(&self) -> String {
        let mut parts = Vec::new();
        for (count, label) in [
            (self.already_stored, "already stored"),
            (self.needs_upload, "need upload"),
            (self.metadata_differences, "metadata differs"),
            (self.failed, "checks failed"),
            (self.skipped, "skipped"),
        ] {
            if count != 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        if parts.is_empty() {
            "checking".to_owned()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Clone, Copy)]
enum BlossomPhase {
    Uploading,
    AwaitingResponse,
    Verifying,
    Retrying,
}

struct ActiveBlossomPlacement {
    phase: BlossomPhase,
    bar: ProgressBar,
}

#[derive(Default)]
struct BlossomActivity {
    batch: usize,
    batches: usize,
    blobs: usize,
    filenames: Vec<String>,
    planned_uploads: usize,
    completed: usize,
    confirmed: usize,
    unavailable: usize,
    active: HashMap<(String, String), ActiveBlossomPlacement>,
    finished_bars: Vec<ProgressBar>,
}

impl BlossomActivity {
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        batch: usize,
        batches: usize,
        blobs: usize,
        filenames: Vec<String>,
        planned_uploads: usize,
        confirmed: usize,
        unavailable: usize,
    ) {
        *self = Self {
            batch,
            batches,
            blobs,
            filenames,
            planned_uploads,
            completed: 0,
            confirmed,
            unavailable,
            active: HashMap::new(),
            finished_bars: Vec::new(),
        };
    }

    fn subject(&self) -> String {
        if self.blobs == 1 {
            let filename = self.filenames.first().map_or("1 file", String::as_str);
            format!(
                "Blossom upload file {}/{}: {filename}",
                self.batch, self.batches
            )
        } else {
            format!(
                "Blossom upload group {}/{}: {} files",
                self.batch, self.batches, self.blobs
            )
        }
    }

    fn finish(
        &mut self,
        filename: &str,
        server: &Url,
        status: BlossomServerStatus,
    ) -> Option<ProgressBar> {
        let placement = self
            .active
            .remove(&(filename.to_owned(), server.to_string()))?;
        self.completed = self.completed.saturating_add(1).min(self.planned_uploads);
        if matches!(
            status,
            BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent
        ) {
            self.confirmed = self.confirmed.saturating_add(1);
        } else {
            self.unavailable = self.unavailable.saturating_add(1);
        }
        let bar = placement.bar;
        Some(bar)
    }

    fn message(&self) -> String {
        let mut uploading = 0;
        let mut awaiting_response = 0;
        let mut verifying = 0;
        let mut retrying = 0;
        for placement in self.active.values() {
            match placement.phase {
                BlossomPhase::Uploading => uploading += 1,
                BlossomPhase::AwaitingResponse => awaiting_response += 1,
                BlossomPhase::Verifying => verifying += 1,
                BlossomPhase::Retrying => retrying += 1,
            }
        }
        let pending = self
            .planned_uploads
            .saturating_sub(self.completed.saturating_add(self.active.len()));
        let total = self
            .confirmed
            .saturating_add(self.unavailable)
            .saturating_add(self.planned_uploads.saturating_sub(self.completed));
        let mut phases = vec![format!("{}/{} confirmed", self.confirmed, total)];
        for (count, label) in [
            (uploading, "uploading"),
            (awaiting_response, "awaiting response"),
            (verifying, "verifying storage"),
            (retrying, "waiting to retry"),
            (pending, "pending"),
            (self.unavailable, "unavailable"),
        ] {
            if count != 0 {
                phases.push(format!("{count} {label}"));
            }
        }
        format!("{} — {}", self.subject(), phases.join("; "))
    }
}

impl BlossomUploadProgress {
    pub(crate) fn new(json_output: bool) -> Result<Arc<Self>> {
        let visible = !json_output && !is_quiet();
        let draw_target = if visible {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        Self::with_draw_target(draw_target, visible, visible && is_verbose())
    }

    fn with_draw_target(
        draw_target: ProgressDrawTarget,
        render_to_stderr: bool,
        verbose: bool,
    ) -> Result<Arc<Self>> {
        let multi = MultiProgress::with_draw_target(draw_target);
        let heading_style = ProgressStyle::with_template(" {spinner} [{elapsed_precise}] {msg}")?
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈");
        let presence_style = ProgressStyle::with_template(
            "   [{elapsed_precise}] Checking existing Blossom copies [{bar:22.cyan/blue}] {pos}/{len} {msg}",
        )?
        .progress_chars("##-");
        let presence_server_style = ProgressStyle::with_template(
            "      {prefix:28} [{bar:18.cyan/blue}] {pos}/{len} {msg}",
        )?
        .progress_chars("##-");
        let upload_style =
            ProgressStyle::with_template(BLOSSOM_UPLOAD_ROW_TEMPLATE)?.progress_chars("##-");
        let phase_style =
            ProgressStyle::with_template(BLOSSOM_PHASE_ROW_TEMPLATE)?.tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈");
        let finished_style = ProgressStyle::with_template(BLOSSOM_FINISHED_ROW_TEMPLATE)?;
        let heading = multi.add(ProgressBar::new_spinner().with_style(heading_style.clone()));
        Ok(Arc::new(Self {
            multi,
            heading,
            render_to_stderr,
            draw_target_attached: AtomicBool::new(render_to_stderr),
            verbose,
            heading_style,
            presence_style,
            presence_server_style,
            upload_style,
            phase_style,
            finished_style,
            presence: Mutex::new(BlossomPresenceActivity::default()),
            activity: Mutex::new(BlossomActivity::default()),
        }))
    }

    fn start_presence_checks(&self, blobs: usize, servers: &[Url], checks: usize) {
        self.restore_draw_target();
        self.clear_presence_bars();
        self.clear_placement_bars();
        self.heading.reset();
        self.heading.set_length(checks as u64);
        self.heading.set_position(0);
        self.heading.set_style(self.presence_style.clone());
        self.heading.set_message(format!(
            "{blobs} blob(s) across {} server(s)",
            servers.len()
        ));
        let mut presence = self
            .presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for server in servers {
            let bar = self.multi.add(ProgressBar::new(blobs as u64));
            bar.set_style(self.presence_server_style.clone());
            bar.set_prefix(blossom_server_label(server));
            bar.set_message("checking");
            presence.servers.insert(
                server.to_string(),
                BlossomPresenceServerActivity {
                    bar,
                    already_stored: 0,
                    needs_upload: 0,
                    metadata_differences: 0,
                    failed: 0,
                    skipped: 0,
                },
            );
        }
        self.heading.force_draw();
    }

    fn advance_presence_checks(
        &self,
        server: &Url,
        status: BlossomPresenceStatus,
        counts: BlossomPresenceCounts,
    ) {
        let mut presence = self
            .presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(activity) = presence.servers.get_mut(server.as_str()) {
            match status {
                BlossomPresenceStatus::AlreadyStored => activity.already_stored += 1,
                BlossomPresenceStatus::NeedsUpload => activity.needs_upload += 1,
                BlossomPresenceStatus::MetadataDiffers => activity.metadata_differences += 1,
                BlossomPresenceStatus::CheckFailed => activity.failed += 1,
                BlossomPresenceStatus::Skipped => activity.skipped += 1,
            }
            activity.bar.inc(1);
            let message = activity.message();
            if activity.bar.position() == activity.bar.length().unwrap_or_default() {
                activity
                    .bar
                    .finish_with_message(format!("{message} — done"));
            } else {
                activity.bar.set_message(message);
            }
        }
        drop(presence);
        self.heading.set_position(counts.checked as u64);
        let mut parts = vec![format!("{} already stored", counts.confirmed)];
        for (count, label) in [
            (counts.missing, "need upload"),
            (counts.metadata_differences, "metadata differs"),
            (counts.failed, "checks failed"),
            (counts.skipped, "skipped"),
        ] {
            if count != 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        self.heading.set_message(parts.join("; "));
    }

    fn finish_presence_checks(&self) {
        self.heading
            .finish_with_message("existing Blossom copy checks complete");
    }

    fn clear_presence_bars(&self) {
        let mut presence = self
            .presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for activity in presence.servers.values() {
            activity.bar.finish_and_clear();
        }
        presence.servers.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn start_upload_group(
        &self,
        batch: usize,
        batches: usize,
        blobs: usize,
        filenames: Vec<String>,
        placements: usize,
        confirmed: usize,
        unavailable: usize,
    ) {
        self.restore_draw_target();
        let message = {
            let mut activity = self
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            activity.start(
                batch,
                batches,
                blobs,
                filenames,
                placements,
                confirmed,
                unavailable,
            );
            activity.message()
        };
        self.heading.reset();
        self.heading.unset_length();
        self.heading.set_style(self.heading_style.clone());
        self.heading.set_message(message);
        self.heading.enable_steady_tick(Duration::from_millis(100));
        self.heading.force_draw();
    }

    #[allow(clippy::too_many_arguments)]
    fn start_upload_request(
        &self,
        filename: &str,
        server: &Url,
        attempt: usize,
        max_attempts: usize,
        total_bytes: u64,
    ) {
        let message = {
            let mut activity = self
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let key = (filename.to_owned(), server.to_string());
            let placement = activity.active.entry(key).or_insert_with(|| {
                let bar = self.multi.add(ProgressBar::new(total_bytes));
                ActiveBlossomPlacement {
                    phase: BlossomPhase::Uploading,
                    bar,
                }
            });
            placement.phase = BlossomPhase::Uploading;
            placement.bar.reset();
            placement.bar.set_length(total_bytes);
            placement.bar.set_position(0);
            placement.bar.set_prefix(blossom_server_label(server));
            placement.bar.set_style(self.upload_style.clone());
            placement.bar.set_message(if attempt > 1 {
                format!("uploading (attempt {attempt}/{max_attempts})")
            } else {
                "uploading".to_owned()
            });
            activity.message()
        };
        self.heading.set_message(message);
    }

    fn increment_upload(&self, filename: &str, server: &Url, bytes: u64) {
        let activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(placement) = activity
            .active
            .get(&(filename.to_owned(), server.to_string()))
        {
            placement.bar.inc(bytes);
        }
    }

    fn set_operation_phase(
        &self,
        filename: &str,
        server: &Url,
        phase: BlossomPhase,
        message: String,
    ) {
        let message = {
            let mut activity = self
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(placement) = activity
                .active
                .get_mut(&(filename.to_owned(), server.to_string()))
            {
                placement.phase = phase;
                placement.bar.set_style(self.phase_style.clone());
                placement.bar.set_message(message);
                placement.bar.enable_steady_tick(Duration::from_millis(100));
                Some(activity.message())
            } else {
                None
            }
        };
        if let Some(message) = message {
            self.heading.set_message(message);
        }
    }

    fn finish_operation(&self, filename: &str, server: &Url, status: BlossomServerStatus) -> bool {
        let heading_message = {
            let mut activity = self
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(bar) = activity.finish(filename, server, status) else {
                return false;
            };
            bar.set_style(self.finished_style.clone());
            bar.finish_with_message(blossom_finished_status_label(status));
            activity.finished_bars.push(bar);
            activity.message()
        };
        self.heading.set_message(heading_message);
        true
    }

    fn finish_upload_group(&self) {
        self.heading.finish_and_clear();
        self.clear_placement_bars();
    }

    fn clear_placement_bars(&self) {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for placement in activity.active.values() {
            placement.bar.finish_and_clear();
        }
        activity.active.clear();
        for bar in &activity.finished_bars {
            bar.finish_and_clear();
        }
        activity.finished_bars.clear();
    }

    fn prepare_for_authorization(&self) {
        // A remote signer writes its own interactive terminal UI. Detach the
        // complete MultiProgress target rather than merely clearing its bars:
        // a retained target can otherwise redraw while the signer owns the
        // terminal. UploadBatchStarted reattaches after signing completes.
        self.heading.finish_and_clear();
        self.clear_presence_bars();
        self.clear_placement_bars();
        let _ = self.multi.clear();
        self.multi.set_draw_target(ProgressDrawTarget::hidden());
        self.draw_target_attached.store(false, Ordering::Release);
    }

    fn restore_draw_target(&self) {
        if self.render_to_stderr {
            self.multi.set_draw_target(ProgressDrawTarget::stderr());
            self.draw_target_attached.store(true, Ordering::Release);
        } else {
            self.multi.set_draw_target(ProgressDrawTarget::hidden());
            self.draw_target_attached.store(false, Ordering::Release);
        }
    }

    fn upload_body_finished(
        &self,
        filename: &str,
        server: &Url,
        attempt: usize,
        max_attempts: usize,
        idle_timeout_secs: u64,
    ) {
        let sent = {
            let activity = self
                .activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            activity
                .active
                .get(&(filename.to_owned(), server.to_string()))
                .map_or(0, |placement| placement.bar.position())
        };
        self.set_operation_phase(
            filename,
            server,
            BlossomPhase::AwaitingResponse,
            blossom_timed_attempt_message(
                &format!("{} sent; awaiting server response", HumanBytes(sent)),
                attempt,
                max_attempts,
                idle_timeout_secs,
                "idle limit",
            ),
        );
    }

    fn verification_started(
        &self,
        filename: &str,
        server: &Url,
        attempt: usize,
        max_attempts: usize,
        timeout_secs: u64,
    ) {
        self.set_operation_phase(
            filename,
            server,
            BlossomPhase::Verifying,
            blossom_timed_attempt_message(
                "verifying stored blob",
                attempt,
                max_attempts,
                timeout_secs,
                "timeout",
            ),
        );
    }

    fn print_placement(
        &self,
        filename: &str,
        server: &Url,
        status: BlossomServerStatus,
        message: Option<&str>,
    ) {
        if !self.verbose {
            return;
        }
        let status = blossom_status_label(status);
        let detail = message.map_or_else(String::new, |message| format!(": {message}"));
        self.heading
            .println(format!("  {filename} -> {server}: {status}{detail}"));
    }

    fn placement_finished(
        &self,
        filename: &str,
        server: &Url,
        status: BlossomServerStatus,
        message: Option<&str>,
    ) {
        let tracked = self.finish_operation(filename, server, status);
        if !tracked || self.verbose {
            self.print_placement(filename, server, status, message);
        }
    }

    fn update_presence_progress(&self, event: &BlossomProgressEvent) {
        match event {
            BlossomProgressEvent::PresenceChecksStarted {
                blobs,
                servers,
                checks,
            } => self.start_presence_checks(*blobs, servers, *checks),
            BlossomProgressEvent::PresenceCheckFinished {
                server,
                status,
                checked,
                confirmed,
                missing,
                metadata_differences,
                failed,
                skipped,
                ..
            } => self.advance_presence_checks(
                server,
                *status,
                BlossomPresenceCounts {
                    checked: *checked,
                    confirmed: *confirmed,
                    missing: *missing,
                    metadata_differences: *metadata_differences,
                    failed: *failed,
                    skipped: *skipped,
                },
            ),
            BlossomProgressEvent::PresenceChecksFinished { .. } => {
                self.finish_presence_checks();
            }
            _ => unreachable!("only presence events are delegated here"),
        }
    }
}

fn blossom_server_label(server: &Url) -> String {
    server
        .as_str()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_owned()
}

fn blossom_finished_status_label(status: BlossomServerStatus) -> &'static str {
    match status {
        BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent => "done: confirmed",
        BlossomServerStatus::Failed => "done: failed",
        BlossomServerStatus::Unknown => "done: unavailable",
        BlossomServerStatus::NotAttempted => "done: not attempted",
    }
}

fn blossom_status_label(status: BlossomServerStatus) -> &'static str {
    match status {
        BlossomServerStatus::Stored => "stored",
        BlossomServerStatus::AlreadyPresent => "already_present",
        BlossomServerStatus::Failed => "failed",
        BlossomServerStatus::Unknown => "unknown",
        BlossomServerStatus::NotAttempted => "not_attempted",
    }
}

fn blossom_timed_attempt_message(
    action: &str,
    attempt: usize,
    max_attempts: usize,
    timeout_secs: u64,
    timeout_label: &str,
) -> String {
    if attempt > 1 {
        format!("{action} (attempt {attempt}/{max_attempts}; {timeout_secs}s {timeout_label})")
    } else {
        format!("{action} ({timeout_secs}s {timeout_label})")
    }
}

impl BlossomProgress for BlossomUploadProgress {
    fn update(&self, event: &BlossomProgressEvent) {
        match event {
            BlossomProgressEvent::PresenceChecksStarted { .. }
            | BlossomProgressEvent::PresenceCheckFinished { .. }
            | BlossomProgressEvent::PresenceChecksFinished { .. } => {
                self.update_presence_progress(event);
            }
            BlossomProgressEvent::AuthorizationStarted { .. } => self.prepare_for_authorization(),
            BlossomProgressEvent::UploadBatchStarted {
                batch,
                batches,
                blobs,
                filenames,
                placements,
                confirmed,
                unavailable,
                ..
            } => self.start_upload_group(
                *batch,
                *batches,
                *blobs,
                filenames.clone(),
                *placements,
                *confirmed,
                *unavailable,
            ),
            BlossomProgressEvent::UploadRequestStarted {
                filename,
                server,
                attempt,
                max_attempts,
                total_bytes,
                ..
            } => self.start_upload_request(filename, server, *attempt, *max_attempts, *total_bytes),
            BlossomProgressEvent::UploadedBytes {
                filename,
                server,
                bytes,
                ..
            } => self.increment_upload(filename, server, *bytes),
            BlossomProgressEvent::UploadBodyFinished {
                filename,
                server,
                attempt,
                max_attempts,
                idle_timeout_secs,
                ..
            } => self.upload_body_finished(
                filename,
                server,
                *attempt,
                *max_attempts,
                *idle_timeout_secs,
            ),
            BlossomProgressEvent::VerificationStarted {
                filename,
                server,
                attempt,
                max_attempts,
                timeout_secs,
                ..
            } => {
                self.verification_started(filename, server, *attempt, *max_attempts, *timeout_secs);
            }
            BlossomProgressEvent::RetryScheduled {
                filename,
                server,
                next_attempt,
                max_attempts,
                ..
            } => self.set_operation_phase(
                filename,
                server,
                BlossomPhase::Retrying,
                format!("waiting to retry (attempt {next_attempt}/{max_attempts})"),
            ),
            BlossomProgressEvent::PlacementFinished {
                filename,
                server,
                status,
                message,
            } => self.placement_finished(filename, server, *status, message.as_deref()),
            BlossomProgressEvent::UploadBatchFinished { .. } => self.finish_upload_group(),
        }
    }
}

impl Drop for BlossomUploadProgress {
    fn drop(&mut self) {
        self.heading.finish_and_clear();
        self.clear_presence_bars();
        self.clear_placement_bars();
        let _ = self.multi.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blossom_batch_rows_reserve_the_spinner_column() {
        let rendered_bracket_column = |template: &str| {
            template
                .replace("{spinner}", "x")
                .find('[')
                .expect("Blossom row template should render elapsed time")
        };

        assert_eq!(
            rendered_bracket_column(BLOSSOM_UPLOAD_ROW_TEMPLATE),
            rendered_bracket_column(BLOSSOM_PHASE_ROW_TEMPLATE)
        );
        assert_eq!(
            rendered_bracket_column(BLOSSOM_FINISHED_ROW_TEMPLATE),
            rendered_bracket_column(BLOSSOM_PHASE_ROW_TEMPLATE)
        );
    }

    #[test]
    fn blossom_progress_distinguishes_body_transfer_from_waiting_for_response() -> Result<()> {
        let progress = BlossomUploadProgress::new(true)?;
        let server = Url::parse("https://blossom.example/")?;
        progress.update(&BlossomProgressEvent::UploadBatchStarted {
            batch: 1,
            batches: 2,
            blobs: 1,
            filenames: vec!["release.tar.gz".to_owned()],
            placements: 1,
            confirmed: 0,
            unavailable: 0,
            bytes: 10,
        });
        progress.update(&BlossomProgressEvent::UploadRequestStarted {
            batch: 1,
            batches: 2,
            filename: "release.tar.gz".to_owned(),
            server: server.clone(),
            attempt: 1,
            max_attempts: 3,
            total_bytes: 10,
            additional_bytes: 0,
        });
        let bar = progress
            .activity
            .lock()
            .unwrap()
            .active
            .get(&("release.tar.gz".to_owned(), server.to_string()))
            .unwrap()
            .bar
            .clone();
        assert_eq!(bar.message(), "uploading");
        progress.update(&BlossomProgressEvent::UploadedBytes {
            batch: 1,
            filename: "release.tar.gz".to_owned(),
            server: server.clone(),
            bytes: 10,
        });
        progress.update(&BlossomProgressEvent::UploadBodyFinished {
            batch: 1,
            batches: 2,
            filename: "release.tar.gz".to_owned(),
            server: server.clone(),
            attempt: 1,
            max_attempts: 3,
            idle_timeout_secs: 30,
        });
        assert!(progress.heading.is_hidden());
        assert_eq!(bar.length(), Some(10));
        assert_eq!(bar.position(), 10);
        assert!(bar.message().contains("sent; awaiting server response"));
        assert!(bar.message().contains("30s idle limit"));
        assert!(!bar.message().contains("attempt 1/3"));
        assert!(progress.heading.message().contains("1 awaiting response"));

        progress.update(&BlossomProgressEvent::RetryScheduled {
            batch: 1,
            batches: 2,
            filename: "release.tar.gz".to_owned(),
            server: server.clone(),
            next_attempt: 2,
            max_attempts: 3,
        });
        progress.update(&BlossomProgressEvent::UploadRequestStarted {
            batch: 1,
            batches: 2,
            filename: "release.tar.gz".to_owned(),
            server,
            attempt: 2,
            max_attempts: 3,
            total_bytes: 10,
            additional_bytes: 10,
        });

        assert_eq!(bar.length(), Some(10));
        assert_eq!(bar.position(), 0);
        assert!(bar.message().contains("uploading (attempt 2/3)"));
        assert!(progress.heading.message().contains("1 uploading"));
        Ok(())
    }

    #[test]
    fn blossom_progress_stops_rendering_while_authorization_is_signed() -> Result<()> {
        let progress =
            BlossomUploadProgress::with_draw_target(ProgressDrawTarget::stderr(), true, false)?;
        let first_server = Url::parse("https://one.example/")?;
        let second_server = Url::parse("https://two.example/")?;
        progress.update(&BlossomProgressEvent::PresenceChecksStarted {
            blobs: 1,
            servers: vec![first_server.clone(), second_server.clone()],
            checks: 2,
        });
        assert!(!progress.heading.is_finished());
        assert_eq!(progress.heading.length(), Some(2));
        assert_eq!(progress.heading.position(), 0);
        assert!(progress.draw_target_attached.load(Ordering::Acquire));

        progress.update(&BlossomProgressEvent::PresenceCheckFinished {
            server: first_server.clone(),
            status: BlossomPresenceStatus::NeedsUpload,
            checked: 1,
            checks: 2,
            confirmed: 0,
            missing: 1,
            metadata_differences: 0,
            failed: 0,
            skipped: 0,
        });
        assert_eq!(progress.heading.position(), 1);
        assert!(progress.heading.message().contains("1 need upload"));
        let presence = progress.presence.lock().unwrap();
        let first = &presence.servers[first_server.as_str()];
        assert_eq!(first.bar.position(), 1);
        assert!(first.bar.message().contains("1 need upload"));
        assert!(first.bar.message().contains("done"));
        assert!(first.bar.is_finished());
        drop(presence);

        progress.update(&BlossomProgressEvent::PresenceCheckFinished {
            server: second_server.clone(),
            status: BlossomPresenceStatus::MetadataDiffers,
            checked: 2,
            checks: 2,
            confirmed: 0,
            missing: 1,
            metadata_differences: 1,
            failed: 0,
            skipped: 0,
        });
        assert!(progress.heading.message().contains("1 metadata differs"));
        let presence = progress.presence.lock().unwrap();
        let second = &presence.servers[second_server.as_str()];
        assert!(second.bar.message().contains("1 metadata differs"));
        assert!(second.bar.is_finished());
        drop(presence);

        progress.update(&BlossomProgressEvent::AuthorizationStarted {
            batch: 1,
            batches: 1,
            blobs: 1,
            filenames: vec!["release.tar.gz".to_owned()],
        });
        assert!(progress.heading.is_finished());
        assert!(!progress.draw_target_attached.load(Ordering::Acquire));

        progress.update(&BlossomProgressEvent::UploadBatchStarted {
            batch: 1,
            batches: 1,
            blobs: 1,
            filenames: vec!["release.tar.gz".to_owned()],
            placements: 2,
            confirmed: 0,
            unavailable: 0,
            bytes: 20,
        });
        assert!(!progress.heading.is_finished());
        assert!(progress.draw_target_attached.load(Ordering::Acquire));

        progress.update(&BlossomProgressEvent::UploadBatchFinished {
            batch: 1,
            batches: 1,
        });
        assert!(progress.heading.is_finished());
        assert!(progress.activity.lock().unwrap().active.is_empty());
        Ok(())
    }

    #[test]
    fn blossom_progress_combines_concurrent_placement_activity() -> Result<()> {
        let progress = BlossomUploadProgress::new(true)?;
        let first_server = Url::parse("https://one.example/")?;
        let second_server = Url::parse("https://two.example/")?;
        progress.update(&BlossomProgressEvent::UploadBatchStarted {
            batch: 1,
            batches: 1,
            blobs: 2,
            filenames: vec!["ngit-grasp.tar.gz".to_owned(), "SHA256SUMS".to_owned()],
            placements: 3,
            confirmed: 0,
            unavailable: 0,
            bytes: 30,
        });
        progress.update(&BlossomProgressEvent::UploadRequestStarted {
            batch: 1,
            batches: 1,
            filename: "ngit-grasp.tar.gz".to_owned(),
            server: first_server.clone(),
            attempt: 1,
            max_attempts: 3,
            total_bytes: 10,
            additional_bytes: 0,
        });
        progress.update(&BlossomProgressEvent::UploadRequestStarted {
            batch: 1,
            batches: 1,
            filename: "SHA256SUMS".to_owned(),
            server: second_server.clone(),
            attempt: 1,
            max_attempts: 3,
            total_bytes: 20,
            additional_bytes: 0,
        });

        let uploading = progress.heading.message();
        assert!(uploading.contains("2 files"));
        assert!(uploading.contains("2 uploading"));
        assert!(uploading.contains("1 pending"));

        progress.update(&BlossomProgressEvent::VerificationStarted {
            batch: 1,
            batches: 1,
            filename: "ngit-grasp.tar.gz".to_owned(),
            server: first_server.clone(),
            attempt: 1,
            max_attempts: 3,
            timeout_secs: 15,
        });
        let mixed = progress.heading.message();
        assert!(mixed.contains("1 uploading"));
        assert!(mixed.contains("1 verifying storage"));

        let verifying_bar = progress
            .activity
            .lock()
            .unwrap()
            .active
            .get(&("ngit-grasp.tar.gz".to_owned(), first_server.to_string()))
            .unwrap()
            .bar
            .clone();
        assert!(verifying_bar.message().contains("15s timeout"));
        assert!(!verifying_bar.message().contains("attempt 1/3"));

        progress.update(&BlossomProgressEvent::VerificationStarted {
            batch: 1,
            batches: 1,
            filename: "ngit-grasp.tar.gz".to_owned(),
            server: first_server.clone(),
            attempt: 2,
            max_attempts: 3,
            timeout_secs: 15,
        });
        assert!(verifying_bar.message().contains("attempt 2/3; 15s timeout"));

        progress.update(&BlossomProgressEvent::PlacementFinished {
            filename: "ngit-grasp.tar.gz".to_owned(),
            server: first_server,
            status: BlossomServerStatus::Stored,
            message: None,
        });
        let placed = progress.heading.message();
        assert!(placed.contains("1/3 confirmed"));
        assert!(placed.contains("1 uploading"));
        let activity = progress.activity.lock().unwrap();
        assert_eq!(activity.finished_bars.len(), 1);
        assert_eq!(activity.finished_bars[0].message(), "done: confirmed");
        drop(activity);

        progress.update(&BlossomProgressEvent::UploadBatchFinished {
            batch: 1,
            batches: 1,
        });
        assert!(progress.activity.lock().unwrap().finished_bars.is_empty());
        Ok(())
    }
}
