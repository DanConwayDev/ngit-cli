use std::{
    collections::{HashMap, HashSet},
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

const BLOSSOM_UPLOAD_ROW_TEMPLATE: &str = "     [{elapsed_precise}] {prefix:28} [{bar:22}] {bytes}/{total_bytes} {bytes_per_sec} {wide_msg}";
const BLOSSOM_PHASE_ROW_TEMPLATE: &str =
    "   {spinner} [{elapsed_precise}] {prefix:28} — {wide_msg}";
const BLOSSOM_FINISHED_ROW_TEMPLATE: &str = "     [{elapsed_precise}] {prefix:28} — {wide_msg}";
const BLOSSOM_SERVER_SUMMARY_ROW_TEMPLATE: &str = "  {prefix}  {wide_msg}";
const BLOSSOM_COMPACT_UPLOAD_ROW_TEMPLATE: &str =
    "    [{bar:22}] {bytes}/{total_bytes} {bytes_per_sec} {wide_msg}";
const BLOSSOM_COMPACT_PHASE_ROW_TEMPLATE: &str = "    {spinner} [{elapsed_precise}] {wide_msg}";
const DETAILED_UPLOAD_BLOB_LIMIT: usize = 3;

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
    server_summary_style: ProgressStyle,
    compact_upload_style: ProgressStyle,
    compact_phase_style: ProgressStyle,
    sequential_file: Mutex<Option<SequentialFileScope>>,
    presence: Mutex<BlossomPresenceActivity>,
    activity: Mutex<BlossomActivity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SequentialFileScope {
    index: usize,
    total: usize,
    filename: String,
}

#[derive(Default)]
struct BlossomPresenceActivity {
    servers: HashMap<String, BlossomPresenceServerActivity>,
    server_order: Vec<Url>,
    outcomes: HashMap<(String, String), BlossomServerStatus>,
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

#[derive(Clone)]
struct ActiveBlossomPlacement {
    phase: BlossomPhase,
    bar: Option<ProgressBar>,
    total_bytes: u64,
    uploaded_bytes: u64,
    message: String,
    server_label: String,
    sequence: usize,
}

struct BlossomServerUploadActivity {
    summary_bar: ProgressBar,
    focus_bar: Option<ProgressBar>,
    focus_sequence: Option<usize>,
    total: usize,
    planned_uploads: usize,
    completed: usize,
    available: usize,
    unavailable: usize,
}

struct InitialBlossomServerActivity {
    server: Url,
    available: usize,
    unavailable: usize,
    planned_uploads: usize,
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
    available_filenames: HashSet<String>,
    compact: bool,
    next_sequence: usize,
    active: HashMap<(String, String), ActiveBlossomPlacement>,
    servers: HashMap<String, BlossomServerUploadActivity>,
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
        available_filenames: HashSet<String>,
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
            available_filenames,
            compact: blobs > DETAILED_UPLOAD_BLOB_LIMIT,
            next_sequence: 0,
            active: HashMap::new(),
            servers: HashMap::new(),
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
    ) -> Option<ActiveBlossomPlacement> {
        let placement = self
            .active
            .remove(&(filename.to_owned(), server.to_string()))?;
        self.completed = self.completed.saturating_add(1).min(self.planned_uploads);
        if matches!(
            status,
            BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent
        ) {
            self.confirmed = self.confirmed.saturating_add(1);
            self.available_filenames.insert(filename.to_owned());
        } else {
            self.unavailable = self.unavailable.saturating_add(1);
        }
        if let Some(summary) = self.servers.get_mut(server.as_str()) {
            summary.completed = summary.completed.saturating_add(1);
            if matches!(
                status,
                BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent
            ) {
                summary.available = summary.available.saturating_add(1);
            } else {
                summary.unavailable = summary.unavailable.saturating_add(1);
            }
        }
        Some(placement)
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
        let mut phases = if self.compact {
            vec![
                format!(
                    "{}/{} files available",
                    self.available_filenames.len(),
                    self.blobs
                ),
                format!("{}/{} copies available", self.confirmed, total),
            ]
        } else {
            vec![format!("{}/{} copies available", self.confirmed, total)]
        };
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
        phases.join("; ")
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
        let heading_style =
            ProgressStyle::with_template(" {spinner} [{elapsed_precise}] {prefix} — {wide_msg}")?
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈");
        let presence_style = ProgressStyle::with_template(
            "   [{elapsed_precise}] Checking existing Blossom copies [{bar:22}] {pos}/{len} {msg}",
        )?
        .progress_chars("##-");
        let presence_server_style =
            ProgressStyle::with_template("      {prefix:28} [{bar:18}] {pos}/{len} {msg}")?
                .progress_chars("##-");
        let upload_style =
            ProgressStyle::with_template(BLOSSOM_UPLOAD_ROW_TEMPLATE)?.progress_chars("##-");
        let phase_style =
            ProgressStyle::with_template(BLOSSOM_PHASE_ROW_TEMPLATE)?.tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈");
        let finished_style = ProgressStyle::with_template(BLOSSOM_FINISHED_ROW_TEMPLATE)?;
        let server_summary_style =
            ProgressStyle::with_template(BLOSSOM_SERVER_SUMMARY_ROW_TEMPLATE)?;
        let compact_upload_style =
            ProgressStyle::with_template(BLOSSOM_COMPACT_UPLOAD_ROW_TEMPLATE)?
                .progress_chars("##-");
        let compact_phase_style = ProgressStyle::with_template(BLOSSOM_COMPACT_PHASE_ROW_TEMPLATE)?
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈");
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
            server_summary_style,
            compact_upload_style,
            compact_phase_style,
            sequential_file: Mutex::new(None),
            presence: Mutex::new(BlossomPresenceActivity::default()),
            activity: Mutex::new(BlossomActivity::default()),
        }))
    }

    /// Present repeated single-file engine calls as one sequential workflow.
    ///
    /// OCI publication deliberately snapshots one potentially large blob at a
    /// time. Without this outer scope every inner placement would misleadingly
    /// render as `file 1/1` even though the shared engine is processing a
    /// larger image.
    pub(crate) fn set_sequential_file(
        &self,
        index: usize,
        total: usize,
        filename: impl Into<String>,
    ) {
        *self
            .sequential_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SequentialFileScope {
            index,
            total,
            filename: filename.into(),
        });
    }

    fn sequential_file_scope(&self) -> Option<SequentialFileScope> {
        self.sequential_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn start_presence_checks(&self, blobs: usize, servers: &[Url], checks: usize) {
        self.restore_draw_target();
        self.clear_presence_bars();
        self.clear_placement_bars();
        self.heading.reset();
        self.heading.set_length(checks as u64);
        self.heading.set_position(0);
        self.heading.set_style(self.presence_style.clone());
        let scope = self
            .sequential_file_scope()
            .map_or_else(String::new, |scope| {
                format!(
                    "file {}/{}: {} — ",
                    scope.index, scope.total, scope.filename
                )
            });
        self.heading.set_message(format!(
            "{scope}{blobs} blob(s) across {} server(s)",
            servers.len()
        ));
        let mut presence = self
            .presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        presence.server_order = servers.to_vec();
        presence.outcomes.clear();
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

    fn record_placement_outcome(&self, filename: &str, server: &Url, status: BlossomServerStatus) {
        self.presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcomes
            .insert((filename.to_owned(), server.to_string()), status);
    }

    fn initial_upload_activity(
        &self,
        filenames: &[String],
    ) -> (HashSet<String>, Vec<InitialBlossomServerActivity>) {
        let presence = self
            .presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut available_filenames = HashSet::new();
        let servers = presence
            .server_order
            .iter()
            .map(|server| {
                let mut available = 0;
                let mut unavailable = 0;
                let mut planned_uploads = 0;
                for filename in filenames {
                    match presence
                        .outcomes
                        .get(&(filename.clone(), server.to_string()))
                    {
                        Some(BlossomServerStatus::Stored | BlossomServerStatus::AlreadyPresent) => {
                            available += 1;
                            available_filenames.insert(filename.clone());
                        }
                        Some(
                            BlossomServerStatus::Failed
                            | BlossomServerStatus::Unknown
                            | BlossomServerStatus::NotAttempted,
                        ) => unavailable += 1,
                        None => planned_uploads += 1,
                    }
                }
                InitialBlossomServerActivity {
                    server: server.clone(),
                    available,
                    unavailable,
                    planned_uploads,
                }
            })
            .collect();
        (available_filenames, servers)
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
        let (available_filenames, initial_servers) = self.initial_upload_activity(&filenames);
        let (subject, message) = {
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
                available_filenames,
            );
            if activity.compact {
                for server in initial_servers.into_iter().rev() {
                    let summary_bar = self
                        .multi
                        .insert_after(&self.heading, ProgressBar::new_spinner());
                    summary_bar.set_style(self.server_summary_style.clone());
                    summary_bar.set_prefix(blossom_server_label(&server.server));
                    activity.servers.insert(
                        server.server.to_string(),
                        BlossomServerUploadActivity {
                            summary_bar,
                            focus_bar: None,
                            focus_sequence: None,
                            total: blobs,
                            planned_uploads: server.planned_uploads,
                            completed: 0,
                            available: server.available,
                            unavailable: server.unavailable,
                        },
                    );
                }
                self.refresh_compact_server_rows(&mut activity);
            }
            (activity.subject(), activity.message())
        };
        self.heading.reset();
        self.heading.unset_length();
        self.heading.set_style(self.heading_style.clone());
        self.heading.set_prefix(subject);
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
            if !activity.active.contains_key(&key) {
                let sequence = activity.next_sequence;
                activity.next_sequence = activity.next_sequence.saturating_add(1);
                activity.active.insert(
                    key.clone(),
                    ActiveBlossomPlacement {
                        phase: BlossomPhase::Uploading,
                        bar: None,
                        total_bytes,
                        uploaded_bytes: 0,
                        message: String::new(),
                        server_label: blossom_server_label(server),
                        sequence,
                    },
                );
            }
            let should_show = !activity.compact;
            let Some(placement) = activity.active.get_mut(&key) else {
                return;
            };
            placement.phase = BlossomPhase::Uploading;
            placement.total_bytes = total_bytes;
            placement.uploaded_bytes = 0;
            placement.message = blossom_file_message(
                &if attempt > 1 {
                    format!("uploading (attempt {attempt}/{max_attempts})")
                } else {
                    "uploading".to_owned()
                },
                filename,
            );
            if placement.bar.is_none() && should_show {
                placement.bar = Some(self.multi.add(ProgressBar::new(total_bytes)));
            }
            if let Some(bar) = &placement.bar {
                bar.reset();
                self.render_active_placement(bar, placement);
            }
            self.refresh_compact_server_rows(&mut activity);
            activity.message()
        };
        self.heading.set_message(message);
    }

    fn increment_upload(&self, filename: &str, server: &Url, bytes: u64) {
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(placement) = activity
            .active
            .get_mut(&(filename.to_owned(), server.to_string()))
        {
            placement.uploaded_bytes = placement.uploaded_bytes.saturating_add(bytes);
            if let Some(bar) = &placement.bar {
                bar.set_position(placement.uploaded_bytes);
            }
        }
        self.refresh_compact_server_rows(&mut activity);
    }

    fn set_operation_phase(
        &self,
        filename: &str,
        server: &Url,
        phase: BlossomPhase,
        message: &str,
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
                placement.message = blossom_file_message(message, filename);
                if let Some(bar) = &placement.bar {
                    self.render_active_placement(bar, placement);
                }
                self.refresh_compact_server_rows(&mut activity);
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
            let Some(placement) = activity.finish(filename, server, status) else {
                return false;
            };
            if let Some(bar) = placement.bar {
                bar.set_style(self.finished_style.clone());
                bar.finish_with_message(blossom_file_message(
                    blossom_finished_status_label(status),
                    filename,
                ));
                activity.finished_bars.push(bar);
            }
            self.refresh_compact_server_rows(&mut activity);
            activity.message()
        };
        self.heading.set_message(heading_message);
        true
    }

    fn render_active_placement(&self, bar: &ProgressBar, placement: &ActiveBlossomPlacement) {
        bar.set_prefix(placement.server_label.clone());
        match placement.phase {
            BlossomPhase::Uploading => {
                bar.disable_steady_tick();
                bar.set_style(self.upload_style.clone());
                bar.set_length(placement.total_bytes);
                bar.set_position(placement.uploaded_bytes);
            }
            BlossomPhase::AwaitingResponse | BlossomPhase::Verifying | BlossomPhase::Retrying => {
                bar.set_style(self.phase_style.clone());
                bar.enable_steady_tick(Duration::from_millis(100));
            }
        }
        bar.set_message(placement.message.clone());
    }

    fn refresh_compact_server_rows(&self, activity: &mut BlossomActivity) {
        if !activity.compact {
            return;
        }
        let server_keys = activity.servers.keys().cloned().collect::<Vec<_>>();
        for server_key in server_keys {
            let mut uploading = 0_usize;
            let mut awaiting_response = 0_usize;
            let mut verifying = 0_usize;
            let mut retrying = 0_usize;
            let focus = activity
                .active
                .iter()
                .filter(|((_, server), placement)| {
                    if server != &server_key {
                        return false;
                    }
                    match placement.phase {
                        BlossomPhase::Uploading => uploading += 1,
                        BlossomPhase::AwaitingResponse => awaiting_response += 1,
                        BlossomPhase::Verifying => verifying += 1,
                        BlossomPhase::Retrying => retrying += 1,
                    }
                    true
                })
                .map(|(_, placement)| placement)
                .min_by_key(|placement| {
                    (blossom_phase_priority(placement.phase), placement.sequence)
                })
                .cloned();
            let Some(summary) = activity.servers.get_mut(&server_key) else {
                continue;
            };
            let active = uploading
                .saturating_add(awaiting_response)
                .saturating_add(verifying)
                .saturating_add(retrying);
            let pending = summary
                .planned_uploads
                .saturating_sub(summary.completed.saturating_add(active));
            let mut parts = vec![format!(
                "{}/{} copies available",
                summary.available, summary.total
            )];
            for (count, label) in [
                (uploading, "uploading"),
                (awaiting_response, "awaiting response"),
                (verifying, "verifying storage"),
                (retrying, "waiting to retry"),
                (pending, "pending"),
                (summary.unavailable, "unavailable"),
            ] {
                if count != 0 {
                    parts.push(format!("{count} {label}"));
                }
            }
            summary.summary_bar.set_message(parts.join("; "));

            if let Some(placement) = focus {
                if summary.focus_bar.is_none() {
                    summary.focus_bar = Some(
                        self.multi
                            .insert_after(&summary.summary_bar, ProgressBar::new(0)),
                    );
                }
                let Some(bar) = summary.focus_bar.as_ref() else {
                    continue;
                };
                if summary.focus_sequence != Some(placement.sequence) {
                    bar.reset_elapsed();
                }
                self.render_compact_placement(bar, &placement);
                summary.focus_sequence = Some(placement.sequence);
            } else {
                if let Some(bar) = summary.focus_bar.take() {
                    bar.finish_and_clear();
                }
                summary.focus_sequence = None;
            }
        }
    }

    fn render_compact_placement(&self, bar: &ProgressBar, placement: &ActiveBlossomPlacement) {
        match placement.phase {
            BlossomPhase::Uploading => {
                bar.disable_steady_tick();
                bar.set_style(self.compact_upload_style.clone());
                bar.set_length(placement.total_bytes);
                bar.set_position(placement.uploaded_bytes);
            }
            BlossomPhase::AwaitingResponse | BlossomPhase::Verifying | BlossomPhase::Retrying => {
                bar.set_style(self.compact_phase_style.clone());
                bar.enable_steady_tick(Duration::from_millis(100));
            }
        }
        bar.set_message(placement.message.clone());
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
            if let Some(bar) = &placement.bar {
                bar.finish_and_clear();
            }
        }
        activity.active.clear();
        for summary in activity.servers.values_mut() {
            if let Some(bar) = summary.focus_bar.take() {
                bar.finish_and_clear();
            }
            summary.summary_bar.finish_and_clear();
        }
        activity.servers.clear();
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
                .map_or(0, |placement| placement.uploaded_bytes)
        };
        self.set_operation_phase(
            filename,
            server,
            BlossomPhase::AwaitingResponse,
            &blossom_timed_attempt_message(
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
            &blossom_timed_attempt_message(
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
        self.record_placement_outcome(filename, server, status);
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

fn blossom_file_message(action: &str, filename: &str) -> String {
    format!("{action} — {filename}")
}

const fn blossom_phase_priority(phase: BlossomPhase) -> u8 {
    match phase {
        BlossomPhase::Uploading => 0,
        BlossomPhase::Verifying => 1,
        BlossomPhase::AwaitingResponse => 2,
        BlossomPhase::Retrying => 3,
    }
}

fn blossom_finished_status_label(status: BlossomServerStatus) -> &'static str {
    match status {
        BlossomServerStatus::Stored => "uploaded and verified",
        BlossomServerStatus::AlreadyPresent => "already stored",
        BlossomServerStatus::Failed => "failed",
        BlossomServerStatus::Unknown => "unavailable",
        BlossomServerStatus::NotAttempted => "not attempted",
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
            } => {
                let (batch, batches) = self
                    .sequential_file_scope()
                    .filter(|_| *batch == 1 && *batches == 1)
                    .map_or((*batch, *batches), |scope| (scope.index, scope.total));
                self.start_upload_group(
                    batch,
                    batches,
                    *blobs,
                    filenames.clone(),
                    *placements,
                    *confirmed,
                    *unavailable,
                );
            }
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
                &format!("waiting to retry (attempt {next_attempt}/{max_attempts})"),
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
    fn compact_rows_form_a_plain_server_hierarchy() {
        assert!(BLOSSOM_SERVER_SUMMARY_ROW_TEMPLATE.starts_with("  {prefix"));
        assert!(BLOSSOM_COMPACT_UPLOAD_ROW_TEMPLATE.starts_with("    ["));
        assert!(BLOSSOM_COMPACT_PHASE_ROW_TEMPLATE.starts_with("    {spinner"));
        for template in [
            BLOSSOM_SERVER_SUMMARY_ROW_TEMPLATE,
            BLOSSOM_COMPACT_UPLOAD_ROW_TEMPLATE,
            BLOSSOM_COMPACT_PHASE_ROW_TEMPLATE,
        ] {
            assert!(!template.contains("cyan"));
            assert!(!template.contains("blue"));
            assert!(!template.contains(".dim"));
        }
    }

    #[test]
    fn sequential_file_scope_preserves_outer_file_numbering() -> Result<()> {
        let progress = BlossomUploadProgress::new(true)?;
        progress.set_sequential_file(2, 5, "layer.tar");
        progress.update(&BlossomProgressEvent::PresenceChecksStarted {
            blobs: 1,
            servers: vec![Url::parse("https://blossom.example/")?],
            checks: 1,
        });
        assert!(progress.heading.message().contains("file 2/5: layer.tar"));

        progress.update(&BlossomProgressEvent::UploadBatchStarted {
            batch: 1,
            batches: 1,
            blobs: 1,
            filenames: vec!["layer.tar".to_owned()],
            placements: 1,
            confirmed: 0,
            unavailable: 0,
            bytes: 10,
        });
        let activity = progress.activity.lock().unwrap();
        assert_eq!(activity.batch, 2);
        assert_eq!(activity.batches, 5);
        assert!(activity.subject().contains("file 2/5"));
        Ok(())
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
            .clone()
            .expect("a single-file upload should have a visible row");
        assert_eq!(bar.message(), "uploading — release.tar.gz");
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
        assert!(progress.heading.prefix().contains("2 files"));
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
            .clone()
            .expect("a small upload group should show every placement");
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
        assert!(placed.contains("1/3 copies available"));
        assert!(placed.contains("1 uploading"));
        let activity = progress.activity.lock().unwrap();
        assert_eq!(activity.finished_bars.len(), 1);
        assert_eq!(
            activity.finished_bars[0].message(),
            "uploaded and verified — ngit-grasp.tar.gz"
        );
        drop(activity);

        progress.update(&BlossomProgressEvent::UploadBatchFinished {
            batch: 1,
            batches: 1,
        });
        assert!(progress.activity.lock().unwrap().finished_bars.is_empty());
        Ok(())
    }

    #[test]
    fn compact_progress_has_one_focused_row_per_server_and_truthful_counts() -> Result<()> {
        let progress = BlossomUploadProgress::new(true)?;
        let first_server = Url::parse("https://one.example/")?;
        let second_server = Url::parse("https://two.example/")?;
        let filenames = (0..6)
            .map(|index| format!("asset-{index}.js"))
            .collect::<Vec<_>>();
        progress.update(&BlossomProgressEvent::PresenceChecksStarted {
            blobs: filenames.len(),
            servers: vec![first_server.clone(), second_server.clone()],
            checks: filenames.len() * 2,
        });
        for (filename, server) in [
            (&filenames[0], &first_server),
            (&filenames[2], &first_server),
            (&filenames[4], &first_server),
            (&filenames[1], &second_server),
            (&filenames[3], &second_server),
        ] {
            progress.update(&BlossomProgressEvent::PlacementFinished {
                filename: filename.clone(),
                server: server.clone(),
                status: BlossomServerStatus::AlreadyPresent,
                message: None,
            });
        }
        progress.update(&BlossomProgressEvent::AuthorizationStarted {
            batch: 1,
            batches: 1,
            blobs: filenames.len(),
            filenames: filenames.clone(),
        });
        progress.update(&BlossomProgressEvent::UploadBatchStarted {
            batch: 1,
            batches: 1,
            blobs: filenames.len(),
            filenames: filenames.clone(),
            placements: 7,
            confirmed: 5,
            unavailable: 0,
            bytes: 70,
        });
        assert!(progress.heading.prefix().contains("6 files"));
        assert!(progress.heading.message().contains("5/6 files available"));
        assert!(progress.heading.message().contains("5/12 copies available"));
        {
            let activity = progress.activity.lock().unwrap();
            assert_eq!(activity.servers.len(), 2);
            assert!(
                activity.servers[first_server.as_str()]
                    .summary_bar
                    .message()
                    .contains("3/6 copies available")
            );
            assert!(
                activity.servers[second_server.as_str()]
                    .summary_bar
                    .message()
                    .contains("2/6 copies available")
            );
        }

        for (filename, server) in [
            (&filenames[1], &first_server),
            (&filenames[3], &first_server),
            (&filenames[0], &second_server),
            (&filenames[2], &second_server),
            (&filenames[5], &second_server),
        ] {
            progress.update(&BlossomProgressEvent::UploadRequestStarted {
                batch: 1,
                batches: 1,
                filename: filename.clone(),
                server: server.clone(),
                attempt: 1,
                max_attempts: 3,
                total_bytes: 10,
                additional_bytes: 0,
            });
        }

        {
            let activity = progress.activity.lock().unwrap();
            assert!(activity.compact);
            assert_eq!(activity.active.len(), 5);
            assert_eq!(activity.servers.len(), 2);
            assert!(
                activity
                    .active
                    .values()
                    .all(|placement| placement.bar.is_none())
            );
            assert!(activity.finished_bars.is_empty());
            let first = &activity.servers[first_server.as_str()];
            assert!(first.summary_bar.message().contains("2 uploading"));
            assert!(first.summary_bar.message().contains("1 pending"));
            assert_eq!(
                first.focus_bar.as_ref().unwrap().message(),
                "uploading — asset-1.js"
            );
            let second = &activity.servers[second_server.as_str()];
            assert!(second.summary_bar.message().contains("3 uploading"));
            assert!(second.summary_bar.message().contains("1 pending"));
            assert_eq!(
                second.focus_bar.as_ref().unwrap().message(),
                "uploading — asset-0.js"
            );
        }

        progress.update(&BlossomProgressEvent::UploadedBytes {
            batch: 1,
            filename: filenames[1].clone(),
            server: first_server.clone(),
            bytes: 5,
        });
        assert_eq!(
            progress.activity.lock().unwrap().servers[first_server.as_str()]
                .focus_bar
                .as_ref()
                .unwrap()
                .position(),
            5
        );
        progress.update(&BlossomProgressEvent::PlacementFinished {
            filename: filenames[1].clone(),
            server: first_server.clone(),
            status: BlossomServerStatus::Stored,
            message: None,
        });
        {
            let activity = progress.activity.lock().unwrap();
            assert!(activity.finished_bars.is_empty());
            let first = &activity.servers[first_server.as_str()];
            assert!(first.summary_bar.message().contains("4/6 copies available"));
            assert!(first.summary_bar.message().contains("1 uploading"));
            assert_eq!(
                first.focus_bar.as_ref().unwrap().message(),
                "uploading — asset-3.js"
            );
        }
        assert!(progress.heading.message().contains("5/6 files available"));
        progress.update(&BlossomProgressEvent::PlacementFinished {
            filename: filenames[5].clone(),
            server: second_server,
            status: BlossomServerStatus::Stored,
            message: None,
        });
        let activity = progress.activity.lock().unwrap();
        assert!(activity.finished_bars.is_empty());
        assert!(activity.message().contains("6/6 files available"));
        assert!(activity.message().contains("7/12 copies available"));
        Ok(())
    }
}
