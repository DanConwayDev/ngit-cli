//! Scoped single-key recovery for direct terminal commands only.

pub(super) enum InputAction {
    Resend(nostr::prelude::EventId),
    Cancel,
}

#[cfg(any(unix, test))]
static INPUT_OWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(any(unix, test))]
struct InputOwner;

#[cfg(any(unix, test))]
impl InputOwner {
    fn try_acquire() -> Option<Self> {
        use std::sync::atomic::Ordering;
        INPUT_OWNED
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}

#[cfg(any(unix, test))]
impl Drop for InputOwner {
    fn drop(&mut self) {
        INPUT_OWNED.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(unix)]
pub(super) struct ResendInput {
    tty: std::fs::File,
    event_id: nostr::prelude::EventId,
    original: libc::termios,
    keys: KeyParser,
    _owner: InputOwner,
}

#[cfg(unix)]
impl ResendInput {
    pub(super) fn open(event_id: nostr::prelude::EventId) -> Option<Self> {
        use std::{
            io::IsTerminal,
            os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
        };

        if !eligible(
            crate::output_mode::terminal_recovery_enabled(),
            std::io::stdin().is_terminal(),
            std::io::stderr().is_terminal(),
        ) {
            return None;
        }
        let owner = InputOwner::try_acquire()?;
        // Cancellation disables recovery before releasing the current owner.
        // Recheck after acquisition so a concurrent waiter cannot reopen it.
        if !crate::output_mode::terminal_recovery_enabled() {
            return None;
        }
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
            .ok()?;
        // Background jobs must not take over the shell's terminal.
        // SAFETY: tty is a live terminal descriptor; these calls retain no data.
        if unsafe { libc::tcgetpgrp(tty.as_raw_fd()) != libc::getpgrp() } {
            return None;
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tty owns a live descriptor and original points to writable
        // storage. Its contents are only read after tcgetattr succeeds.
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), original.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: successful tcgetattr initialized the complete termios value.
        let original = unsafe { original.assume_init() };
        let input_mode = key_input_mode(original);
        // Read Ctrl+C as a key so its owner can restore attributes before
        // cancelling, including when signing runs in a background SDK task.
        // Keep output processing intact. TCSAFLUSH drops
        // stale input so keys cannot spill from an earlier signing request.
        // SAFETY: tty and input_mode remain live for this non-retaining call.
        if unsafe { libc::tcsetattr(tty.as_raw_fd(), libc::TCSAFLUSH, &input_mode) } != 0 {
            return None;
        }
        Some(Self {
            tty,
            event_id,
            original,
            keys: KeyParser::default(),
            _owner: owner,
        })
    }

    pub(super) fn event_id(&self) -> nostr::prelude::EventId {
        self.event_id
    }

