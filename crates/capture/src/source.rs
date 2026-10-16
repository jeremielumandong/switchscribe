//! Where samples come from: the Windows devices, or a synthetic voice for tests and for
//! trying the app on a machine that cannot capture.

use std::time::{Duration, Instant};

use crate::Result;
use crate::audio::Block;

/// A running capture stream. Sources are opened on the thread that reads them (WASAPI
/// objects are bound to their COM apartment), so they need not be `Send`.
pub trait Source {
    /// Wait up to `timeout` for audio. `Ok(None)` means nothing arrived, which is normal:
    /// loopback streams deliver no packets while nothing is playing. An error ends the
    /// track, for example when the device is unplugged.
    fn read(&mut self, timeout: Duration) -> Result<Option<Block>>;
}

/// A source that was opened, and how.
pub struct Opened {
    /// The stream.
    pub source: Box<dyn Source>,
    /// What is being captured, for logs and `capture.json` ("Teams (process 4120)").
    pub description: String,
    /// Set when the preferred source was unavailable and a fallback was opened instead,
    /// with the reason shown to the user.
    pub fallback: Option<String>,
}

/// Opens a track's source on the capture thread.
pub type OpenFn = Box<dyn FnOnce() -> Result<Opened> + Send>;

/// Which voice a [`FakeSource`] speaks with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeVoice {
    /// A lower voice that talks first.
    Near,
    /// A higher voice that answers, delivered as 44.1 kHz stereo to exercise the
    /// downmix and resampler.
    Far,
}

/// A synthetic talker, paced in real time: phrases of a buzzy tone with syllable-rate
/// amplitude changes, separated by pauses, so meters move and the files hold something
/// speech-shaped. Used by tests and `SWITCHSCRIBE_FAKE_AUDIO=1`.
pub struct FakeSource {
    voice: FakeVoice,
    rate: u32,
    channels: u16,
    started: Instant,
    emitted: u64,
    /// Stop after this many frames (tests); `None` runs until the recorder stops.
    limit: Option<u64>,
}

impl FakeSource {
    /// A source that talks until the recorder stops.
    pub fn new(voice: FakeVoice) -> Self {
        let (rate, channels) = match voice {
            FakeVoice::Near => (48_000, 1),
            FakeVoice::Far => (44_100, 2),
        };
        Self {
            voice,
            rate,
            channels,
            started: Instant::now(),
            emitted: 0,
            limit: None,
        }
    }

    /// Fail with a device error after `secs` seconds, like an unplugged headset.
    pub fn failing_after(mut self, secs: f64) -> Self {
        self.limit = Some((secs * f64::from(self.rate)) as u64);
        self
    }

    /// An [`OpenFn`] for this voice.
    pub fn open(voice: FakeVoice) -> OpenFn {
        Box::new(move || {
            Ok(Opened {
                source: Box::new(FakeSource::new(voice)),
                description: format!("synthetic voice ({voice:?})"),
                fallback: None,
            })
        })
    }

    fn sample(&self, frame: u64) -> f32 {
        let t = frame as f64 / f64::from(self.rate);
        // Near talks for 4 s then listens for 3 s; Far is the mirror image.
        let (period, talk, offset, pitch) = match self.voice {
            FakeVoice::Near => (7.0, 4.0, 0.0, 130.0),
            FakeVoice::Far => (7.0, 3.0, 4.0, 210.0),
        };
        let phase = (t - offset).rem_euclid(period);
        if phase >= talk {
            return 0.0;
        }
        let syllable = 0.5 + 0.5 * (t * 4.3 * std::f64::consts::TAU).sin().abs();
        let tone = (t * pitch * std::f64::consts::TAU).sin()
            + 0.4 * (t * pitch * 2.0 * std::f64::consts::TAU).sin()
            + 0.2 * (t * pitch * 3.0 * std::f64::consts::TAU).sin();
        (0.18 * syllable * tone) as f32
    }
}

impl Source for FakeSource {
    fn read(&mut self, timeout: Duration) -> Result<Option<Block>> {
        if let Some(limit) = self.limit
            && self.emitted >= limit
        {
            return Err(crate::Error::Device("the device was disconnected".into()));
        }
        let wait = timeout.min(Duration::from_millis(10));
        std::thread::sleep(wait);
        let due = (self.started.elapsed().as_secs_f64() * f64::from(self.rate)) as u64;
        let mut frames = due.saturating_sub(self.emitted);
        if let Some(limit) = self.limit {
            frames = frames.min(limit - self.emitted);
        }
        if frames == 0 {
            return Ok(None);
        }
        let ch = usize::from(self.channels);
        let mut samples = Vec::with_capacity(frames as usize * ch);
        for f in self.emitted..self.emitted + frames {
            let s = self.sample(f);
            samples.extend(std::iter::repeat_n(s, ch));
        }
        self.emitted += frames;
        Ok(Some(Block {
            samples,
            channels: self.channels,
            rate: self.rate,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_source_paces_in_real_time() {
        let started = Instant::now();
        let mut s = FakeSource::new(FakeVoice::Far);
        std::thread::sleep(Duration::from_millis(50));
        let b = s.read(Duration::from_millis(10)).unwrap().unwrap();
        assert_eq!(b.channels, 2);
        assert_eq!(b.rate, 44_100);
        // What elapsed (at least 50 ms; more on a loaded CI runner), never a burst of
        // seconds ahead of the clock.
        let due = (started.elapsed().as_secs_f64() * 44_100.0) as usize;
        assert!(
            b.frames() > 2000 && b.frames() <= due,
            "{} of {due}",
            b.frames()
        );
    }

    #[test]
    fn failing_source_errors_after_its_limit() {
        let mut s = FakeSource::new(FakeVoice::Near).failing_after(0.01);
        let mut got = 0;
        loop {
            match s.read(Duration::from_millis(5)) {
                Ok(Some(b)) => got += b.frames(),
                Ok(None) => {}
                Err(_) => break,
            }
        }
        assert_eq!(got, 480);
    }
}
