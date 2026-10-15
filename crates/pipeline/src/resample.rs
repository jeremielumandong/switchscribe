//! 48 kHz tracks down to the 16 kHz mono the speech models take.

/// The speech models' sample rate.
pub const RATE: u32 = 16_000;

/// Taps of the anti-aliasing filter.
const TAPS: usize = 63;

/// A windowed-sinc low-pass at 7 kHz for 48 kHz input (Hamming window, unity gain).
fn lowpass() -> [f32; TAPS] {
    let cutoff = 7_000.0 / 48_000.0;
    let mid = (TAPS / 2) as f32;
    let mut h = [0f32; TAPS];
    for (i, tap) in h.iter_mut().enumerate() {
        let n = i as f32 - mid;
        let sinc = if n == 0.0 {
            2.0 * cutoff
        } else {
            (2.0 * std::f32::consts::PI * cutoff * n).sin() / (std::f32::consts::PI * n)
        };
        let window =
            0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (TAPS - 1) as f32).cos();
        *tap = sinc * window;
    }
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|t| *t /= sum);
    h
}

/// Filter and decimate 48 kHz mono to 16 kHz.
pub fn to_16k(input: &[f32]) -> Vec<f32> {
    let h = lowpass();
    let half = TAPS / 2;
    let mut out = Vec::with_capacity(input.len() / 3 + 1);
    let mut i = 0;
    while i < input.len() {
        let mut acc = 0.0;
        for (k, tap) in h.iter().enumerate() {
            let j = i as isize + k as isize - half as isize;
            if j >= 0 && (j as usize) < input.len() {
                acc += tap * input[j as usize];
            }
        }
        out.push(acc);
        i += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / 48_000.0).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn keeps_speech_and_drops_what_would_alias() {
        let speech = to_16k(&tone(1_000.0, 48_000));
        assert_eq!(speech.len(), 16_000);
        assert!((rms(&speech[100..15_900]) - 0.707).abs() < 0.03);
        let high = to_16k(&tone(12_000.0, 48_000));
        assert!(rms(&high[100..15_900]) < 0.02);
    }
}
