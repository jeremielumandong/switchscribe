//! whisper.cpp through whisper-rs: the post-call speech engine and the Silero VAD.

use std::ops::Range;
use std::path::Path;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperVadContextParams, WhisperVadParams,
};

use crate::vad::Vad;
use crate::{Error, Options, Recognized, Result, SpeechEngine, Transcribed};

fn engine_err(e: impl std::fmt::Display) -> Error {
    Error::Engine(e.to_string())
}

fn threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8) as i32)
        .unwrap_or(4)
}

/// A loaded Whisper model.
pub struct WhisperEngine {
    ctx: WhisperContext,
}

impl WhisperEngine {
    /// Load a ggml model. `use_gpu` only matters in builds with a GPU backend.
    pub fn load(model: &Path, use_gpu: bool) -> Result<Self> {
        let path = model
            .to_str()
            .ok_or_else(|| Error::Model(format!("{}: not a UTF-8 path", model.display())))?;
        let mut params = WhisperContextParameters::default();
        params.use_gpu(use_gpu);
        let ctx = WhisperContext::new_with_params(path, params)
            .map_err(|e| Error::Model(format!("{}: {e}", model.display())))?;
        Ok(Self { ctx })
    }
}

/// Lines Whisper emits for non-speech, which never belong in a transcript.
fn is_noise(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
        || (t.starts_with('*') && t.ends_with('*'))
}

impl SpeechEngine for WhisperEngine {
    fn transcribe(&self, audio: &[f32], opts: &Options) -> Result<Transcribed> {
        let mut state = self.ctx.create_state().map_err(engine_err)?;
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        params.set_n_threads(threads());
        params.set_language(Some(opts.language.as_deref().unwrap_or("auto")));
        if !opts.prompt.is_empty() {
            params.set_initial_prompt(&opts.prompt);
        }
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        // Chunks are independent; carrying context across them breeds repetition.
        params.set_no_context(true);
        state.full(params, audio).map_err(engine_err)?;

        let language = whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_owned);
        let mut segments = Vec::new();
        for seg in state.as_iter() {
            let text = seg.to_str_lossy().map_err(engine_err)?.trim().to_owned();
            if is_noise(&text) {
                continue;
            }
            segments.push(Recognized {
                // Centiseconds.
                start_ms: seg.start_timestamp() * 10,
                end_ms: seg.end_timestamp() * 10,
                text,
            });
        }
        Ok(Transcribed { segments, language })
    }
}

/// Silero VAD (the ggml build whisper.cpp ships).
pub struct SileroVad {
    ctx: WhisperVadContext,
}

impl SileroVad {
    /// Load the ggml Silero model.
    pub fn load(model: &Path) -> Result<Self> {
        let path = model
            .to_str()
            .ok_or_else(|| Error::Model(format!("{}: not a UTF-8 path", model.display())))?;
        let mut params = WhisperVadContextParams::new();
        params.set_n_threads(threads());
        let ctx = WhisperVadContext::new(path, params)
            .map_err(|e| Error::Model(format!("{}: {e}", model.display())))?;
        Ok(Self { ctx })
    }
}

impl Vad for SileroVad {
    fn detect(&mut self, audio: &[f32]) -> Result<Vec<Range<usize>>> {
        let mut params = WhisperVadParams::new();
        params.set_min_speech_duration(250);
        params.set_min_silence_duration(500);
        params.set_speech_pad(200);
        let segments = self
            .ctx
            .segments_from_samples(params, audio)
            .map_err(engine_err)?;
        // Centiseconds to 16 kHz samples.
        Ok(segments
            .map(|s| {
                let start = (s.start.max(0.0) * 160.0) as usize;
                let end = ((s.end.max(0.0) * 160.0) as usize).min(audio.len());
                start..end
            })
            .filter(|r| r.start < r.end)
            .collect())
    }
}

/// Transcribe the meeting in `dir` with a Whisper model, finding speech with Silero when
/// `vad_model` loads and by level otherwise. See [`crate::transcribe_meeting`].
pub fn transcribe_with_whisper(
    dir: &Path,
    model: &Path,
    vad_model: Option<&Path>,
    use_gpu: bool,
    input: &crate::JobInput,
    progress: &mut dyn FnMut(f32),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<switchscribe_store::NewSegment>> {
    let engine = WhisperEngine::load(model, use_gpu)?;
    let mut vad: Box<dyn Vad> = match vad_model.map(SileroVad::load) {
        Some(Ok(v)) => Box::new(v),
        Some(Err(e)) => {
            tracing::warn!("Silero VAD unavailable, finding speech by level: {e}");
            Box::new(crate::EnergyVad::default())
        }
        None => Box::new(crate::EnergyVad::default()),
    };
    crate::transcribe_meeting(dir, &engine, vad.as_mut(), input, progress, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_speech_markers_are_dropped() {
        assert!(is_noise(" [BLANK_AUDIO]"));
        assert!(is_noise("(keyboard clicking)"));
        assert!(is_noise("*music*"));
        assert!(!is_noise("Let's flip the DNS tomorrow."));
    }

    #[test]
    fn missing_models_fail_cleanly() {
        let missing = Path::new("/nonexistent/ggml-none.bin");
        assert!(matches!(
            WhisperEngine::load(missing, false),
            Err(Error::Model(_))
        ));
    }
}
