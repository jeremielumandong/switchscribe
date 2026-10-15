//! Playing a meeting back: both tracks mixed, at 1× to 2× without raising the pitch,
//! optionally skipping long silences. Output goes to the default device through cpal;
//! with no device the player still keeps time, so the transcript stays in sync.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::Resampler;
use crate::stretch::{HOP, Stretch, sample, to_i16};
use crate::{Error, Result, Track};

/// Silences at least this long are skipped, seconds.
const MIN_SILENCE_SECS: f32 = 1.5;
/// Kept on each side of a skipped silence, seconds, so speech never starts abruptly.
const SILENCE_KEEP_SECS: f32 = 0.3;
/// Quieter than this is silence, dBFS.
const SILENCE_DB: f32 = -45.0;
/// Playback decodes at 24 kHz: plenty for speech, half the memory of 48 kHz.
pub const PLAY_RATE: u32 = 24_000;

/// What the UI changes and the audio thread reads.
#[derive(Debug)]
struct State {
    /// Position in source samples (24 kHz).
    pos: f64,
    playing: bool,
    speed: f32,
    skip_silence: bool,
    /// Set by a seek, so the stretcher starts afresh.
    jumped: bool,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// A loaded meeting, ready to play.
pub struct Player {
    state: Arc<Mutex<State>>,
    len: usize,
    silences: Arc<Vec<Range<usize>>>,
    samples: Arc<Vec<i16>>,
    device: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Load the tracks in a meeting folder, mixed, and open the default output device.
    /// Missing tracks are skipped; no tracks at all is an error.
    pub fn open(dir: &Path) -> Result<Player> {
        let mut mixed: Vec<i16> = Vec::new();
        let mut found = false;
        for track in Track::ALL {
            let path = dir.join(track.file_name());
            if !path.exists() {
                continue;
            }
            found = true;
            let mut at = 0usize;
            crate::ogg_opus::decode_stream(&path, PLAY_RATE, &mut |chunk| {
                for &x in chunk {
                    match mixed.get_mut(at) {
                        Some(m) => *m = to_i16(sample(std::slice::from_ref(m), 0) + x),
                        None => mixed.push(to_i16(x)),
                    }
                    at += 1;
                }
            })?;
        }
        if !found {
            return Err(Error::Format {
                path: dir.to_path_buf(),
                what: "no audio tracks".into(),
            });
        }
        Ok(Player::new(mixed, true))
    }

