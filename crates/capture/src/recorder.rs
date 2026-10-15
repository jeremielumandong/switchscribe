//! A recording session: one thread per track pulling from its source into an Opus file,
//! a shared clock that pauses, live levels for the meters, and bookmarks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::audio::{self, Resampler, SAMPLE_RATE};
use crate::ogg_opus::OpusWriter;
use crate::source::{FakeSource, FakeVoice, OpenFn};
use crate::{Error, Result, Track, io_err};

/// How long a source may stay quiet before the gap is filled with silence. Loopback
/// streams deliver nothing while nobody talks; padding keeps both tracks on one clock.
const IDLE_PAD: Duration = Duration::from_millis(200);
/// How long a meter holds its level without new audio before dropping to zero.
const METER_HOLD: Duration = Duration::from_millis(300);
/// Name of the file describing a finished recording, next to the tracks.
pub const CAPTURE_FILE: &str = "capture.json";

/// Opens the two tracks' sources.
pub struct Backend {
    /// The microphone.
    pub mic: OpenFn,
    /// Teams (or the whole system).
    pub remote: OpenFn,
}

impl Backend {
    /// Two synthetic voices instead of devices.
    pub fn fake() -> Self {
        Self {
            mic: FakeSource::open(FakeVoice::Near),
            remote: FakeSource::open(FakeVoice::Far),
        }
    }
}

/// Something the user should hear about while recording.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// The preferred source was unavailable and a fallback is recording instead.
    Fallback {
        /// Which track.
        track: Track,
        /// Why, in words for the banner.
        reason: String,
    },
    /// A track could not start or stopped early; the other track keeps recording.
    TrackLost {
        /// Which track.
        track: Track,
        /// Position in the recording, milliseconds.
        at_ms: u64,
        /// The device error.
        error: String,
    },
}

/// What one track ended up with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackReport {
    /// Which track.
    pub track: Track,
    /// The file name inside the meeting folder, if anything was written.
    pub file: Option<String>,
    /// What was captured.
    pub source: Option<String>,
    /// Length of the audio in the file, milliseconds.
    pub duration_ms: u64,
    /// Why the track stopped early or never started.
    pub error: Option<String>,
}

/// A finished recording, also saved as `capture.json` in its folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    /// Recorded time, pauses excluded, milliseconds.
    pub duration_ms: u64,
    /// The tracks.
    pub tracks: Vec<TrackReport>,
    /// Bookmarked moments, milliseconds into the recording.
    pub bookmarks: Vec<u64>,
}

impl Recording {
    /// Read a folder's `capture.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(CAPTURE_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        serde_json::from_str(&text).map_err(|e| Error::Format {
            path,
            what: e.to_string(),
        })
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CAPTURE_FILE);
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::Format {
            path: path.clone(),
            what: e.to_string(),
        })?;
        std::fs::write(&path, text).map_err(|e| io_err(&path, e))
    }
}

#[derive(Debug)]
struct Clock {
    started: Instant,
    paused_since: Option<Instant>,
    paused_total: Duration,
}

impl Clock {
    fn elapsed(&self) -> Duration {
        let now = self.paused_since.unwrap_or_else(Instant::now);
        now.saturating_duration_since(self.started)
            .saturating_sub(self.paused_total)
    }
}

struct Shared {
    stop: AtomicBool,
    clock: Mutex<Clock>,
    /// Meter levels as `f32` bits, indexed like [`Track::ALL`].
    levels: [AtomicU32; 2],
    notices: Mutex<Vec<Notice>>,
    /// Where written audio is copied for live captions, once someone asks.
    tap: Mutex<Option<Sender<Tapped>>>,
}

/// Audio a track just wrote, for live captions: 48 kHz mono.
#[derive(Clone, Debug)]
pub struct Tapped {
    /// The track.
    pub track: Track,
    /// Where it starts in the track, in samples at 48 kHz.
    pub start: u64,
    /// The samples.
    pub samples: Vec<f32>,
}

impl Shared {
    fn clock(&self) -> MutexGuard<'_, Clock> {
        self.clock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self, notice: Notice) {
        tracing::warn!(?notice, "capture notice");
        self.notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(notice);
    }

    fn set_level(&self, track: Track, level: f32) {
        self.levels[index(track)].store(level.to_bits(), Ordering::Relaxed);
    }
}

