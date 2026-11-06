use anyhow::Result;
use console::Term;
use tokio::time::Instant;

use super::{Indicator, RelayLine, Report};
use crate::{output_mode::ProgressPolicy, remote_signer::RetryStatus};

/// Own the complete terminal report, including its waiting spinner and
/// countdown. Redirected output keeps reports as an append-only history of
/// transitions.
pub(super) struct CountdownLine<'a> {
    term: &'a Term,
    policy: ProgressPolicy,
    last_report: Option<String>,
    body: Vec<String>,
    animated_relays: Vec<(usize, RelayLine)>,
    frame: u64,
    action: Option<String>,
    rendered_action: String,
    tip: Option<String>,
    size: Option<(u16, u16)>,
}

impl<'a> CountdownLine<'a> {
    pub(super) fn new(term: &'a Term, policy: ProgressPolicy) -> Self {
        Self {
            term,
            policy,
            last_report: None,
            body: Vec::new(),
            animated_relays: Vec::new(),
            frame: 0,
            action: None,
            rendered_action: String::new(),
            tip: None,
            size: None,
        }
    }

    pub(super) fn show(
        &mut self,
        report: Report,
        retry: Option<RetryStatus>,
        deadline: Instant,
    ) -> Result<()> {
        if self.policy.quiet {
            return Ok(());
        }
        let key = report_key(&report, retry);
        if !self.policy.animate && self.last_report.as_ref() == Some(&key) {
            return Ok(());
        }
        self.last_report = Some(key);
        self.clear()?;
        self.body = report.body_lines();
        if report.waiting {
            self.animated_relays = report
                .relays
                .iter()
                .enumerate()
                .filter(|(_, relay)| relay.indicator != Indicator::Failed)
                .map(|(index, relay)| (index + 1, relay.clone()))
                .collect();
        }
        self.tip = report.waiting.then(|| {
            console::Style::new()
                .for_stderr()
                .color256(247)
                .apply_to(CONNECTION_TIP)
                .to_string()
        });
        self.action = Some(report.action);
        self.paint(retry, deadline)
    }

    pub(super) fn show_final(&mut self, report: Report) -> Result<()> {
        self.clear()?;
        for line in report.body_lines() {
            self.term.write_line(&line)?;
        }
        if !report.action.is_empty() {
            self.term.write_line(&report.action)?;
        }
        Ok(())
    }

    pub(super) fn refresh(&mut self, retry: Option<RetryStatus>, deadline: Instant) -> Result<()> {
        if !self.policy.animate || self.action.is_none() {
            return Ok(());
        }
        let size = self.term.size();
        let rows = self.rows(size.1);
        if self.size != Some(size) {
            // Resize invalidates ownership; append just one fresh frame.
            self.advance_spinner();
            return self.paint(retry, deadline);
        }
        if can_replace(self.size, size, rows) {
            self.term.clear_last_lines(rows)?;
            self.advance_spinner();
            self.paint(retry, deadline)?;
        }
        // Oversized blocks have no safely addressable header/action/tip region.
        // Keep them static until the next transition rather than erase history.
        Ok(())
    }

    pub(super) fn clear(&mut self) -> Result<()> {
        let size = self.term.size();
        let rows = self.rows(size.1);
        if self.policy.animate && can_replace(self.size, size, rows) {
            self.term.clear_last_lines(rows)?;
        }
        self.body.clear();
        self.animated_relays.clear();
        self.frame = 0;
        self.action = None;
        self.rendered_action.clear();
        self.tip = None;
        self.size = None;
        Ok(())
    }

    fn rows(&self, width: u16) -> usize {
        self.body
            .iter()
            .map(|line| wrapped_rows(line, width))
            .sum::<usize>()
            + if self.action.is_some() {
                wrapped_rows(&self.rendered_action, width)
            } else {
                0
            }
            + self.tip.as_ref().map_or(0, |tip| wrapped_rows(tip, width))
    }

    fn advance_spinner(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        for (row, relay) in &self.animated_relays {
            self.body[*row] = relay.render(self.frame);
        }
    }