    /// Play mono 24 kHz `samples`; `output: false` keeps time without a device (tests).
    pub fn new(samples: Vec<i16>, output: bool) -> Player {
        let silences = Arc::new(silences(&samples));
        let samples = Arc::new(samples);
        let state = Arc::new(Mutex::new(State {
            pos: 0.0,
            playing: false,
            speed: 1.0,
            skip_silence: false,
            jumped: false,
        }));
        let device = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, samples, silences, device, stop) = (
                state.clone(),
                samples.clone(),
                silences.clone(),
                device.clone(),
                stop.clone(),
            );
            std::thread::Builder::new()
                .name("playback".into())
                .spawn(move || run(output, &state, samples, silences, &device, &stop))
                .map_err(|e| tracing::warn!("no playback thread: {e}"))
                .ok()
        };
        Player {
            state,
            len: samples.len(),
            silences,
            samples,
            device,
            stop,
            thread,
        }
    }

    /// Start or resume. At the end, starts over.
    pub fn play(&self) {
        let mut s = lock(&self.state);
        if s.pos as usize >= self.len {
            s.pos = 0.0;
            s.jumped = true;
        }
        s.playing = true;
    }

    /// Pause.
    pub fn pause(&self) {
        lock(&self.state).playing = false;
    }

    /// Whether it is playing.
    pub fn is_playing(&self) -> bool {
        lock(&self.state).playing
    }

    /// Jump to `ms`.
    pub fn seek_ms(&self, ms: u64) {
        let mut s = lock(&self.state);
        s.pos = (ms as f64 * f64::from(PLAY_RATE) / 1000.0).min(self.len as f64);
        s.jumped = true;
    }

    /// Where playback is, milliseconds.
    pub fn position_ms(&self) -> u64 {
        (lock(&self.state).pos * 1000.0 / f64::from(PLAY_RATE)) as u64
    }

    /// Length, milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.len as u64 * 1000 / u64::from(PLAY_RATE)
    }

    /// Playback speed, 0.5..=3.
    pub fn set_speed(&self, speed: f32) {
        lock(&self.state).speed = speed.clamp(0.5, 3.0);
    }

    /// See [`Player::set_speed`].
    pub fn speed(&self) -> f32 {
        lock(&self.state).speed
    }

    /// Skip silences longer than 1.5 s.
    pub fn set_skip_silence(&self, on: bool) {
        lock(&self.state).skip_silence = on;
    }

    /// See [`Player::set_skip_silence`].
    pub fn skip_silence(&self) -> bool {
        lock(&self.state).skip_silence
    }

    /// The silences that skipping jumps over, milliseconds.
    pub fn silences_ms(&self) -> Vec<Range<u64>> {
        let ms = |s: usize| s as u64 * 1000 / u64::from(PLAY_RATE);
        self.silences
            .iter()
            .map(|r| ms(r.start)..ms(r.end))
            .collect()
    }

    /// The mixed audio's loudness in `bars` equal slices, `0.0..=1.0`, for drawing.
    pub fn envelope(&self, bars: usize) -> Vec<f32> {
        envelope(&self.samples, bars)
    }

    /// The output device's name; `None` while opening or when there is none.
    pub fn device(&self) -> Option<String> {
        self.device.lock().ok().and_then(|d| d.clone())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Loudness of `samples` in `bars` equal slices, scaled from -50..0 dBFS to 0..1.
pub fn envelope(samples: &[i16], bars: usize) -> Vec<f32> {
    if bars == 0 {
        return Vec::new();
    }
    (0..bars)
        .map(|b| {
            let a = samples.len() * b / bars;
            let z = (samples.len() * (b + 1) / bars).max(a);
            let rms = rms16(&samples[a..z]);
            ((20.0 * rms.max(1e-9).log10() + 50.0) / 50.0).clamp(0.0, 1.0)
        })
        .collect()
}

/// Stretches quieter than [`SILENCE_DB`] lasting [`MIN_SILENCE_SECS`] or more, minus
/// [`SILENCE_KEEP_SECS`] on each side.
fn silences(samples: &[i16]) -> Vec<Range<usize>> {
    let frame = PLAY_RATE as usize * 30 / 1000;
    let min = (MIN_SILENCE_SECS * PLAY_RATE as f32) as usize;
    let keep = (SILENCE_KEEP_SECS * PLAY_RATE as f32) as usize;
    let threshold = 10f32.powf(SILENCE_DB / 20.0);
    let mut out = Vec::new();
    let mut run: Option<usize> = None;
    let close = |start: usize, end: usize, out: &mut Vec<Range<usize>>| {
        if end - start >= min {
            out.push(start + keep..end - keep);
        }
    };
    for (i, f) in samples.chunks(frame).enumerate() {
        let quiet = rms16(f) < threshold;
        match (quiet, run) {
            (true, None) => run = Some(i * frame),
            (false, Some(start)) => {
                close(start, i * frame, &mut out);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run {
        close(start, samples.len(), &mut out);
    }
    out
}

/// Root mean square of 16-bit samples, full scale 1.0.
fn rms16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / samples.len() as f64).sqrt() / 32_768.0) as f32
}

/// Where to continue from `pos`: past the silence it falls in, if any.
fn skip(pos: f64, silences: &[Range<usize>]) -> f64 {
    let p = pos as usize;
    silences
        .iter()
        .find(|r| r.contains(&p))
        .map_or(pos, |r| r.end as f64)
}

/// Produces stretched 48 kHz audio from the shared state, converted to the device rate.
struct Renderer {
    samples: Arc<Vec<i16>>,
    silences: Arc<Vec<Range<usize>>>,
    stretch: Stretch,
    resampler: Resampler,
    hop: Vec<f32>,
    ready: Vec<f32>,
    used: usize,
}

impl Renderer {
    fn new(samples: Arc<Vec<i16>>, silences: Arc<Vec<Range<usize>>>, rate: u32) -> Self {
        Self {
            samples,
            silences,
            stretch: Stretch::new(),
            resampler: Resampler::new(PLAY_RATE, rate),
            hop: Vec::with_capacity(HOP),
            ready: Vec::new(),
            used: 0,
        }
    }

    /// Fill `out` with mono samples at the device rate.
    fn fill(&mut self, state: &Mutex<State>, out: &mut [f32]) {
        let mut s = lock(state);
        if s.jumped {
            s.jumped = false;
            self.stretch.reset();
            self.ready.clear();
            self.used = 0;
        }
        for o in out.iter_mut() {
            if self.used >= self.ready.len() {
                self.ready.clear();
                self.used = 0;
                if !s.playing {
                    self.stretch.reset();
                } else if (s.pos as usize) >= self.samples.len() {
                    s.playing = false;
                    s.pos = self.samples.len() as f64;
                    self.stretch.reset();
                } else {
                    if s.skip_silence {
                        let to = skip(s.pos, &self.silences);
                        if to != s.pos {
                            s.pos = to;
                            self.stretch.reset();
                        }
                    }
                    self.hop.clear();
                    self.stretch
                        .next(&self.samples, s.pos as usize, &mut self.hop);
                    s.pos += HOP as f64 * f64::from(s.speed);
                    self.resampler.process(&self.hop, &mut self.ready);
                }
            }
            *o = match self.ready.get(self.used) {
                Some(v) => {
                    self.used += 1;
                    *v
                }
                None => 0.0,
            };
        }
    }
}

/// The playback thread: a cpal stream when a device opens, else a silent clock.
fn run(
    output: bool,
    state: &Arc<Mutex<State>>,
    samples: Arc<Vec<i16>>,
    silences: Arc<Vec<Range<usize>>>,
    device: &Mutex<Option<String>>,
    stop: &AtomicBool,
) {
    let stream = if output {
        match open_stream(state.clone(), samples.clone(), silences.clone()) {
            Ok((stream, name)) => {
                if let Ok(mut d) = device.lock() {
                    *d = Some(name);
                }
                Some(stream)
            }
            Err(e) => {
                tracing::warn!("no audio output, playing silently: {e}");
                None
            }
        }
    } else {
        None
    };
    if stream.is_some() {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        return;
    }
    // No device: render into nothing at real time, so position, speed and skipping
    // behave exactly as they would with sound.
    let rate = 8_000;
    let mut r = Renderer::new(samples, silences, rate);
    let mut buf = vec![0.0f32; rate as usize / 100];
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        r.fill(state, &mut buf);
        next += Duration::from_millis(10);
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        } else {
            next = Instant::now();
        }
    }
}

