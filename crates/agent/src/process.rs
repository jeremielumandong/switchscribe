//! Starting other programs (the coding CLI, `explorer`, `git`) without a console window.
//!
//! switchscribe is a GUI-subsystem app on Windows, so any console program it starts gets
//! a fresh console window of its own unless told not to. Every spawn goes through
//! [`command`], which passes `CREATE_NO_WINDOW`; clippy rejects `Command::new` anywhere
//! else (see `clippy.toml`).

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// Find `program` on `PATH`. On Windows this also tries the `PATHEXT` extensions, since
/// npm installs CLIs as `claude.cmd`, which `Command` would not find by its bare name.
pub fn which(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|e| dir.join(format!("{program}{e}")))
            .find(|p| is_executable(p))
    })
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// What a finished program printed.
#[derive(Debug)]
pub struct Output {
    /// Exited with success.
    pub success: bool,
    /// Its exit code, when it had one.
    pub code: Option<i32>,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// Why [`run`] gave up.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// It would not start.
    #[error("could not start {program}: {source}")]
    Start {
        /// The program.
        program: String,
        /// Why.
        source: std::io::Error,
    },
    /// It ran past its time and was stopped.
    #[error("{0} took too long and was stopped")]
    Timeout(String),
    /// The user cancelled it.
    #[error("cancelled")]
    Cancelled,
    /// Waiting on it failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Run `cmd` to completion: write `stdin` to it, collect its output, and kill it after
/// `timeout` or once `cancel` is set. Blocks; call it from a worker thread.
pub fn run(
    mut cmd: Command,
    stdin: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output, RunError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunError::Start {
            program: program.clone(),
            source,
        })?;
    // Readers and the writer get their own threads so a full pipe never deadlocks.
    let out = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });
    let err = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });
    if let Some(mut input) = child.stdin.take() {
        let text = stdin.to_owned();
        std::thread::spawn(move || {
            let _ = input.write_all(text.as_bytes());
        });
    }
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Cancelled);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Timeout(program));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        h.and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    Ok(Output {
        success: status.success(),
        code: status.code(),
        stdout: text(out),
        stderr: text(err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_the_path() {
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which(shell).is_some());
        assert!(which("switchscribe-no-such-program").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn runs_with_input_and_stops_on_timeout() {
        let never = AtomicBool::new(false);
        let mut cat = command("sh");
        cat.args(["-c", "cat; echo oops >&2"]);
        let out = run(cat, "hello", Duration::from_secs(10), &never).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr.trim(), "oops");
        let mut slow = command("sh");
        slow.args(["-c", "sleep 5"]);
        assert!(matches!(
            run(slow, "", Duration::from_millis(200), &never),
            Err(RunError::Timeout(_))
        ));
    }

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