    pub(super) fn poll(&mut self) -> std::io::Result<Option<InputAction>> {
        use std::io::Read;

        let mut bytes = [0; 128];
        match self.tty.read(&mut bytes) {
            Ok(count) => Ok(self.keys.read(&bytes[..count], self.event_id)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
impl Drop for ResendInput {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor and saved attributes are live until after
        // Drop returns. Flush unread keys before restoring the previous mode.
        let _ = unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSAFLUSH, &self.original) };
    }
}

#[cfg(unix)]
fn key_input_mode(mut attributes: libc::termios) -> libc::termios {
    attributes.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL | libc::ISIG);
    attributes.c_cc[libc::VMIN] = 0;
    attributes.c_cc[libc::VTIME] = 0;
    attributes
}

#[cfg(any(unix, test))]
fn eligible(direct_text_command: bool, stdin_terminal: bool, stderr_terminal: bool) -> bool {
    direct_text_command && stdin_terminal && stderr_terminal
}

#[cfg(not(unix))]
pub(super) struct ResendInput;

#[cfg(not(unix))]
impl ResendInput {
    pub(super) fn open(_event_id: nostr::prelude::EventId) -> Option<Self> {
        None
    }
    pub(super) fn event_id(&self) -> nostr::prelude::EventId {
        unreachable!()
    }
    pub(super) fn poll(&mut self) -> std::io::Result<Option<InputAction>> {
        Ok(None)
    }
}

#[cfg(any(unix, test))]
#[derive(Default)]
enum KeyParser {
    #[default]
    Normal,
    Escape,
    Sequence,
}

#[cfg(any(unix, test))]
impl KeyParser {
    fn read(&mut self, bytes: &[u8], event_id: nostr::prelude::EventId) -> Option<InputAction> {
        // Cancellation wins over resending even when both keys were queued.
        if bytes.contains(&3) {
            return Some(InputAction::Cancel);
        }
        let mut resend = false;
        for byte in bytes {
            match (&*self, byte) {
                (_, 0x1b) => *self = Self::Escape,
                (Self::Normal, b'r' | b'R') => resend = true,
                (Self::Escape, b'[' | b'O') => *self = Self::Sequence,
                (Self::Escape, _) | (Self::Sequence, 0x40..=0x7e) => *self = Self::Normal,
                _ => {}
            }
        }
        resend.then_some(InputAction::Resend(event_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_reader_owns_the_terminal_and_drop_releases_it() {
        let owner = InputOwner::try_acquire().expect("terminal should be unowned");
        assert!(InputOwner::try_acquire().is_none());
        drop(owner);
        assert!(InputOwner::try_acquire().is_some());
    }

    #[test]
    fn both_cases_resend_without_enter_and_coalesce_repeated_keys() {
        let mut keys = KeyParser::default();
        let id = nostr::prelude::EventId::from_byte_array([1; 32]);
        for bytes in [b"r".as_slice(), b"R", b"rRr"] {
            assert!(
                matches!(keys.read(bytes, id), Some(InputAction::Resend(actual)) if actual == id)
            );
        }
        for bytes in [b"\n".as_slice(), b"x", b""] {
            assert!(keys.read(bytes, id).is_none());
        }
    }

    #[test]
    fn terminal_escape_sequences_do_not_count_as_resend_keys() {
        let mut keys = KeyParser::default();
        let id = nostr::prelude::EventId::from_byte_array([1; 32]);
        assert!(keys.read(b"\x1bO", id).is_none());
        assert!(keys.read(b"R", id).is_none()); // F3, split across reads.
        assert!(keys.read(b"\x1b[1;2R", id).is_none());
        assert!(keys.read(b"\x1br", id).is_none()); // Alt-r.
        assert!(matches!(keys.read(b"r", id), Some(InputAction::Resend(_))));
    }

    #[test]
    fn ctrl_c_cancels_instead_of_resending_queued_keys() {
        let mut keys = KeyParser::default();
        let id = nostr::prelude::EventId::from_byte_array([1; 32]);
        assert!(matches!(keys.read(b"\x03", id), Some(InputAction::Cancel)));
        assert!(matches!(
            keys.read(b"r\x03R", id),
            Some(InputAction::Cancel)
        ));
        assert!(keys.read(b"\x1b[", id).is_none());
        assert!(matches!(keys.read(b"\x03", id), Some(InputAction::Cancel)));
    }

    #[test]
    fn only_direct_text_commands_with_terminal_input_and_output_are_eligible() {
        assert!(eligible(true, true, true));
        // The disabled mode covers both JSON and the Git remote helper,
        // independently of the terminal or interactive environment setting.
        assert!(!eligible(false, true, true));
        assert!(!eligible(true, false, true));
        assert!(!eligible(true, true, false));
        assert!(!eligible(true, false, false));
    }

    #[cfg(unix)]
    #[test]
    fn key_mode_reads_control_keys_and_preserves_output_processing() {
        // SAFETY: all termios fields are integers or arrays of integers.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        original.c_lflag = libc::ISIG | libc::ICANON | libc::ECHO | libc::ECHONL;
        original.c_oflag = libc::OPOST;
        let mode = key_input_mode(original);
        assert_eq!(mode.c_lflag & libc::ISIG, 0);
        assert_eq!(mode.c_lflag & (libc::ICANON | libc::ECHO | libc::ECHONL), 0);
        assert_eq!(mode.c_oflag, original.c_oflag);
        assert_eq!(mode.c_iflag, original.c_iflag);
        assert_eq!(mode.c_cflag, original.c_cflag);
        assert_eq!(mode.c_cc[libc::VMIN], 0);
        assert_eq!(mode.c_cc[libc::VTIME], 0);
    }
}
