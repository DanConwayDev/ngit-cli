use std::sync::atomic::{AtomicU8, Ordering};

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

static OUTPUT_MODE: AtomicU8 = AtomicU8::new(OutputMode::Normal as u8);

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
        if self.rendered {
            self.term.clear_last_lines(1)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::OutputMode;

    #[test]
    fn git_verbosity_maps_to_shared_output_modes() {
        assert_eq!(OutputMode::from_git_verbosity(0), OutputMode::Quiet);
        assert_eq!(OutputMode::from_git_verbosity(1), OutputMode::Normal);
        assert_eq!(OutputMode::from_git_verbosity(2), OutputMode::Verbose);
        assert_eq!(OutputMode::from_git_verbosity(u8::MAX), OutputMode::Verbose);
    }
}