    fn paint(&mut self, retry: Option<RetryStatus>, deadline: Instant) -> Result<()> {
        let size = self.term.size();
        for line in &self.body {
            self.term.write_line(line)?;
        }
        self.write_action(retry, deadline)?;
        if let Some(tip) = &self.tip {
            self.term.write_line(tip)?;
        }
        // A resize during output can mix wrapping widths; do not claim those rows.
        self.size = (self.term.size() == size).then_some(size);
        Ok(())
    }

    fn write_action(&mut self, retry: Option<RetryStatus>, deadline: Instant) -> Result<()> {
        if let Some(action) = &self.action {
            self.rendered_action = if self.policy.animate {
                format!("{action} {}", timing(retry, deadline, Instant::now()))
            } else {
                log_action(action, retry)
            };
            self.term.write_line(&self.rendered_action)?;
        }
        Ok(())
    }
}

fn log_action(action: &str, retry: Option<RetryStatus>) -> String {
    retry.map_or_else(
        || action.to_owned(),
        |retry| {
            format!(
                "{action} Attempted {}/3; {}.",
                3 - retry.sends_remaining,
                if retry.sends_remaining > 0 {
                    "automatic resend scheduled"
                } else {
                    "awaiting final response"
                }
            )
        },
    )
}

// Retry timestamps identify new attempts without exposing request identifiers
// in logs.
fn report_key(report: &Report, retry: Option<RetryStatus>) -> String {
    format!("{:?}\n{}\n{retry:?}", report.body_lines(), report.action)
}

const CONNECTION_TIP: &str = "Tip: check signer is connected to these relays";

fn can_replace(rendered: Option<(u16, u16)>, current: (u16, u16), rows: usize) -> bool {
    rendered == Some(current) && rows > 0 && rows < usize::from(current.0)
}

fn wrapped_rows(text: &str, width: u16) -> usize {
    text.split('\n')
        .map(|line| {
            console::measure_text_width(line)
                .div_ceil(usize::from(width).max(1))
                .max(1)
        })
        .sum()
}

fn seconds_left(deadline: Instant, now: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(now);
    remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)
}

