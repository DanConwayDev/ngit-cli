//! Compatibility launcher for the `git-remote-nostr` remote helper.
//!
//! Git discovers remote helpers strictly by executable name, so an
//! executable called `git-remote-nostr` must remain on `PATH`. The
//! implementation, however, lives in `ngit` (see
//! `src/bin/ngit/git_remote_helper/`). This launcher re-invokes `ngit`
//! with a hidden internal command, forwarding all arguments and stdio
//! untouched so git's remote-helper protocol flows straight through.
//!
//! It must never write to stdout itself: stdout belongs to the
//! protocol.
#![cfg_attr(not(test), warn(clippy::pedantic))]
#![cfg_attr(not(test), warn(clippy::expect_used))]

use std::{
    env,
    path::PathBuf,
    process::{Command, exit},
};

/// Hidden `ngit` entry point for the remote helper. Must match
/// `INTERNAL_COMMAND` in `src/bin/ngit/git_remote_helper/mod.rs`.
const INTERNAL_COMMAND: &str = "__git-remote-nostr";

/// Prefer the `ngit` installed next to this launcher — both
/// `cargo install ngit` and the cargo test layout place the two
/// executables in the same directory — falling back to `PATH`
/// resolution.
fn ngit_executable() -> PathBuf {
    if let Ok(current_exe) = env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let sibling = dir.join(format!("ngit{}", env::consts::EXE_SUFFIX));
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("ngit")
}

fn main() {
    let ngit = ngit_executable();
    let mut command = Command::new(&ngit);
    command.arg(INTERNAL_COMMAND).args(env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec replaces this process so signals, stdio and exit status
        // are transparently the child's; it only returns on failure.
        let err = command.exec();
        eprintln!(
            "error: git-remote-nostr failed to launch `{}`: {err}",
            ngit.display()
        );
        eprintln!("error: is ngit installed and on PATH?");
        exit(127);
    }

    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!(
                    "error: git-remote-nostr failed to launch `{}`: {err}",
                    ngit.display()
                );
                eprintln!("error: is ngit installed and on PATH?");
                exit(127);
            }
        }
    }
}
