//! Live captions (M6): while a call records, each track's audio is cut at pauses into
//! short pieces and run through a small model with greedy decoding. The captions are
//! provisional; the post-call pass replaces them with the real transcript.
//!
//! One thread does the work for both tracks. When the model cannot keep up, older pieces
//! are dropped so the captions stay current rather than complete.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use switchscribe_capture::{Tapped, Track};

use crate::{Options, RATE, Result, SpeechEngine};

/// The recorder's rate; [`RATE`] divides it.
const TAP_RATE: u64 = 48_000;
/// Input samples per output sample.
const DECIMATE: usize = (TAP_RATE / RATE as u64) as usize;
/// A piece is cut at a pause once it is this long…
const MIN_PIECE: usize = 2 * RATE as usize;
/// …and regardless once it reaches this.
const MAX_PIECE: usize = 8 * RATE as usize;
/// The pause that ends a piece.
const PAUSE: usize = RATE as usize / 2;
/// Level (RMS) that counts as speech.
const SPEECH_RMS: f32 = 0.01;
/// Pieces waiting for the model beyond this are dropped, oldest first.
const MAX_WAITING: usize = 2;

/// A caption: provisional text for a stretch of one track.
#[derive(Clone, Debug, PartialEq)]
pub struct Caption {
    /// The track it was heard on (the mic is the user).
    pub track: Track,
    /// Start, milliseconds into the recording.
    pub start_ms: i64,
    /// End, milliseconds into the recording.
    pub end_ms: i64,
    /// What was said.
    pub text: String,
}

/// What the captioner reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// New caption.
    Caption(Caption),
    /// The model would not load; no captions this recording.
    Failed(String),
}

/// Loads the speech engine on the captioner's thread (loading takes a moment).
pub type LoadEngine = Box<dyn FnOnce() -> Result<Box<dyn SpeechEngine>> + Send>;

/// Captions a recording from its tap until dropped or the recording ends.
pub struct Captioner {
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
}

impl Captioner {
    /// Start captioning `audio` (from [`switchscribe_capture::Recorder::tap`]).
    pub fn start(audio: Receiver<Tapped>, load: LoadEngine, language: Option<String>) -> Self {
        let (tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let spawned = thread::Builder::new()
            .name("live-captions".into())
            .spawn(move || run(audio, load, language, &tx, &flag));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start live captions");
        }
        Self { events, stop }
    }

