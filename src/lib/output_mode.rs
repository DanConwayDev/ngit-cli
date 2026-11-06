use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::Result;
use console::Term;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OutputMode {
    Quiet = 0,
    Normal = 1,
    Verbose = 2,
}

impl OutputMode {
    const fn from_git_verbosity(verbosity: u8) -> Self {
        match verbosity {
            0 => Self::Quiet,
            1 => Self::Normal,
            _ => Self::Verbose,
        }
    }
}

static JSON_MODE: AtomicBool = AtomicBool::new(false);

static OUTPUT_MODE: AtomicU8 = AtomicU8::new(OutputMode::Normal as u8);

/// Verbosity determines detail; JSON and terminal detection determine
/// rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgressPolicy {
    pub animate: bool,
    pub detailed: bool,
    pub quiet: bool,
}

impl ProgressPolicy {
    #[must_use]
    pub const fn new(mode: OutputMode, json: bool, terminal: bool) -> Self {
        let quiet = matches!(mode, OutputMode::Quiet);
        Self {
            animate: terminal && !json && !quiet,
            detailed: matches!(mode, OutputMode::Verbose),
            quiet,
        }
    }
}

pub fn set_json_mode(enabled: bool) {
    JSON_MODE.store(enabled, Ordering::Relaxed);
}

#[must_use]
pub fn is_json() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

#[must_use]
pub fn progress_policy() -> ProgressPolicy {
    let mode = if is_quiet() {
        OutputMode::Quiet
    } else if is_verbose() {
        OutputMode::Verbose
    } else {
        OutputMode::Normal
    };
    ProgressPolicy::new(mode, is_json(), Term::stderr().is_term())
}

// Disabled until a direct, non-JSON CLI invocation explicitly enables it.
// Git remote helpers must never consume terminal input for signer recovery.
static TERMINAL_RECOVERY_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_terminal_recovery_enabled(enabled: bool) {
    TERMINAL_RECOVERY_ENABLED.store(enabled, Ordering::Relaxed);
}

#[must_use]
pub fn terminal_recovery_enabled() -> bool {
    TERMINAL_RECOVERY_ENABLED.load(Ordering::Relaxed)
}

pub fn set_output_mode(mode: OutputMode) {
    OUTPUT_MODE.store(mode as u8, Ordering::Relaxed);
}

/// Apply Git's remote-helper `verbosity` option to this process.
///
/// Git uses `0` for quiet, `1` for normal output, and larger values for
/// increasingly verbose output.
pub fn set_git_verbosity(verbosity: u8) {
    set_output_mode(OutputMode::from_git_verbosity(verbosity));
}

#[must_use]
pub fn is_quiet() -> bool {
    OUTPUT_MODE.load(Ordering::Relaxed) == OutputMode::Quiet as u8
}

#[must_use]
pub fn is_verbose() -> bool {
    OUTPUT_MODE.load(Ordering::Relaxed) == OutputMode::Verbose as u8
}

pub fn write_progress_line(term: &Term, message: &str) -> Result<()> {
    if !is_quiet() {
        term.write_line(message)?;
    }
    Ok(())
}

/// A status line that knows whether it was rendered, so quiet mode can never
/// clear terminal content that it did not write.
pub struct TransientLine<'a> {
    term: &'a Term,
    rendered: bool,
}

impl<'a> TransientLine<'a> {
    pub fn write(term: &'a Term, message: &str) -> Result<Self> {
        let rendered = !is_quiet();
        if rendered {
            term.write_line(message)?;
        }
        Ok(Self { term, rendered })
    }

    pub fn clear(&self) -> Result<()> {
        if self.rendered && progress_policy().animate {
            self.term.clear_last_lines(1)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputMode, ProgressPolicy};

    #[test]
    fn json_and_pipes_never_animate_regardless_of_verbosity() {
        for mode in [OutputMode::Quiet, OutputMode::Normal, OutputMode::Verbose] {
            for terminal in [false, true] {
                let policy = ProgressPolicy::new(mode, true, terminal);
                assert!(!policy.animate);
                assert_eq!(policy.detailed, mode == OutputMode::Verbose);
                assert_eq!(policy.quiet, mode == OutputMode::Quiet);
            }
            assert!(!ProgressPolicy::new(mode, false, false).animate);
        }
        assert!(ProgressPolicy::new(OutputMode::Normal, false, true).animate);
        assert!(ProgressPolicy::new(OutputMode::Verbose, false, true).animate);
        assert!(!ProgressPolicy::new(OutputMode::Quiet, false, true).animate);
    }

    #[test]
    fn git_verbosity_maps_to_shared_output_modes() {
        assert_eq!(OutputMode::from_git_verbosity(0), OutputMode::Quiet);
        assert_eq!(OutputMode::from_git_verbosity(1), OutputMode::Normal);
        assert_eq!(OutputMode::from_git_verbosity(2), OutputMode::Verbose);
        assert_eq!(OutputMode::from_git_verbosity(u8::MAX), OutputMode::Verbose);
    }
}