fn index(track: Track) -> usize {
    match track {
        Track::Mic => 0,
        Track::Remote => 1,
    }
}

/// A cheap, cloneable view of a running recorder for UI polling.
#[derive(Clone)]
pub struct Monitor(Arc<Shared>);

impl Monitor {
    /// Recorded time so far, pauses excluded.
    pub fn elapsed(&self) -> Duration {
        self.0.clock().elapsed()
    }

    /// Whether recording is paused.
    pub fn is_paused(&self) -> bool {
        self.0.clock().paused_since.is_some()
    }

    /// Current meter level of a track, `0.0..=1.0`.
    pub fn level(&self, track: Track) -> f32 {
        f32::from_bits(self.0.levels[index(track)].load(Ordering::Relaxed))
    }

    /// Notices raised since the last call.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(
            &mut *self
                .0
                .notices
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

/// A running recording.
pub struct Recorder {
    shared: Arc<Shared>,
    dir: PathBuf,
    threads: Vec<JoinHandle<TrackReport>>,
    bookmarks: Vec<u64>,
}

impl Recorder {
    /// Create `dir` and start recording both tracks into it. Sources open on their own
    /// threads, so this returns at once; a source that fails to open shows up as a
    /// [`Notice::TrackLost`].
    pub fn start(dir: &Path, backend: Backend) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            clock: Mutex::new(Clock {
                started: Instant::now(),
                paused_since: None,
                paused_total: Duration::ZERO,
            }),
            levels: [AtomicU32::new(0), AtomicU32::new(0)],
            notices: Mutex::new(Vec::new()),
            tap: Mutex::new(None),
        });
        let mut threads = Vec::new();
        for (track, open) in [(Track::Mic, backend.mic), (Track::Remote, backend.remote)] {
            let shared = shared.clone();
            let path = dir.join(track.file_name());
            let handle = std::thread::Builder::new()
                .name(format!("capture-{}", track.meter_label().to_lowercase()))
                .spawn(move || run_track(track, open, &path, &shared))
                .map_err(|e| io_err(dir, e))?;
            threads.push(handle);
        }
        tracing::info!(dir = %dir.display(), "recording started");
        Ok(Self {
            shared,
            dir: dir.to_path_buf(),
            threads,
            bookmarks: Vec::new(),
        })
    }

    /// A handle for polling levels, time and notices from another thread.
    pub fn monitor(&self) -> Monitor {
        Monitor(self.shared.clone())
    }

    /// A copy of the audio both tracks write from now on (silence padding left out),
    /// for live captions. A later call replaces the earlier receiver.
    pub fn tap(&self) -> Receiver<Tapped> {
        let (tx, rx) = mpsc::channel();
        *self.shared.tap.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    /// The meeting folder being recorded into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Pause or resume. While paused, audio is read and dropped and the clock stands
    /// still, so the files skip the paused stretch.
    pub fn set_paused(&self, paused: bool) {
        let mut clock = self.shared.clock();
        match (paused, clock.paused_since) {
            (true, None) => clock.paused_since = Some(Instant::now()),
            (false, Some(since)) => {
                clock.paused_total += since.elapsed();
                clock.paused_since = None;
            }
            _ => {}
        }
    }

    /// Mark the current moment; returns its position in milliseconds.
    pub fn bookmark(&mut self) -> u64 {
        let at = self.shared.clock().elapsed().as_millis() as u64;
        self.bookmarks.push(at);
        at
    }

    /// Bookmarks so far, milliseconds.
    pub fn bookmarks(&self) -> &[u64] {
        &self.bookmarks
    }

    /// Stop both tracks, finish the files and write `capture.json`. Blocks until the
    /// capture threads exit (a tenth of a second or so), so call it off the UI thread.
    pub fn stop(self) -> Result<Recording> {
        let duration_ms = self.shared.clock().elapsed().as_millis() as u64;
        self.shared.stop.store(true, Ordering::Relaxed);
        let mut tracks = Vec::new();
        for (handle, track) in self.threads.into_iter().zip(Track::ALL) {
            let report = handle.join().unwrap_or_else(|_| TrackReport {
                track,
                file: None,
                source: None,
                duration_ms: 0,
                error: Some("the capture thread panicked".into()),
            });
            tracks.push(report);
        }
        let recording = Recording {
            duration_ms,
            tracks,
            bookmarks: self.bookmarks,
        };
        recording.save(&self.dir)?;
        tracing::info!(dir = %self.dir.display(), duration_ms, "recording stopped");
        Ok(recording)
    }
}

