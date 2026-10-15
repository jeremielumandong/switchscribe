//! Transcribing a recorded meeting: decode each track, find speech, transcribe it in
//! chunks, and merge both tracks into one transcript.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use switchscribe_capture::{Recording, Track};
use switchscribe_store::NewSegment;

use crate::vad::{Vad, chunks};
use crate::{Error, Options, RATE, Result, SpeechEngine, merge_tracks};

/// Chunks sent to the model are at most this long (Whisper's window).
const MAX_CHUNK_SECS: usize = 28;
/// Speech regions closer than this are sent together.
const JOIN_GAP_SECS: f32 = 1.5;
/// The transcript file in a meeting folder.
pub const TRANSCRIPT_FILE: &str = "transcript.json";

/// What a transcription needs to know about the meeting.
#[derive(Clone, Debug, Default)]
pub struct JobInput {
    /// The user's person id, for the mic track.
    pub me: Option<i64>,
    /// Vocabulary hint: glossary and attendee names.
    pub prompt: String,
    /// Force a language instead of detecting it.
    pub language: Option<String>,
}

/// A speaker label for segments of a track before diarization (M3).
pub fn track_label(track: Track) -> &'static str {
    match track {
        Track::Mic => "You",
        Track::Remote => "Teams",
    }
}

/// One line of `transcript.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    /// Start, milliseconds.
    pub start_ms: i64,
    /// End, milliseconds.
    pub end_ms: i64,
    /// Which track it was heard on.
    pub track: Track,
    /// Text.
    pub text: String,
}

/// `transcript.json`: the engine's output before speakers are named, kept so speakers
/// can be re-identified without transcribing again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptFile {
    /// Detected or forced language.
    pub language: Option<String>,
    /// Lines in time order.
    pub lines: Vec<TranscriptLine>,
}

