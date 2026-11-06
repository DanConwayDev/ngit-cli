//! Central relay budgets for NGITTEST and the integration harness.
//! These ceilings do not delay completed operations. ExpectedFailure is only
//! for fixtures that deliberately exhaust a relay timeout, never evidence of
//! rejection by itself.

pub const RELAY_TIMEOUT_ENV: &str = "NGIT_TEST_RELAY_TIMEOUT_SECS";
/// Production overall relay budget in seconds. Overrides the 45s
/// `long_timeout`. Example: `NGIT_RELAY_TIMEOUT_SECS=8 ngit repo`
pub const RELAY_TIMEOUT_SECS_ENV: &str = "NGIT_RELAY_TIMEOUT_SECS";
/// Production per-connect attempt budget in seconds. Overrides the 7s
/// `short_timeout` (also used as the quorum grace). If unset,
/// `NGIT_RELAY_TIMEOUT_SECS` caps it too.
pub const RELAY_CONNECT_TIMEOUT_SECS_ENV: &str = "NGIT_RELAY_CONNECT_TIMEOUT_SECS";

#[derive(Clone, Copy, Debug)]
pub enum RelayTimeout {
    Normal,
    ExpectedFailure,
}

impl RelayTimeout {
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Normal => 10,
            Self::ExpectedFailure => 3,
        }
    }
}
