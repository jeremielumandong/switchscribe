//! Live captions in the app: which model runs them, the captioner a recording starts, and
//! the caption blocks the live view and the mini recorder show.

use std::collections::HashSet;
use std::path::Path;

use gpui_kit::Context;
use switchscribe_capture::{Recorder, Track};
use switchscribe_pipeline::live::{Caption, Captioner, Event, LoadEngine};
use switchscribe_pipeline::models::{self, ModelInfo};
use switchscribe_pipeline::{Options, Recognized, SpeechEngine, Transcribed};

use crate::recording::Banner;
use crate::workspace::{Screen, Workspace};

/// Captions from one track closer than this join the block before them.
const JOIN_MS: i64 = 4_000;
/// Blocks kept on screen.
const KEEP: usize = 200;

/// Models fast enough for captions, preferred first. The large models are too slow to
/// keep up on a CPU, so they are used only when nothing smaller is installed and the GPU
/// is on.
const LIVE_MODELS: [&str; 2] = ["base", "small"];

/// The model live captions use, if one is installed.
pub fn live_model(installed: &HashSet<&'static str>, use_gpu: bool) -> Option<&'static ModelInfo> {
    let large = ["large-v3-turbo-q5", "large-v3-turbo"];
    LIVE_MODELS
        .iter()
        .chain(large.iter().filter(|_| use_gpu))
        .find(|id| installed.contains(**id))
        .and_then(|id| models::find(id))
}

/// Consecutive captions from one track.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// The track (the mic is the user).
    pub track: Track,
    /// Start, milliseconds into the recording.
    pub start_ms: i64,
    /// End of the last caption in it.
    pub end_ms: i64,
    /// The text so far.
    pub text: String,
}

/// A recording's captions.
pub struct Live {
    captioner: Captioner,
    /// What was said, oldest first.
    pub blocks: Vec<Block>,
}

impl Live {
    /// Add a caption.
    fn add(&mut self, c: Caption) {
        add_caption(&mut self.blocks, c);
        if self.blocks.len() > KEEP {
            self.blocks.drain(..self.blocks.len() - KEEP);
        }
    }

    /// The latest line, for the mini recorder.
    pub fn last(&self) -> Option<&Block> {
        self.blocks.last()
    }
}

fn add_caption(blocks: &mut Vec<Block>, c: Caption) {
    match blocks.last_mut() {
        Some(b) if b.track == c.track && c.start_ms - b.end_ms <= JOIN_MS => {
            b.text.push(' ');
            b.text.push_str(&c.text);
            b.end_ms = b.end_ms.max(c.end_ms);
        }
        _ => blocks.push(Block {
            track: c.track,
            start_ms: c.start_ms,
            end_ms: c.end_ms,
            text: c.text,
        }),
    }
}

/// The demo's "model": reads the demo transcript back in order, so captions can be seen
/// with synthetic voices.
struct Script {
    lines: Vec<&'static str>,
    next: std::sync::atomic::AtomicUsize,
}

impl SpeechEngine for Script {
    fn transcribe(&self, audio: &[f32], _: &Options) -> switchscribe_pipeline::Result<Transcribed> {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let text = self.lines[i % self.lines.len().max(1)];
        let ms = (audio.len() as u64 * 1000 / u64::from(switchscribe_pipeline::RATE)) as i64;
        Ok(Transcribed {
            segments: vec![Recognized {
                start_ms: 0,
                end_ms: ms,
                text: text.to_owned(),
            }],
            language: None,
        })
    }
}

impl Workspace {
    /// Start captioning the recording, when Settings asks for it. Returns why not, for a
    /// banner, when no model can run them.
    pub(crate) fn start_captions(&self, recorder: &Recorder) -> Result<Option<Live>, Banner> {
        if !self.settings.models.live_captions {
            return Ok(None);
        }
        let load: LoadEngine = if self.demo || crate::recording::fake_audio_requested() {
            let lines = crate::demo::caption_lines();
            Box::new(move || {
                Ok(Box::new(Script {
                    lines,
                    next: Default::default(),
                }) as Box<dyn SpeechEngine>)
            })
        } else {
            let use_gpu = self.settings.models.use_gpu;
            let Some(model) = live_model(&self.jobs.installed, use_gpu) else {
                return Err(Banner {
                    text: "Live captions need the Base or Small model; this call is still \
                           recorded and transcribed after it."
                        .into(),
                    action: Some(("Download a model", Screen::Settings)),
                });
            };
            let path = model.path(&self.paths.models_dir());
            Box::new(move || load_whisper(&path, use_gpu))
        };
        Ok(Some(Live {
            captioner: Captioner::start(recorder.tap(), load, None),
            blocks: Vec::new(),
        }))
    }

    /// Take new captions; called from the recording tick.
    pub(crate) fn poll_captions(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.recording.as_mut().and_then(|s| s.captions.as_mut()) else {
            return;
        };
        let mut failed = None;
        let mut changed = false;
        for event in live.captioner.take() {
            match event {
                Event::Caption(c) => {
                    live.add(c);
                    changed = true;
                }
                Event::Failed(e) => failed = Some(e),
            }
        }
        if let Some(e) = failed {
            if let Some(s) = self.recording.as_mut() {
                s.captions = None;
            }
            self.banners.push(Banner::plain(format!(
                "Live captions stopped: {e}. The call is still recorded."
            )));
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }
}

fn load_whisper(
    path: &Path,
    use_gpu: bool,
) -> switchscribe_pipeline::Result<Box<dyn SpeechEngine>> {
    Ok(Box::new(
        switchscribe_pipeline::whisper::WhisperEngine::load(path, use_gpu)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(track: Track, start_ms: i64, text: &str) -> Caption {
        Caption {
            track,
            start_ms,
            end_ms: start_ms + 2000,
            text: text.into(),
        }
    }

    #[test]
    fn captions_join_into_blocks_per_speaker() {
        let mut b = Vec::new();
        add_caption(&mut b, cap(Track::Remote, 0, "Two of three"));
        add_caption(&mut b, cap(Track::Remote, 3000, "pools are done."));
        add_caption(&mut b, cap(Track::Mic, 5500, "Great."));
        add_caption(&mut b, cap(Track::Remote, 7000, "GPU is next."));
        add_caption(&mut b, cap(Track::Remote, 20_000, "Anything else?"));
        let texts: Vec<&str> = b.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Two of three pools are done.",
                "Great.",
                "GPU is next.",
                "Anything else?"
            ]
        );
    }

    #[test]
    fn small_models_caption_first() {
        let mut installed: HashSet<&'static str> = ["large-v3-turbo"].into();
        assert_eq!(live_model(&installed, false), None);
        assert_eq!(
            live_model(&installed, true).map(|m| m.id),
            Some("large-v3-turbo")
        );
        installed.insert("small");
        installed.insert("base");
        assert_eq!(live_model(&installed, true).map(|m| m.id), Some("base"));
    }
}
