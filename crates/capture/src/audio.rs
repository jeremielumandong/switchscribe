//! Sample plumbing shared by every source: blocks, downmixing, resampling and levels.

/// The rate both tracks are stored at. Opus works natively at 48 kHz, and it is what
/// Windows' shared-mode engine runs at on nearly every machine.
pub const SAMPLE_RATE: u32 = 48_000;

/// A block of interleaved `f32` samples as a source delivered it.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// Interleaved samples, nominally in `-1.0..=1.0`.
    pub samples: Vec<f32>,
    /// Channels per frame.
    pub channels: u16,
    /// Frames per second.
    pub rate: u32,
}

impl Block {
    /// Frames in the block.
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }
}

/// Average interleaved channels down to mono.
pub fn downmix(samples: &[f32], channels: u16) -> Vec<f32> {
    let ch = usize::from(channels.max(1));
    if ch == 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// A streaming linear resampler for mono audio. Linear interpolation is plenty for speech
/// headed to an Opus encoder and the speech models; it only runs when a device does not
/// deliver 48 kHz, which shared-mode WASAPI with autoconvert almost never does.
#[derive(Clone, Debug)]
pub struct Resampler {
    step: f64,
    /// Position of the next output sample, relative to `prev` at index 0.
    pos: f64,
    /// The last input sample of the previous call.
    prev: Option<f32>,
}

impl Resampler {
    /// Convert from `from` Hz to `to` Hz.
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            pos: 0.0,
            prev: None,
        }
    }

    /// Resample `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        // Index 0 is the carried sample (or the first input sample on the first call).
        let off = usize::from(self.prev.is_some());
        let prev = self.prev.unwrap_or(0.0);
        let data_len = input.len() + off;
        let at = |i: usize| if i < off { prev } else { input[i - off] };
        let mut t = self.pos;
        while t + 1.0 < data_len as f64 {
            let i = t as usize;
            let frac = (t - i as f64) as f32;
            out.push(at(i) + (at(i + 1) - at(i)) * frac);
            t += self.step;
        }
        self.pos = t - (data_len - 1) as f64;
        self.prev = input.last().copied();
    }
}

/// Root mean square of a block.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Map an RMS value to a meter position in `0.0..=1.0`: -60 dBFS and below is empty,
/// 0 dBFS is full. Speech usually sits between 0.4 and 0.8.
pub fn meter(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_frames() {
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(downmix(&[0.25, 0.5], 1), vec![0.25, 0.5]);
    }

    #[test]
    fn resampler_keeps_duration_across_calls() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        let input = vec![0.5f32; 441];
        for _ in 0..100 {
            r.process(&input, &mut out);
        }
        // One second in, one second out (within a sample).
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn resampler_interpolates() {
        let mut r = Resampler::new(1, 2);
        let mut out = Vec::new();
        r.process(&[0.0, 1.0], &mut out);
        r.process(&[2.0], &mut out);
        assert_eq!(out, vec![0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn same_rate_passes_through() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter(0.0), 0.0);
        assert!((meter(1.0) - 1.0).abs() < 1e-6);
        assert!((meter(0.001) - 0.0).abs() < 1e-6);
        let mid = meter(rms(&[0.1, -0.1, 0.1, -0.1]));
        assert!((mid - 2.0 / 3.0).abs() < 1e-3, "{mid}");
    }
}
