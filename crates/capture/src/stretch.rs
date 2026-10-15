//! Time stretching for faster playback without the chipmunk voice: WSOLA (waveform
//! similarity overlap-add). Each output hop takes a Hann-windowed frame from near the
//! wanted source position, nudged within a few milliseconds so it lines up with the
//! waveform the previous frame left off on.
//!
//! Sources are 16-bit, which halves what a long meeting holds in memory.

/// Frame length, samples (43 ms at 24 kHz).
const N: usize = 1024;
/// Output hop: half a frame, so Hann windows sum to one.
pub(crate) const HOP: usize = N / 2;
/// How far a frame may move to line up, samples (10 ms at 24 kHz).
const SEEK: usize = 240;
/// Correlation looks at every `STRIDE`th sample; plenty for speech.
const STRIDE: usize = 2;

/// A streaming WSOLA stretcher over a mono buffer.
pub(crate) struct Stretch {
    window: Vec<f32>,
    /// The second half of the previous frame, waiting to be overlapped.
    tail: Vec<f32>,
    /// Where the previous frame started in the source.
    prev: Option<usize>,
}

impl Stretch {
    pub(crate) fn new() -> Self {
        // Periodic Hann: overlapping halves sum to exactly one.
        let window = (0..N)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos())
            .collect();
        Self {
            window,
            tail: vec![0.0; HOP],
            prev: None,
        }
    }

    /// Forget the previous frame (after a seek or a pause); the next hop fades in.
    pub(crate) fn reset(&mut self) {
        self.tail.fill(0.0);
        self.prev = None;
    }

    /// Append [`HOP`] output samples taken from `src` around `pos`. The caller then
    /// advances `pos` by `HOP × speed`.
    pub(crate) fn next(&mut self, src: &[i16], pos: usize, out: &mut Vec<f32>) {
        let at = |i: usize| sample(src, i);
        let start = match self.prev {
            // Where the waveform would naturally continue.
            Some(prev) if prev + HOP != pos => best_offset(src, prev + HOP, pos),
            _ => pos,
        };
        for i in 0..HOP {
            out.push(self.tail[i] + at(start + i) * self.window[i]);
        }
        for i in 0..HOP {
            self.tail[i] = at(start + HOP + i) * self.window[HOP + i];
        }
        self.prev = Some(start);
    }
}

/// A 16-bit sample as a float; zero past the end.
pub(crate) fn sample(src: &[i16], i: usize) -> f32 {
    src.get(i).map_or(0.0, |s| f32::from(*s) / 32_768.0)
}

/// A float sample as 16-bit, clipped.
pub(crate) fn to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32_767.0) as i16
}

/// The frame start within `SEEK` of `pos` whose first half best matches the natural
/// continuation at `natural`.
fn best_offset(src: &[i16], natural: usize, pos: usize) -> usize {
    let at = |i: usize| sample(src, i);
    let lo = pos.saturating_sub(SEEK);
    let hi = pos + SEEK;
    let mut best = (pos, f32::MIN);
    for cand in lo..=hi {
        let (mut dot, mut energy) = (0.0f32, 0.0f32);
        for i in (0..HOP).step_by(STRIDE) {
            let x = at(cand + i);
            dot += x * at(natural + i);
            energy += x * x;
        }
        let score = if energy > 0.0 {
            dot / energy.sqrt()
        } else {
            0.0
        };
        if score > best.1 {
            best = (cand, score);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 24_000.0;

    fn tone(len: usize, hz: f32) -> Vec<i16> {
        (0..len)
            .map(|i| to_i16((i as f32 * hz * 2.0 * std::f32::consts::PI / RATE).sin() * 0.5))
            .collect()
    }

    fn render(src: &[i16], speed: f32) -> Vec<f32> {
        let mut s = Stretch::new();
        let mut out = Vec::new();
        let mut pos = 0.0f64;
        while (pos as usize) < src.len() {
            s.next(src, pos as usize, &mut out);
            pos += HOP as f64 * f64::from(speed);
        }
        out
    }

    #[test]
    fn normal_speed_is_the_input_after_the_fade_in() {
        let src = tone(24_000, 220.0);
        let out = render(&src, 1.0);
        for (i, o) in out.iter().enumerate().take(src.len() - N).skip(HOP) {
            assert!((o - sample(&src, i)).abs() < 1e-4, "sample {i}");
        }
    }

    #[test]
    fn double_speed_halves_the_length_and_keeps_the_pitch() {
        let src = tone(48_000, 200.0);
        let out = render(&src, 2.0);
        assert!(
            (out.len() as i64 - 24_000).abs() <= HOP as i64,
            "{}",
            out.len()
        );
        // Count rising zero crossings in the steady middle: about 200 per second.
        let mid = &out[HOP * 2..out.len() - HOP * 2];
        let crossings = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let hz = crossings as f32 / (mid.len() as f32 / RATE);
        assert!((hz - 200.0).abs() < 12.0, "{hz} Hz");
        // No dropouts: the level stays up throughout.
        for c in mid.chunks(240) {
            let rms = (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt();
            assert!(rms > 0.25, "{rms}");
        }
    }
}
