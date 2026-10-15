//! Voice activity detection: where in a 16 kHz track someone is talking, so silence is
//! never sent to the speech model (it is slow there and invents words).

use std::ops::Range;

use crate::Result;
use crate::resample::RATE;

/// Finds speech in a 16 kHz track.
pub trait Vad: Send {
    /// Speech regions, as sample ranges, in order.
    fn detect(&mut self, audio: &[f32]) -> Result<Vec<Range<usize>>>;
}

/// A level-based detector: 30 ms frames louder than the track's noise floor by a margin.
/// The fallback when the Silero model is not installed, and the one tests use.
#[derive(Clone, Debug)]
pub struct EnergyVad {
    /// Frames must be this many dB above the noise floor.
    pub margin_db: f32,
    /// ...and never quieter than this.
    pub min_db: f32,
    /// A frame this loud is always speech.
    pub max_db: f32,
    /// Speech shorter than this is dropped, milliseconds.
    pub min_speech_ms: u32,
    /// Gaps shorter than this are bridged, milliseconds.
    pub min_silence_ms: u32,
    /// Padding added on both sides, milliseconds.
    pub pad_ms: u32,
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self {
            margin_db: 12.0,
            min_db: -50.0,
            max_db: -40.0,
            min_speech_ms: 250,
            min_silence_ms: 500,
            pad_ms: 200,
        }
    }
}

const FRAME: usize = (RATE as usize) * 30 / 1000;

fn ms(n: u32) -> usize {
    RATE as usize * n as usize / 1000
}

impl Vad for EnergyVad {
    fn detect(&mut self, audio: &[f32]) -> Result<Vec<Range<usize>>> {
        let db: Vec<f32> = audio
            .chunks(FRAME)
            .map(|f| {
                let rms = (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt();
                20.0 * rms.max(1e-9).log10()
            })
            .collect();
        if db.is_empty() {
            return Ok(Vec::new());
        }
        // The noise floor: the 10th percentile frame.
        let mut sorted = db.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 10];
        // Clamped, so a track that is all speech (no quiet frames) still counts as speech.
        let threshold = (floor + self.margin_db).clamp(self.min_db, self.max_db);

        let mut regions: Vec<Range<usize>> = Vec::new();
        for (i, &level) in db.iter().enumerate() {
            if level < threshold {
                continue;
            }
            let frame = i * FRAME..((i + 1) * FRAME).min(audio.len());
            match regions.last_mut() {
                Some(last) if frame.start - last.end <= ms(self.min_silence_ms) => {
                    last.end = frame.end;
                }
                _ => regions.push(frame),
            }
        }
        let pad = ms(self.pad_ms);
        Ok(regions
            .into_iter()
            .filter(|r| r.len() >= ms(self.min_speech_ms))
            .map(|r| r.start.saturating_sub(pad)..(r.end + pad).min(audio.len()))
            .fold(Vec::new(), |mut acc: Vec<Range<usize>>, r| {
                match acc.last_mut() {
                    Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                    _ => acc.push(r),
                }
                acc
            }))
    }
}

/// Group speech regions into model-sized chunks: neighbours closer than `join_gap`
/// samples are joined while the chunk stays under `max_len`; longer regions are split.
pub fn chunks(speech: &[Range<usize>], max_len: usize, join_gap: usize) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in speech {
        let mut start = r.start;
        while start < r.end {
            let end = (start + max_len).min(r.end);
            match out.last_mut() {
                Some(last) if start - last.end <= join_gap && end - last.start <= max_len => {
                    last.end = end;
                }
                _ => out.push(start..end),
            }
            start = end;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noise at -60 dB with bursts of "speech" at -20 dB.
    fn track(bursts: &[(f32, f32)], secs: f32) -> Vec<f32> {
        let n = (secs * RATE as f32) as usize;
        let mut seed = 1u32;
        (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let noise = ((seed >> 16) as f32 / 32_768.0 - 1.0) * 0.001;
                let t = i as f32 / RATE as f32;
                let on = bursts.iter().any(|&(a, b)| t >= a && t < b);
                noise + if on { 0.1 * (t * 900.0).sin() } else { 0.0 }
            })
            .collect()
    }

    #[test]
    fn finds_bursts_and_bridges_short_gaps() {
        let audio = track(&[(1.0, 2.0), (2.3, 3.0), (6.0, 7.5), (9.0, 9.1)], 10.0);
        let found = EnergyVad::default().detect(&audio).unwrap();
        let secs: Vec<(f32, f32)> = found
            .iter()
            .map(|r| (r.start as f32 / 16_000.0, r.end as f32 / 16_000.0))
            .collect();
        // 1.0–3.0 joined (0.3 s gap), 6.0–7.5 alone, the 0.1 s blip dropped.
        assert_eq!(secs.len(), 2, "{secs:?}");
        assert!((secs[0].0 - 0.8).abs() < 0.05 && (secs[0].1 - 3.2).abs() < 0.05);
        assert!((secs[1].0 - 5.8).abs() < 0.05 && (secs[1].1 - 7.7).abs() < 0.05);
    }

    #[test]
    fn a_track_of_only_speech_is_speech() {
        let audio = track(&[(0.0, 2.0)], 2.0);
        let found = EnergyVad::default().detect(&audio).unwrap();
        assert_eq!(found, vec![0..audio.len()]);
    }

    #[test]
    fn silence_has_no_speech() {
        assert!(EnergyVad::default().detect(&vec![0.0; 16_000]).unwrap().is_empty());
        assert!(EnergyVad::default().detect(&[]).unwrap().is_empty());
    }

    #[test]
    fn chunks_join_and_split() {
        let c = chunks(&[0..100, 120..200, 500..1300], 300, 50);
        assert_eq!(c, vec![0..200, 500..800, 800..1100, 1100..1300]);
    }
}
