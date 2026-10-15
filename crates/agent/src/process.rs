//! Starting other programs (the coding CLI, `explorer`, `git`) without a console window.
//!
//! switchscribe is a GUI-subsystem app on Windows, so any console program it starts gets
//! a fresh console window of its own unless told not to. Every spawn goes through
//! [`command`], which passes `CREATE_NO_WINDOW`; clippy rejects `Command::new` anywhere
//! else (see `clippy.toml`).

use std::ffi::OsStr;
use std::process::Command;

/// `CREATE_NO_WINDOW`: run a console program with no console window.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`Command`] for `program` that never opens a console window on Windows. Output is
/// still captured or piped as usual.
#[allow(clippy::disallowed_methods)]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let mut cmd = Command::new(program);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    Command::new(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_captures_output() {
        let out = if cfg!(windows) {
            command("cmd").args(["/C", "echo hi"]).output()
        } else {
            command("sh").args(["-c", "echo hi"]).output()
        }
        .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }
}