/// The body of a capture thread: open the source, then copy audio into the track's file
/// until told to stop or the source fails.
fn run_track(track: Track, open: OpenFn, path: &Path, shared: &Shared) -> TrackReport {
    let mut report = TrackReport {
        track,
        file: None,
        source: None,
        duration_ms: 0,
        error: None,
    };
    let lost = |error: String, report: &mut TrackReport| {
        let at_ms = shared.clock().elapsed().as_millis() as u64;
        shared.notify(Notice::TrackLost {
            track,
            at_ms,
            error: error.clone(),
        });
        report.error = Some(error);
    };
    let opened = match open() {
        Ok(o) => o,
        Err(e) => {
            lost(e.to_string(), &mut report);
            return report;
        }
    };
    report.source = Some(opened.description.clone());
    if let Some(reason) = opened.fallback {
        shared.notify(Notice::Fallback { track, reason });
    }
    tracing::info!(?track, source = %opened.description, "capture source open");
    let mut writer = match OpusWriter::create(path) {
        Ok(w) => w,
        Err(e) => {
            lost(e.to_string(), &mut report);
            return report;
        }
    };
    report.file = Some(track.file_name().to_owned());

    let mut source = opened.source;
    let mut resampler: Option<(u32, Resampler)> = None;
    let mut last_audio = Instant::now();
    let mut buf = Vec::new();
    let result: Result<()> = (|| {
        while !shared.stop.load(Ordering::Relaxed) {
            let block = source.read(Duration::from_millis(50))?;
            let (elapsed, paused) = {
                let clock = shared.clock();
                (clock.elapsed(), clock.paused_since.is_some())
            };
            if let Some(block) = block {
                last_audio = Instant::now();
                let mono = audio::downmix(&block.samples, block.channels);
                shared.set_level(track, audio::meter(audio::rms(&mono)));
                if paused {
                    continue;
                }
                buf.clear();
                if block.rate == SAMPLE_RATE {
                    buf.extend_from_slice(&mono);
                } else {
                    let r = match &mut resampler {
                        Some((rate, r)) if *rate == block.rate => r,
                        slot => {
                            &mut slot
                                .insert((block.rate, Resampler::new(block.rate, SAMPLE_RATE)))
                                .1
                        }
                    };
                    r.process(&mono, &mut buf);
                }
                let start = writer.samples();
                writer.push(&buf)?;
                let mut tap = shared.tap.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(tx) = tap.as_ref() {
                    let sent = tx.send(Tapped {
                        track,
                        start,
                        samples: buf.clone(),
                    });
                    if sent.is_err() {
                        // Nobody listens any more.
                        *tap = None;
                    }
                }
            } else if last_audio.elapsed() > METER_HOLD {
                shared.set_level(track, 0.0);
            }
            // Keep the track on the recording clock through silence.
            if !paused && last_audio.elapsed() > IDLE_PAD {
                let due = elapsed.as_secs_f64() * f64::from(SAMPLE_RATE);
                let behind = due as u64 - writer.samples().min(due as u64);
                if behind > 0 {
                    writer.push_silence(behind as usize)?;
                }
            }
        }
        Ok(())
    })();
    shared.set_level(track, 0.0);
    if let Err(e) = result {
        lost(e.to_string(), &mut report);
    }
    report.duration_ms = writer.samples() * 1000 / u64::from(SAMPLE_RATE);
    if let Err(e) = writer.finish() {
        report.error.get_or_insert(e.to_string());
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg_opus::decode_file;
    use crate::source::Opened;

    fn wait(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    #[test]
    fn the_tap_copies_what_each_track_writes() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::start(dir.path(), Backend::fake()).unwrap();
        let tap = rec.tap();
        wait(500);
        drop(rec.stop().unwrap());
        let got: Vec<Tapped> = tap.try_iter().collect();
        for track in Track::ALL {
            let blocks: Vec<&Tapped> = got.iter().filter(|t| t.track == track).collect();
            assert!(!blocks.is_empty(), "{track:?}");
            // Blocks follow each other in the track (padding may sit between them).
            for w in blocks.windows(2) {
                assert!(w[1].start >= w[0].start + w[0].samples.len() as u64);
            }
        }
    }

    #[test]
    fn records_both_tracks_on_one_clock() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::start(dir.path(), Backend::fake()).unwrap();
        wait(600);
        let mon = rec.monitor();
        assert!(mon.level(Track::Mic) > 0.2, "{}", mon.level(Track::Mic));
        let done = rec.stop().unwrap();

        assert_eq!(done.tracks.len(), 2);
        for t in &done.tracks {
            assert_eq!(t.error, None);
            let audio = decode_file(&dir.path().join(t.track.file_name())).unwrap();
            let ms = audio.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
            assert!(
                ms.abs_diff(done.duration_ms) < 150,
                "{ms} vs {}",
                done.duration_ms
            );
        }
        assert_eq!(Recording::load(dir.path()).unwrap(), done);
    }

    #[test]
    fn pause_skips_time() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::start(dir.path(), Backend::fake()).unwrap();
        wait(300);
        rec.set_paused(true);
        let mon = rec.monitor();
        assert!(mon.is_paused());
        let at_pause = mon.elapsed();
        wait(400);
        assert_eq!(mon.elapsed(), at_pause);
        rec.set_paused(false);
        wait(300);
        let done = rec.stop().unwrap();
        assert!(done.duration_ms < 800, "{}", done.duration_ms);
        let mic = done.tracks.iter().find(|t| t.track == Track::Mic).unwrap();
        assert!(mic.duration_ms.abs_diff(done.duration_ms) < 150);
    }

    #[test]
    fn a_lost_track_keeps_the_other_recording() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend {
            mic: Box::new(|| {
                Ok(Opened {
                    source: Box::new(FakeSource::new(FakeVoice::Near).failing_after(0.1)),
                    description: "headset".into(),
                    fallback: None,
                })
            }),
            remote: Box::new(|| Err(Error::Device("no Teams".into()))),
        };
        let mut rec = Recorder::start(dir.path(), backend).unwrap();
        wait(400);
        let at = rec.bookmark();
        let notices = rec.monitor().take_notices();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices.iter().any(|n| matches!(
            n,
            Notice::TrackLost { track: Track::Mic, at_ms, .. } if *at_ms >= 100
        )));
        let done = rec.stop().unwrap();
        assert_eq!(done.bookmarks, vec![at]);
        let remote = done
            .tracks
            .iter()
            .find(|t| t.track == Track::Remote)
            .unwrap();
        assert_eq!(remote.file, None);
        assert!(remote.error.as_deref().unwrap().contains("no Teams"));
        let mic = done.tracks.iter().find(|t| t.track == Track::Mic).unwrap();
        assert!(mic.error.is_some());
        assert!(
            mic.duration_ms >= 90 && mic.duration_ms < 200,
            "{}",
            mic.duration_ms
        );
        assert!(decode_file(&dir.path().join("mic.opus")).is_ok());
    }

    #[test]
    fn silence_is_padded_to_the_clock() {
        struct Quiet;
        impl crate::source::Source for Quiet {
            fn read(&mut self, timeout: Duration) -> Result<Option<crate::audio::Block>> {
                std::thread::sleep(timeout);
                Ok(None)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend {
            mic: FakeSource::open(FakeVoice::Near),
            remote: Box::new(|| {
                Ok(Opened {
                    source: Box::new(Quiet),
                    description: "quiet".into(),
                    fallback: Some("Teams not detected".into()),
                })
            }),
        };
        let rec = Recorder::start(dir.path(), backend).unwrap();
        wait(700);
        let notices = rec.monitor().take_notices();
        assert!(matches!(
            &notices[..],
            [Notice::Fallback {
                track: Track::Remote,
                ..
            }]
        ));
        let done = rec.stop().unwrap();
        let remote = done
            .tracks
            .iter()
            .find(|t| t.track == Track::Remote)
            .unwrap();
        // Padded up to the clock, give or take one idle window.
        assert!(
            remote.duration_ms + 300 > done.duration_ms,
            "{} vs {}",
            remote.duration_ms,
            done.duration_ms
        );
    }
}
