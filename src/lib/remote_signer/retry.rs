//! Delivery budget for one pending NIP-46 request.

use std::time::Duration;

use tokio::time::Instant;

pub(super) const RETRY_INTERVAL: Duration = Duration::from_secs(120);

/// Schedule exposed to progress reporting without request contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryStatus {
    pub next_action_at: Instant,
    pub deadline: Instant,
    /// Automatic sends remaining before the final response wait.
    pub sends_remaining: u8,
}

impl RetryStatus {
    pub(super) fn fresh(now: Instant) -> Self {
        Self {
            next_action_at: now + RETRY_INTERVAL,
            deadline: now + RETRY_INTERVAL * 3,
            sends_remaining: 2,
        }
    }

    pub(super) fn automatic(&mut self, now: Instant) {
        self.sends_remaining -= 1;
        self.next_action_at = now + RETRY_INTERVAL;
        self.deadline = now + RETRY_INTERVAL * u32::from(self.sends_remaining + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_sends_leave_a_full_final_response_interval() {
        let start = Instant::now();
        let mut retry = RetryStatus::fresh(start);
        assert_eq!(retry.next_action_at, start + Duration::from_secs(120));
        assert_eq!(retry.sends_remaining, 2);
        retry.automatic(retry.next_action_at);
        assert_eq!(retry.next_action_at, start + Duration::from_secs(240));
        assert_eq!(retry.sends_remaining, 1);
        retry.automatic(retry.next_action_at);
        assert_eq!(retry.next_action_at, start + Duration::from_secs(360));
        assert_eq!(retry.sends_remaining, 0);
        assert_eq!(retry.next_action_at, retry.deadline);
    }

    #[test]
    fn delayed_third_send_still_gets_a_full_response_interval() {
        let start = Instant::now();
        let mut retry = RetryStatus::fresh(start);
        retry.automatic(start + Duration::from_secs(120));
        retry.automatic(start + Duration::from_secs(250));
        assert_eq!(retry.sends_remaining, 0);
        assert_eq!(retry.next_action_at, start + Duration::from_secs(370));
        assert_eq!(retry.deadline, retry.next_action_at);
    }

    #[test]
    fn each_manual_send_starts_a_new_budget_without_a_lifetime_cap() {
        let start = Instant::now();
        let mut retry = RetryStatus::fresh(start);
        for elapsed in [239, 500, 900, 1_200] {
            let now = start + Duration::from_secs(elapsed);
            retry.automatic(retry.next_action_at);
            retry = RetryStatus::fresh(now);
            assert_eq!(retry.sends_remaining, 2);
            assert_eq!(retry.next_action_at, now + RETRY_INTERVAL);
            assert_eq!(retry.deadline, now + Duration::from_secs(360));
        }
    }
}