/// Open the default output device with the renderer feeding it.
fn open_stream(
    state: Arc<Mutex<State>>,
    samples: Arc<Vec<i16>>,
    silences: Arc<Vec<Range<usize>>>,
) -> Result<(cpal::Stream, String)> {
    use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
    use cpal::{FromSample, SampleFormat, SizedSample};

    let dev_err = |e: cpal::Error| Error::Device(e.to_string());
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| Error::Device("no output device".into()))?;
    let name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "Speakers".into());
    let supported = device.default_output_config().map_err(dev_err)?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = usize::from(config.channels.max(1));
    let mut renderer = Renderer::new(samples, silences, config.sample_rate);
    let mut mono = Vec::new();

    fn build<T: SizedSample + FromSample<f32>>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        channels: usize,
        mut render: impl FnMut(&mut [f32]) + Send + 'static,
        mono: &mut Vec<f32>,
    ) -> std::result::Result<cpal::Stream, cpal::Error> {
        let mut mono = std::mem::take(mono);
        device.build_output_stream(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                mono.resize(data.len() / channels, 0.0);
                render(&mut mono);
                for (frame, v) in data.chunks_mut(channels).zip(&mono) {
                    frame.fill(T::from_sample(*v));
                }
            },
            |e| tracing::warn!("playback: {e}"),
            None,
        )
    }

    let render = move |out: &mut [f32]| renderer.fill(&state, out);
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, channels, render, &mut mono),
        SampleFormat::I16 => build::<i16>(&device, &config, channels, render, &mut mono),
        SampleFormat::U16 => build::<u16>(&device, &config, channels, render, &mut mono),
        SampleFormat::I32 => build::<i32>(&device, &config, channels, render, &mut mono),
        other => return Err(Error::Device(format!("unsupported sample format {other}"))),
    }
    .map_err(dev_err)?;
    stream.play().map_err(dev_err)?;
    tracing::info!(device = %name, rate = config.sample_rate, "playback opened");
    Ok((stream, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of tone, `gap` seconds of silence, one second of tone.
    fn speech_gap_speech(gap: f32) -> Vec<i16> {
        let sec = PLAY_RATE as usize;
        let tone = |n: usize| (0..n).map(|i| to_i16((i as f32 * 0.1).sin() * 0.3));
        tone(sec)
            .chain(std::iter::repeat_n(0, (gap * sec as f32) as usize))
            .chain(tone(sec))
            .collect()
    }

    fn wait_until(mut ok: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn finds_long_silences_only() {
        let s = silences(&speech_gap_speech(3.0));
        assert_eq!(s.len(), 1);
        let ms = |x: usize| x * 1000 / PLAY_RATE as usize;
        assert!((1250..1350).contains(&ms(s[0].start)), "{s:?}");
        assert!((3650..3750).contains(&ms(s[0].end)), "{s:?}");
        assert!(silences(&speech_gap_speech(1.0)).is_empty());
        assert_eq!(skip(24_000.0 * 2.0, &s), s[0].end as f64);
        assert_eq!(skip(100.0, &s), 100.0);
    }

    #[test]
    fn mixes_and_draws() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Player::open(dir.path()).is_err());
        let env = envelope(&speech_gap_speech(2.0), 4);
        assert!(env[0] > 0.5 && env[3] > 0.5);
        assert!(env[1] < 0.6 && env[2] < 0.6 && env[1].min(env[2]) == 0.0);
        assert!(envelope(&[], 3).iter().all(|v| *v == 0.0));
    }

    #[test]
    fn keeps_time_without_a_device_and_skips_silence() {
        let p = Player::new(speech_gap_speech(3.0), false);
        assert_eq!(p.duration_ms(), 5000);
        assert!(p.device().is_none());
        p.set_speed(2.0);
        p.set_skip_silence(true);
        p.play();
        // At 2× with the 2.4 s silence skipped, the end comes in well under 2 s.
        let start = Instant::now();
        assert!(wait_until(|| !p.is_playing()));
        let took = start.elapsed();
        assert!(took < Duration::from_millis(1800), "{took:?}");
        assert!(took > Duration::from_millis(900), "{took:?}");
        assert_eq!(p.position_ms(), 5000);

        // Seeking and playing again from the end starts over.
        p.seek_ms(4500);
        assert_eq!(p.position_ms(), 4500);
        p.play();
        assert!(wait_until(|| !p.is_playing()));
        p.play();
        assert!(p.position_ms() < 100);
        p.pause();
    }
}