fn timing(retry: Option<RetryStatus>, deadline: Instant, now: Instant) -> String {
    if let Some(retry) = retry {
        let (action, at) = if retry.sends_remaining > 0 {
            ("Resend", retry.next_action_at)
        } else {
            ("Timeout", deadline)
        };
        format!(
            "{action} in {}s (attempted {}/3).",
            seconds_left(at, now),
            3 - retry.sends_remaining
        )
    } else {
        format!("Timeout in {}s.", seconds_left(deadline, now))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn append_only_actions_identify_attempts_without_clock_updates() {
        let now = Instant::now();
        for remaining in 0..=2 {
            let retry = RetryStatus {
                next_action_at: now,
                deadline: now + Duration::from_secs(120),
                sends_remaining: remaining,
            };
            let line = log_action("Check signer approvals.", Some(retry));
            assert!(line.contains(&format!("Attempted {}/3", 3 - remaining)));
            assert_eq!(line.contains("automatic resend scheduled"), remaining > 0);
            let later = RetryStatus {
                next_action_at: now + Duration::from_secs(10),
                ..retry
            };
            assert_eq!(line, log_action("Check signer approvals.", Some(later)));
        }
        assert_eq!(log_action("Waiting.", None), "Waiting.");
    }

    #[test]
    fn append_only_dedup_tracks_transitions_and_retry_attempts() {
        let mut report = Report {
            heading: "Waiting: signing".to_string(),
            waiting: true,
            relays: vec![RelayLine {
                indicator: Indicator::Loading,
                text: "relay: subscription pending".to_string(),
            }],
            details: Vec::new(),
            action: "Check signer approvals.".to_string(),
        };
        let now = Instant::now();
        let retry = RetryStatus {
            next_action_at: now,
            deadline: now + Duration::from_secs(120),
            sends_remaining: 2,
        };
        let first = report_key(&report, Some(retry));
        assert_eq!(first, report_key(&report, Some(retry)));
        let next = RetryStatus {
            sends_remaining: 1,
            ..retry
        };
        assert_ne!(first, report_key(&report, Some(next)));
        report.relays[0].text = "relay: awaiting response".to_string();
        assert_ne!(first, report_key(&report, Some(retry)));
    }

    #[test]
    fn replacing_a_report_counts_body_and_wrapped_action_rows() {
        let body = [
            "Waiting",
            "  ✓ relay: accepted",
            "  … relay: awaiting acceptance",
        ];
        let body_rows = body
            .iter()
            .map(|line| wrapped_rows(line, 20))
            .sum::<usize>();
        let action_rows = wrapped_rows("Press R to resend now.", 20);
        assert_eq!(body_rows, 4);
        assert_eq!(action_rows, 2);
        assert!(can_replace(
            Some((24, 20)),
            (24, 20),
            body_rows + action_rows
        ));
    }

    #[test]
    fn owned_rows_include_the_tip_after_the_countdown() {
        let term = Term::buffered_stderr();
        let display = CountdownLine {
            term: &term,
            policy: ProgressPolicy::new(crate::output_mode::OutputMode::Normal, false, true),
            last_report: None,
            body: vec!["Waiting".to_string()],
            animated_relays: Vec::new(),
            frame: 0,
            action: Some("Press R to resend now.".to_string()),
            rendered_action: "Press R to resend now. Timeout in 120s.".to_string(),
            tip: Some(CONNECTION_TIP.to_string()),
            size: Some((24, 20)),
        };
        assert_eq!(display.rows(20), 1 + 2 + 3);
        assert!(!can_replace(display.size, (6, 20), display.rows(20)));
    }

    #[test]
    fn changed_size_and_oversized_reports_never_clear_earlier_output() {
        assert!(!can_replace(Some((24, 20)), (24, 80), 4));
        assert!(!can_replace(Some((24, 80)), (10, 80), 4));
        assert!(!can_replace(Some((24, 80)), (24, 80), 24));
        assert!(!can_replace(None, (24, 80), 4));
        assert!(!can_replace(Some((24, 80)), (24, 80), 0));
    }

    #[test]
    fn row_count_handles_ansi_and_embedded_newlines() {
        assert_eq!(wrapped_rows("\x1b[32m✓\x1b[0m relay", 7), 1);
        assert_eq!(wrapped_rows("abcdefgh\nx", 4), 3);
        assert_eq!(wrapped_rows("", 0), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn countdown_counts_attempts_for_this_request_and_the_final_wait() {
        let started = Instant::now();
        let mut retry = RetryStatus {
            next_action_at: started + Duration::from_secs(120),
            deadline: started + Duration::from_secs(360),
            sends_remaining: 2,
        };
        assert_eq!(
            timing(Some(retry), retry.deadline, Instant::now()),
            "Resend in 120s (attempted 1/3)."
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(timing(Some(retry), retry.deadline, Instant::now()).contains("119s"));
        retry.sends_remaining = 1;
        retry.next_action_at = started + Duration::from_secs(240);
        tokio::time::advance(Duration::from_secs(119)).await;
        assert_eq!(
            timing(Some(retry), retry.deadline, Instant::now()),
            "Resend in 120s (attempted 2/3)."
        );
        retry.sends_remaining = 0;
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(
            timing(Some(retry), retry.deadline, Instant::now()),
            "Timeout in 120s (attempted 3/3)."
        );
        // A manual resend starts a fresh window for the same logical request.
        retry.sends_remaining = 2;
        retry.next_action_at = Instant::now() + Duration::from_secs(120);
        retry.deadline = Instant::now() + Duration::from_secs(360);
        assert_eq!(
            timing(Some(retry), retry.deadline, Instant::now()),
            "Resend in 120s (attempted 1/3)."
        );
        assert_eq!(
            timing(
                None,
                Instant::now() + Duration::from_secs(120),
                Instant::now()
            ),
            "Timeout in 120s."
        );
    }
}
