//! Meeting detection: is a Teams call in progress?
//!
//! On Windows a call shows up as an active audio session on a capture device (the
//! microphone) owned by Teams or one of its child processes; Teams keeps that stream open
//! while muted, so it lasts the whole call. The call's window title, when Teams shows a
//! meeting window, names it. [`Watcher`] polls on its own thread and reports a call only
//! once it has been seen twice in a row, and its end only after it has been gone for a
//! few polls, so a brief device switch does not end a meeting.
//!
//! `SWITCHSCRIBE_FAKE_MEETING=<title>` fakes a call a few seconds after start on any OS.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// How often the audio sessions are checked.
pub const POLL: Duration = Duration::from_secs(2);
/// Polls a call must be seen in before it counts.
const SEEN_TO_START: u32 = 2;
/// Polls a call must be missing from before it has ended.
const MISSING_TO_END: u32 = 4;

/// A Teams call in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamsCall {
    /// The process holding the call's audio session.
    pub pid: u32,
    /// The meeting's name from its Teams window, when one shows it.
    pub title: Option<String>,
}

/// What the watcher saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detection {
    /// A call started (or the watcher started during one).
    Started(TeamsCall),
    /// The call ended.
    Ended,
}

/// One look at the system: the call in progress, if any.
pub trait Probe: Send {
    /// Look now.
    fn probe(&mut self) -> Option<TeamsCall>;
}

/// The probe for this machine: Teams' audio sessions on Windows, a fake call when
/// `SWITCHSCRIBE_FAKE_MEETING` is set, nothing otherwise.
pub fn system_probe() -> Option<Box<dyn Probe>> {
    if let Ok(title) = std::env::var("SWITCHSCRIBE_FAKE_MEETING") {
        return Some(Box::new(FakeProbe::new(title, Duration::from_secs(4))));
    }
    #[cfg(windows)]
    {
        Some(Box::new(crate::windows::SessionProbe))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// A call that starts after a delay and never ends.
pub struct FakeProbe {
    title: String,
    from: Instant,
}

impl FakeProbe {
    /// A call named `title` from `after` now.
    pub fn new(title: String, after: Duration) -> Self {
        Self {
            title,
            from: Instant::now() + after,
        }
    }
}

impl Probe for FakeProbe {
    fn probe(&mut self) -> Option<TeamsCall> {
        (Instant::now() >= self.from).then(|| TeamsCall {
            pid: 0,
            title: (!self.title.trim().is_empty()).then(|| self.title.clone()),
        })
    }
}

/// Turns probe results into started / ended events, with hysteresis.
#[derive(Default)]
pub struct Debounce {
    seen: u32,
    missing: u32,
    active: Option<TeamsCall>,
}

impl Debounce {
    /// Feed one probe result; returns an event when the state changes.
    pub fn feed(&mut self, now: Option<TeamsCall>) -> Option<Detection> {
        match now {
            Some(call) => {
                self.missing = 0;
                self.seen += 1;
                match &mut self.active {
                    Some(active) => {
                        // A title can appear after the call starts; keep the latest.
                        if call.title.is_some() {
                            active.title = call.title;
                        }
                        None
                    }
                    None if self.seen >= SEEN_TO_START => {
                        self.active = Some(call.clone());
                        Some(Detection::Started(call))
                    }
                    None => None,
                }
            }
            None => {
                self.seen = 0;
                self.missing += 1;
                if self.active.is_some() && self.missing >= MISSING_TO_END {
                    self.active = None;
                    Some(Detection::Ended)
                } else {
                    None
                }
            }
        }
    }

    /// The call in progress, with the latest title seen.
    pub fn active(&self) -> Option<&TeamsCall> {
        self.active.as_ref()
    }
}

/// Watches for Teams calls on a thread of its own until dropped.
pub struct Watcher {
    events: Receiver<Detection>,
}

impl Watcher {
    /// Start watching with `probe`.
    pub fn start(mut probe: Box<dyn Probe>) -> Self {
        let (tx, events) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("meeting-detection".into())
            .spawn(move || {
                let mut debounce = Debounce::default();
                loop {
                    if let Some(event) = debounce.feed(probe.probe())
                        && tx.send(event).is_err()
                    {
                        // The watcher was dropped.
                        return;
                    }
                    thread::sleep(POLL);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start meeting detection");
        }
        Self { events }
    }

    /// Events since the last call. Never blocks.
    pub fn take(&self) -> Vec<Detection> {
        let mut out = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

/// The meeting's name from a Teams window title, or `None` for Teams' own views (Chat,
/// Calendar, the main window).
///
/// New Teams titles a meeting window "<meeting> | Microsoft Teams"; views read
/// "Chat | <person> | Microsoft Teams" and the like.
pub fn meeting_title(window_title: &str) -> Option<String> {
    const VIEWS: [&str; 10] = [
        "Chat", "Activity", "Calendar", "Teams", "Calls", "Files", "Apps", "OneDrive", "Search",
        "Settings",
    ];
    let mut parts: Vec<&str> = window_title.split(" | ").map(str::trim).collect();
    let last = parts.last()?;
    if !last.starts_with("Microsoft Teams") {
        return None;
    }
    parts.pop();
    let first = parts.first()?;
    if parts.len() != 1 || VIEWS.contains(first) || first.is_empty() {
        return None;
    }
    let title = first
        .strip_prefix("Meeting in \"")
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(first);
    Some(title.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(title: Option<&str>) -> Option<TeamsCall> {
        Some(TeamsCall {
            pid: 7,
            title: title.map(Into::into),
        })
    }

    #[test]
    fn calls_start_and_end_with_hysteresis() {
        let mut d = Debounce::default();
        assert_eq!(d.feed(call(None)), None);
        assert_eq!(
            d.feed(call(None)),
            Some(Detection::Started(TeamsCall {
                pid: 7,
                title: None
            }))
        );
        // The title shows up later.
        assert_eq!(d.feed(call(Some("Cerberus rollout sync"))), None);
        assert_eq!(
            d.active().and_then(|c| c.title.as_deref()),
            Some("Cerberus rollout sync")
        );
        // A short gap does not end it.
        assert_eq!(d.feed(None), None);
        assert_eq!(d.feed(call(None)), None);
        for _ in 0..3 {
            assert_eq!(d.feed(None), None);
        }
        assert_eq!(d.feed(None), Some(Detection::Ended));
        assert_eq!(d.feed(None), None);
    }

    #[test]
    fn window_titles_name_meetings() {
        assert_eq!(
            meeting_title("Cerberus rollout sync | Microsoft Teams").as_deref(),
            Some("Cerberus rollout sync")
        );
        assert_eq!(
            meeting_title("Meeting in \"Platform\" | Microsoft Teams").as_deref(),
            Some("Platform")
        );
        assert_eq!(meeting_title("Chat | Priya Nair | Microsoft Teams"), None);
        assert_eq!(meeting_title("Calendar | Microsoft Teams"), None);
        assert_eq!(meeting_title("Microsoft Teams"), None);
        assert_eq!(meeting_title("Inbox - Outlook"), None);
    }

    #[test]
    fn the_watcher_reports_a_fake_call() {
        let w = Watcher::start(Box::new(FakeProbe::new("Standup".into(), Duration::ZERO)));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let events = w.take();
            if let Some(Detection::Started(c)) = events.first() {
                assert_eq!(c.title.as_deref(), Some("Standup"));
                break;
            }
            assert!(Instant::now() < deadline, "no call reported");
            thread::sleep(Duration::from_millis(100));
        }
    }
}
