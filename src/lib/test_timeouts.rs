//! Central relay budgets for NGITTEST and the integration harness.
//! These ceilings do not delay completed operations. ExpectedFailure is only
//! for fixtures that deliberately exhaust a relay timeout, never evidence of
//! rejection by itself.

pub const RELAY_TIMEOUT_ENV: &str = "NGIT_TEST_RELAY_TIMEOUT_SECS";

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
