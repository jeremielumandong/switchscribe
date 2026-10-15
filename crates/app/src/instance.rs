//! One switchscribe per user: a second launch (a `switchscribe://` link clicked in a Jira
//! issue, or the app started twice) hands its request to the running app over a loopback
//! socket and exits, so two processes never share the data folder.
//!
//! The running app listens on 127.0.0.1 and writes the port and a random key to
//! `instance.port` in the data folder; a request must carry the key, so only the same
//! user can send one. Links look like `switchscribe://meeting/<id>?t=<seconds>`.

use std::hash::{BuildHasher as _, Hasher as _};
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// The file holding the running app's port and key.
const PORT_FILE: &str = "instance.port";
/// The link scheme.
pub const SCHEME: &str = "switchscribe";

/// What a launch asks the running app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Come to the front.
    Show,
    /// Open a meeting at a moment (seconds).
    Open(String, i64),
}

impl Request {
    /// The request a command line makes: a link argument, else Show.
    pub fn from_args(args: &[String]) -> Request {
        args.iter()
            .find_map(|a| parse_link(a))
            .unwrap_or(Request::Show)
    }

    fn line(&self) -> String {
        match self {
            Request::Show => "show".into(),
            Request::Open(id, t) => format!("open {id} {t}"),
        }
    }

    fn parse_line(line: &str) -> Option<Request> {
        let mut parts = line.split_whitespace();
        match parts.next()? {
            "show" => Some(Request::Show),
            "open" => {
                let id = parts.next()?;
                let t = parts.next()?.parse().ok()?;
                valid_id(id).then(|| Request::Open(id.to_owned(), t))
            }
            _ => None,
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A link to a moment of a meeting.
pub fn link(meeting_id: &str, at_secs: i64) -> String {
    format!("{SCHEME}://meeting/{meeting_id}?t={at_secs}")
}

/// Read a `switchscribe://meeting/<id>?t=<seconds>` link.
pub fn parse_link(s: &str) -> Option<Request> {
    let rest = s.trim().strip_prefix(SCHEME)?.strip_prefix("://")?;
    let rest = rest.strip_prefix("meeting/")?.trim_end_matches('/');
    let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
    let id = id.trim_end_matches('/');
    if !valid_id(id) {
        return None;
    }
    let t = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("t="))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    Some(Request::Open(id.to_owned(), t))
}

/// Hand `request` to a running app. True when one took it.
pub fn forward(data: &Path, request: &Request) -> bool {
    let Ok(text) = std::fs::read_to_string(data.join(PORT_FILE)) else {
        return false;
    };
    let mut parts = text.split_whitespace();
    let (Some(port), Some(key)) = (
        parts.next().and_then(|p| p.parse::<u16>().ok()),
        parts.next(),
    ) else {
        return false;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if writeln!(stream, "{key} {}", request.line()).is_err() {
        return false;
    }
    // The app answers "ok" once it has the request.
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// The running app's end: requests from later launches.
pub struct Listener {
    requests: Receiver<Request>,
    port_file: PathBuf,
}

impl Listener {
    /// Listen and publish the port. `None` if the socket or the file cannot be made (the
    /// app then just runs without the handoff).
    pub fn start(data: &Path) -> Option<Listener> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| tracing::warn!(error = %e, "could not listen for other launches"))
            .ok()?;
        let port = listener.local_addr().ok()?.port();
        let key = random_key();
        let port_file = data.join(PORT_FILE);
        if let Err(e) = std::fs::write(&port_file, format!("{port} {key}\n")) {
            tracing::warn!(error = %e, "could not write the instance file");
            return None;
        }
        let (tx, requests) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("instance".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
                        continue;
                    };
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let Some(rest) = line.trim().strip_prefix(key.as_str()) else {
                        tracing::warn!("ignored a request without the instance key");
                        continue;
                    };
                    if let Some(request) = Request::parse_line(rest) {
                        let mut stream = stream;
                        let _ = writeln!(stream, "ok");
                        if tx.send(request).is_err() {
                            return;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the instance listener");
            return None;
        }
        Some(Listener {
            requests,
            port_file,
        })
    }

    /// Requests since the last call. Never blocks.
    pub fn take(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.port_file);
    }
}

fn random_key() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    let a = h.finish();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(a);
    format!("{a:016x}{:016x}", h.finish())
}

/// Make `switchscribe://` links open this executable (per user, no admin rights).
#[cfg(windows)]
pub fn register_scheme() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let key = format!(r"HKCU\Software\Classes\{SCHEME}");
    let command = format!("\"{}\" \"%1\"", exe.display());
    let runs: [Vec<String>; 3] = [
        vec![
            "add".into(),
            key.clone(),
            "/ve".into(),
            "/d".into(),
            "URL:switchscribe".into(),
            "/f".into(),
        ],
        vec![
            "add".into(),
            key.clone(),
            "/v".into(),
            "URL Protocol".into(),
            "/d".into(),
            String::new(),
            "/f".into(),
        ],
        vec![
            "add".into(),
            format!(r"{key}\shell\open\command"),
            "/ve".into(),
            "/d".into(),
            command,
            "/f".into(),
        ],
    ];
    for args in runs {
        let mut cmd = switchscribe_agent::process::command("reg");
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.status() {
            Ok(s) if s.success() => {}
            other => {
                tracing::warn!(?other, "could not register the switchscribe:// links");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let l = link("2026-10-15-1000-aks", 236);
        assert_eq!(l, "switchscribe://meeting/2026-10-15-1000-aks?t=236");
        assert_eq!(
            parse_link(&l),
            Some(Request::Open("2026-10-15-1000-aks".into(), 236))
        );
        assert_eq!(
            parse_link("switchscribe://meeting/aks/"),
            Some(Request::Open("aks".into(), 0))
        );
        assert_eq!(parse_link("switchscribe://meeting/../etc?t=1"), None);
        assert_eq!(parse_link("https://meeting/aks"), None);
        assert_eq!(
            Request::from_args(&["--demo".into(), l]),
            Request::Open("2026-10-15-1000-aks".into(), 236)
        );
        assert_eq!(Request::from_args(&[]), Request::Show);
    }

    #[test]
    fn a_second_launch_hands_over() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!forward(dir.path(), &Request::Show));
        let listener = Listener::start(dir.path()).unwrap();
        assert!(forward(dir.path(), &Request::Open("aks".into(), 42)));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let got = loop {
            let r = listener.take();
            if !r.is_empty() || std::time::Instant::now() > deadline {
                break r;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(got, [Request::Open("aks".into(), 42)]);
        // A request without the key is dropped.
        let text = std::fs::read_to_string(dir.path().join(PORT_FILE)).unwrap();
        let port: u16 = text.split_whitespace().next().unwrap().parse().unwrap();
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        writeln!(s, "wrong show").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(listener.take().is_empty());
        drop(listener);
        assert!(!dir.path().join(PORT_FILE).exists());
    }
}