impl TranscriptFile {
    /// Read a meeting's `transcript.json`.
    pub fn load(dir: &Path) -> Result<TranscriptFile> {
        let path = dir.join(TRANSCRIPT_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_str(&text).map_err(|e| Error::Engine(format!("{}: {e}", path.display())))
    }
}

/// Transcribe the meeting in `dir`. `progress` receives `0.0..=1.0`; setting `cancel`
/// stops between chunks. Writes `transcript.json` and returns the merged segments.
pub fn transcribe_meeting(
    dir: &Path,
    engine: &dyn SpeechEngine,
    vad: &mut dyn Vad,
    input: &JobInput,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<Vec<NewSegment>> {
    // Which tracks have audio: capture.json when present, else whatever files exist.
    let tracks: Vec<Track> = match Recording::load(dir) {
        Ok(rec) => rec
            .tracks
            .iter()
            .filter(|t| t.file.is_some())
            .map(|t| t.track)
            .collect(),
        Err(_) => Track::ALL
            .into_iter()
            .filter(|t| dir.join(t.file_name()).exists())
            .collect(),
    };
    if tracks.is_empty() {
        return Err(Error::NoAudio);
    }

    // Decode and find speech first, so progress can be weighted by speech length.
    let mut work = Vec::new();
    for track in tracks {
        // Opus decodes straight to 16 kHz, so a long meeting never sits in memory at 48.
        let audio =
            switchscribe_capture::ogg_opus::decode_file_at(&dir.join(track.file_name()), RATE)?;
        let speech = vad.detect(&audio)?;
        let pieces = chunks(
            &speech,
            MAX_CHUNK_SECS * RATE as usize,
            (JOIN_GAP_SECS * RATE as f32) as usize,
        );
        work.push((track, audio, pieces));
    }
    let total: usize = work
        .iter()
        .flat_map(|(_, _, p)| p.iter().map(|r| r.len()))
        .sum::<usize>()
        .max(1);
    progress(0.0);

    let mut opts = Options {
        prompt: input.prompt.clone(),
        language: input.language.clone(),
        fast: false,
    };
    let mut done = 0usize;
    let mut lines = Vec::new();
    for (track, audio, pieces) in &work {
        for piece in pieces {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let out = engine.transcribe(&audio[piece.clone()], &opts)?;
            // Keep the first detected language for the rest of the meeting.
            if opts.language.is_none() {
                opts.language = out.language;
            }
            let offset = (piece.start * 1000 / RATE as usize) as i64;
            let chunk_end = (piece.end * 1000 / RATE as usize) as i64;
            for r in out.segments {
                lines.push(TranscriptLine {
                    start_ms: offset + r.start_ms,
                    end_ms: (offset + r.end_ms).min(chunk_end),
                    track: *track,
                    text: r.text,
                });
            }
            done += piece.len();
            progress(done as f32 / total as f32);
        }
    }
    lines.sort_by_key(|l| l.start_ms);
    let file = TranscriptFile {
        language: opts.language,
        lines,
    };
    let path = dir.join(TRANSCRIPT_FILE);
    let json = serde_json::to_string_pretty(&file).map_err(|e| Error::Engine(e.to_string()))?;
    std::fs::write(&path, json).map_err(|e| Error::io(&path, e))?;
    progress(1.0);
    Ok(segments(&file, input.me))
}

/// Store segments from a transcript file: mic lines are the user, Teams lines wait for
/// diarization.
pub fn segments(file: &TranscriptFile, me: Option<i64>) -> Vec<NewSegment> {
    let (mut mic, mut remote) = (Vec::new(), Vec::new());
    for l in &file.lines {
        let seg = NewSegment {
            start_ms: l.start_ms,
            end_ms: l.end_ms,
            person_id: if l.track == Track::Mic { me } else { None },
            speaker_label: track_label(l.track).to_owned(),
            text: l.text.clone(),
            low_confidence: false,
        };
        match l.track {
            Track::Mic => mic.push(seg),
            Track::Remote => remote.push(seg),
        }
    }
    merge_tracks(mic, remote)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use switchscribe_capture::{Backend, Recorder};

    use super::*;
    use crate::vad::EnergyVad;
    use crate::{Recognized, Transcribed};

    /// Reports each chunk it is given as one segment spanning it.
    struct EchoEngine {
        calls: Mutex<Vec<(usize, Option<String>)>>,
    }

    impl SpeechEngine for EchoEngine {
        fn transcribe(&self, audio: &[f32], opts: &Options) -> Result<Transcribed> {
            let mut calls = self.calls.lock().unwrap();
            calls.push((audio.len(), opts.language.clone()));
            Ok(Transcribed {
                segments: vec![Recognized {
                    start_ms: 0,
                    end_ms: (audio.len() * 1000 / 16_000) as i64,
                    text: format!("chunk {}", calls.len()),
                }],
                language: Some("en".into()),
            })
        }
    }

    #[test]
    fn transcribes_both_tracks_and_merges_them() {
        let dir = tempfile::tempdir().unwrap();
        // Six seconds of the two synthetic voices: Near talks 0–4 s, Far from 4 s.
        let rec = Recorder::start(dir.path(), Backend::fake()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(6000));
        rec.stop().unwrap();

        let engine = EchoEngine {
            calls: Mutex::new(Vec::new()),
        };
        let mut seen = Vec::new();
        let segs = transcribe_meeting(
            dir.path(),
            &engine,
            &mut EnergyVad::default(),
            &JobInput {
                me: Some(7),
                ..JobInput::default()
            },
            &mut |p| seen.push(p),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(segs.len(), 2, "{segs:?}");
        assert_eq!(segs[0].person_id, Some(7));
        assert_eq!(segs[0].speaker_label, "You");
        assert!(segs[0].start_ms < 500);
        assert_eq!(segs[1].speaker_label, "Teams");
        assert!(
            (3500..4500).contains(&segs[1].start_ms),
            "{}",
            segs[1].start_ms
        );
        // The language found on the first chunk is reused.
        let calls = engine.calls.lock().unwrap();
        assert_eq!(calls[1].1.as_deref(), Some("en"));
        assert_eq!(seen.last(), Some(&1.0));

        let file: TranscriptFile = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(TRANSCRIPT_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(file.language.as_deref(), Some("en"));
        assert_eq!(segments(&file, Some(7)), segs);
    }

    #[test]
    fn nothing_recorded_is_an_error_and_cancel_stops() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EchoEngine {
            calls: Mutex::new(Vec::new()),
        };
        let run = |cancel: bool| {
            transcribe_meeting(
                dir.path(),
                &engine,
                &mut EnergyVad::default(),
                &JobInput::default(),
                &mut |_| {},
                &AtomicBool::new(cancel),
            )
        };
        assert!(matches!(run(false), Err(Error::NoAudio)));
        let rec = Recorder::start(dir.path(), Backend::fake()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        rec.stop().unwrap();
        assert!(matches!(run(true), Err(Error::Cancelled)));
    }
}