    /// Events since the last call. Never blocks.
    pub fn take(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

impl Drop for Captioner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A piece of one track waiting for the model.
#[derive(Debug)]
struct Piece {
    track: Track,
    start_ms: i64,
    audio: Vec<f32>,
}

/// Cuts one track's audio into pieces.
#[derive(Debug)]
struct Cutter {
    track: Track,
    /// 16 kHz audio not yet cut.
    buf: Vec<f32>,
    /// Where `buf` starts, in 16 kHz samples.
    start: u64,
    /// Where the next tapped block should start, at 48 kHz.
    next: u64,
    /// Input samples carried over to the next block (fewer than [`DECIMATE`]).
    carry: Vec<f32>,
}

impl Cutter {
    fn new(track: Track) -> Self {
        Self {
            track,
            buf: Vec::new(),
            start: 0,
            next: 0,
            carry: Vec::new(),
        }
    }

    /// Add a block; returns any pieces it completes.
    fn push(&mut self, block: &Tapped, out: &mut Vec<Piece>) {
        if block.start > self.next {
            // The recorder padded silence: a pause.
            self.cut(out);
            self.carry.clear();
        }
        if self.buf.is_empty() {
            self.start = block.start / DECIMATE as u64;
        }
        self.next = block.start + block.samples.len() as u64;
        self.carry.extend_from_slice(&block.samples);
        let (groups, _) = self.carry.as_chunks::<DECIMATE>();
        self.buf.extend(
            groups
                .iter()
                .map(|c| c.iter().sum::<f32>() / DECIMATE as f32),
        );
        self.carry.drain(..groups.len() * DECIMATE);
        let full = self.buf.len() >= MAX_PIECE;
        let paused =
            self.buf.len() >= MIN_PIECE && rms(&self.buf[self.buf.len() - PAUSE..]) < SPEECH_RMS;
        if full || paused {
            self.cut(out);
        }
    }

    /// Cut what is buffered into a piece if it holds speech.
    fn cut(&mut self, out: &mut Vec<Piece>) {
        if self.buf.is_empty() {
            return;
        }
        let audio = std::mem::take(&mut self.buf);
        let start = self.start;
        self.start += audio.len() as u64;
        if has_speech(&audio) {
            out.push(Piece {
                track: self.track,
                start_ms: (start * 1000 / u64::from(RATE)) as i64,
                audio,
            });
        }
    }
}

fn rms(audio: &[f32]) -> f32 {
    if audio.is_empty() {
        return 0.0;
    }
    (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt()
}

/// Whether any 30 ms frame is loud enough to be speech.
fn has_speech(audio: &[f32]) -> bool {
    audio
        .chunks(RATE as usize * 3 / 100)
        .any(|f| rms(f) >= SPEECH_RMS)
}

fn run(
    audio: Receiver<Tapped>,
    load: LoadEngine,
    language: Option<String>,
    tx: &Sender<Event>,
    stop: &AtomicBool,
) {
    let engine = match load() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "live captions unavailable");
            let _ = tx.send(Event::Failed(e.to_string()));
            return;
        }
    };
    let opts = Options {
        prompt: String::new(),
        language,
        fast: true,
    };
    let mut cutters = [Cutter::new(Track::Mic), Cutter::new(Track::Remote)];
    let mut waiting: VecDeque<Piece> = VecDeque::new();
    let mut cut = Vec::new();
    let mut open = true;
    while !stop.load(Ordering::Relaxed) && (open || !waiting.is_empty()) {
        // Take everything recorded so far; wait a little only when there is nothing to do.
        let first = if waiting.is_empty() && open {
            match audio.recv_timeout(Duration::from_millis(200)) {
                Ok(b) => Some(b),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    open = false;
                    None
                }
            }
        } else {
            None
        };
        for block in first.into_iter().chain(audio.try_iter()) {
            let i = usize::from(block.track == Track::Remote);
            cutters[i].push(&block, &mut cut);
        }
        if !open {
            // The recording ended: caption what is left.
            for c in &mut cutters {
                c.cut(&mut cut);
            }
        }
        waiting.extend(cut.drain(..));
        while waiting.len() > MAX_WAITING {
            waiting.pop_front();
        }
        let Some(piece) = waiting.pop_front() else {
            continue;
        };
        match engine.transcribe(&piece.audio, &opts) {
            Ok(t) => {
                for r in t.segments {
                    let caption = Caption {
                        track: piece.track,
                        start_ms: piece.start_ms + r.start_ms,
                        end_ms: piece.start_ms + r.end_ms,
                        text: r.text,
                    };
                    if tx.send(Event::Caption(caption)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "a live caption failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Recognized, Transcribed};

    /// Says how long each piece was.
    struct Measure;

    impl SpeechEngine for Measure {
        fn transcribe(&self, audio: &[f32], opts: &Options) -> Result<Transcribed> {
            assert!(opts.fast);
            let ms = (audio.len() as u64 * 1000 / u64::from(RATE)) as i64;
            Ok(Transcribed {
                segments: vec![Recognized {
                    start_ms: 0,
                    end_ms: ms,
                    text: format!("{ms} ms"),
                }],
                language: None,
            })
        }
    }

    fn tone(secs: f32) -> Vec<f32> {
        (0..(secs * TAP_RATE as f32) as usize)
            .map(|i| 0.3 * (i as f32 * 0.05).sin())
            .collect()
    }

    fn blocks(track: Track, start: u64, audio: &[f32]) -> Vec<Tapped> {
        audio
            .chunks(480)
            .scan(start, |at, c| {
                let b = Tapped {
                    track,
                    start: *at,
                    samples: c.to_vec(),
                };
                *at += c.len() as u64;
                Some(b)
            })
            .collect()
    }

    #[test]
    fn pieces_end_at_pauses() {
        let (tx, rx) = mpsc::channel();
        // 3 s of speech, 1 s of quiet, then 1 s of speech after a padded gap.
        let mut audio = tone(3.0);
        audio.extend(std::iter::repeat_n(0.0, TAP_RATE as usize));
        for b in blocks(Track::Remote, 0, &audio) {
            tx.send(b).unwrap();
        }
        for b in blocks(Track::Mic, 6 * TAP_RATE, &tone(1.0)) {
            tx.send(b).unwrap();
        }
        drop(tx);
        let c = Captioner::start(rx, Box::new(|| Ok(Box::new(Measure) as _)), None);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < 2 && std::time::Instant::now() < deadline {
            got.extend(c.take());
            thread::sleep(Duration::from_millis(20));
        }
        let caps: Vec<Caption> = got
            .into_iter()
            .filter_map(|e| match e {
                Event::Caption(c) => Some(c),
                Event::Failed(_) => None,
            })
            .collect();
        assert_eq!(caps.len(), 2, "{caps:?}");
        let teams = caps.iter().find(|c| c.track == Track::Remote).unwrap();
        assert_eq!(teams.start_ms, 0);
        // Cut at the pause, between 2.5 and 4 s.
        assert!((2500..=4000).contains(&teams.end_ms), "{teams:?}");
        let me = caps.iter().find(|c| c.track == Track::Mic).unwrap();
        assert_eq!(me.start_ms, 6000);
    }

    #[test]
    fn silence_is_not_captioned() {
        let mut out = Vec::new();
        let mut c = Cutter::new(Track::Mic);
        for b in blocks(Track::Mic, 0, &vec![0.0; 10 * TAP_RATE as usize]) {
            c.push(&b, &mut out);
        }
        c.cut(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_model_that_will_not_load_says_so() {
        let (_tx, rx) = mpsc::channel();
        let c = Captioner::start(
            rx,
            Box::new(|| Err(crate::Error::Model("no model".into()))),
            None,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(Event::Failed(e)) = c.take().into_iter().next() {
                assert!(e.contains("no model"));
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
    }
}
