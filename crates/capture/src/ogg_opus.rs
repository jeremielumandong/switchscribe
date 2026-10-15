//! Mono Opus tracks in an Ogg container (RFC 7845), written as audio arrives and read
//! back whole for the pipeline.
//!
//! Pages are closed about once a second and the file is flushed with them, so a crash
//! mid-call loses at most the last second of audio.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use ogg::writing::PacketWriteEndInfo;
use ogg::{PacketReader, PacketWriter};

use crate::audio::SAMPLE_RATE;
use crate::{Error, Result, io_err};

/// Samples per Opus frame: 20 ms at 48 kHz.
pub const FRAME: usize = 960;
/// Packets per Ogg page: one page per second of audio.
const PACKETS_PER_PAGE: u32 = 50;
/// Bitrate for speech. 32 kbit/s mono Opus is transparent for voice and costs about
/// 14 MB per track-hour.
const BITRATE: i32 = 32_000;
/// The largest packet libopus will produce for one frame.
const MAX_PACKET: usize = 4000;
/// Serial number of the single logical stream.
const SERIAL: u32 = 0x5357_5343; // "SWSC"

/// Writes a mono 48 kHz Ogg Opus file.
pub struct OpusWriter<W: Write> {
    encoder: opus::Encoder,
    packets: PacketWriter<'static, W>,
    pending: Vec<f32>,
    pre_skip: u64,
    /// Real (unpadded) samples pushed so far.
    samples: u64,
    /// Samples encoded so far, padding included.
    encoded: u64,
    in_page: u32,
    out: Vec<u8>,
}

impl OpusWriter<BufWriter<File>> {
    /// Create `path` and write the stream headers.
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|e| io_err(path, e))?;
        Self::new(BufWriter::new(file))
    }
}

impl<W: Write> OpusWriter<W> {
    /// Start a stream on `out`, writing the identification and comment headers.
    pub fn new(out: W) -> Result<Self> {
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        let pre_skip = u64::try_from(encoder.get_lookahead()?).unwrap_or(0);
        let mut packets = PacketWriter::new(out);

        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
        head.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // mapping family: mono/stereo
        packets.write_packet(head, SERIAL, PacketWriteEndInfo::EndPage, 0)?;

        let vendor = concat!("switchscribe ", env!("CARGO_PKG_VERSION"));
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments
        packets.write_packet(tags, SERIAL, PacketWriteEndInfo::EndPage, 0)?;

        Ok(Self {
            encoder,
            packets,
            pending: Vec::with_capacity(FRAME * 2),
            pre_skip,
            samples: 0,
            encoded: 0,
            in_page: 0,
            out: vec![0; MAX_PACKET],
        })
    }

    /// Real samples written so far (48 kHz).
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Append mono 48 kHz samples.
    pub fn push(&mut self, samples: &[f32]) -> Result<()> {
        self.samples += samples.len() as u64;
        self.pending.extend_from_slice(samples);
        let mut start = 0;
        while self.pending.len() - start >= FRAME {
            let frame: Vec<f32> = self.pending[start..start + FRAME].to_vec();
            self.encode(&frame, false)?;
            start += FRAME;
        }
        self.pending.drain(..start);
        Ok(())
    }

    /// Append `n` samples of silence.
    pub fn push_silence(&mut self, n: usize) -> Result<()> {
        // In frame-sized pieces so a long gap does not allocate its whole length.
        let zeros = [0f32; FRAME];
        let mut left = n;
        while left > 0 {
            let take = left.min(FRAME);
            self.push(&zeros[..take])?;
            left -= take;
        }
        Ok(())
    }

    fn encode(&mut self, frame: &[f32], last: bool) -> Result<()> {
        let len = self.encoder.encode_float(frame, &mut self.out)?;
        self.encoded += FRAME as u64;
        self.in_page += 1;
        // The final granule position marks where real audio ends, trimming the padding.
        let granule = if last {
            self.pre_skip + self.samples
        } else {
            self.pre_skip + self.encoded
        };
        let end = if last {
            PacketWriteEndInfo::EndStream
        } else if self.in_page >= PACKETS_PER_PAGE {
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        let closes_page = end != PacketWriteEndInfo::NormalPacket;
        self.packets
            .write_packet(self.out[..len].to_vec(), SERIAL, end, granule)?;
        if closes_page {
            self.in_page = 0;
            self.packets.inner_mut().flush()?;
        }
        Ok(())
    }

    /// Encode what is left (padding the last frame, plus enough to flush the encoder's
    /// lookahead), end the stream and return the writer.
    pub fn finish(mut self) -> Result<W> {
        let mut tail = std::mem::take(&mut self.pending);
        // Push the lookahead out of the encoder too, so the final samples decode.
        let needed = tail.len() + self.pre_skip as usize;
        let frames = needed.div_ceil(FRAME).max(1);
        tail.resize(frames * FRAME, 0.0);
        for (i, frame) in tail.as_chunks::<FRAME>().0.iter().enumerate() {
            self.encode(frame, i + 1 == frames)?;
        }
        let mut out = self.packets.into_inner();
        out.flush()?;
        Ok(out)
    }
}

/// Decode a whole file to mono samples at 48 kHz, with the encoder delay trimmed.
pub fn decode_file(path: &Path) -> Result<Vec<f32>> {
    decode_file_at(path, SAMPLE_RATE)
}

/// Decode a whole file to mono samples at `rate` (8, 12, 16, 24 or 48 kHz; Opus decodes
/// natively at each), with the encoder delay trimmed.
pub fn decode_file_at(path: &Path, rate: u32) -> Result<Vec<f32>> {
    let mut out = Vec::new();
    decode_stream(path, rate, &mut |chunk| out.extend_from_slice(chunk))?;
    Ok(out)
}

/// Decode a file to mono samples at `rate`, handing them to `sink` a packet at a time,
/// so a long meeting never has to sit in memory at full rate. Returns the sample count.
pub fn decode_stream(path: &Path, rate: u32, sink: &mut dyn FnMut(&[f32])) -> Result<usize> {
    let file = File::open(path).map_err(|e| io_err(path, e))?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let head = reader
        .read_packet()?
        .ok_or_else(|| bad(path, "empty file"))?;
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        return Err(bad(path, "not an Opus stream"));
    }
    let channels = match head.data[9] {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => return Err(bad(path, "unsupported channel count")),
    };
    let ch = channels as usize;
    // Granule positions and pre-skip count 48 kHz samples whatever the decode rate.
    let scale = |n: u64| (n * u64::from(rate) / u64::from(SAMPLE_RATE)) as usize;
    let pre_skip = scale(u64::from(u16::from_le_bytes([
        head.data[10],
        head.data[11],
    ])));
    // Comment header.
    reader
        .read_packet()?
        .ok_or_else(|| bad(path, "missing comment header"))?;

    let mut decoder = opus::Decoder::new(rate, channels)?;
    let mut buf = vec![0f32; 5760 * ch];
    // The last packet is held back: only the final granule says how much of it is real.
    let mut held: Vec<f32> = Vec::new();
    let mut out = Trimmed {
        to_skip: pre_skip,
        emitted: 0,
        sink,
    };
    let mut last_granule = None;
    while let Some(packet) = reader.read_packet()? {
        let n = decoder.decode_float(&packet.data, &mut buf, false)?;
        out.emit(&held);
        held.clear();
        if ch == 1 {
            held.extend_from_slice(&buf[..n]);
        } else {
            held.extend(crate::audio::downmix(&buf[..n * ch], 2));
        }
        if packet.last_in_page() {
            last_granule = Some(packet.absgp_page());
        }
    }
    if let Some(g) = last_granule {
        // Samples the stream really holds, counted from the start including pre-skip.
        let before = pre_skip - out.to_skip + out.emitted;
        held.truncate(scale(g).saturating_sub(before));
    }
    out.emit(&held);
    let emitted = out.emitted;
    Ok(emitted)
}

/// Drops the encoder delay from the front of a decoded stream and passes on the rest.
struct Trimmed<'a> {
    to_skip: usize,
    emitted: usize,
    sink: &'a mut dyn FnMut(&[f32]),
}

impl Trimmed<'_> {
    fn emit(&mut self, chunk: &[f32]) {
        let skip = self.to_skip.min(chunk.len());
        self.to_skip -= skip;
        let rest = &chunk[skip..];
        if !rest.is_empty() {
            (self.sink)(rest);
            self.emitted += rest.len();
        }
    }
}

fn bad(path: &Path, what: &str) -> Error {
    Error::Format {
        path: PathBuf::from(path),
        what: what.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, hz: f32) -> Vec<f32> {
        (0..n)
            .map(|i| 0.3 * (i as f32 * hz * std::f32::consts::TAU / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn round_trip_keeps_length_and_signal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.opus");
        let input = tone(SAMPLE_RATE as usize + 123, 440.0);
        let mut w = OpusWriter::create(&path).unwrap();
        for chunk in input.chunks(517) {
            w.push(chunk).unwrap();
        }
        assert_eq!(w.samples(), input.len() as u64);
        w.finish().unwrap();

        let out = decode_file(&path).unwrap();
        assert_eq!(out.len(), input.len());
        // Lossy, but the level survives: RMS of a 0.3 sine is about 0.212.
        let level = crate::audio::rms(&out[4800..out.len() - 4800]);
        assert!((level - 0.212).abs() < 0.03, "{level}");

        // Decoding at a lower rate gives the same length in time, streamed or not.
        for rate in [16_000, 24_000] {
            let low = decode_file_at(&path, rate).unwrap();
            let expect = input.len() * rate as usize / SAMPLE_RATE as usize;
            assert!(
                low.len().abs_diff(expect) <= 1,
                "{rate}: {} vs {expect}",
                low.len()
            );
            let level = crate::audio::rms(&low[low.len() / 10..low.len() * 9 / 10]);
            assert!((level - 0.212).abs() < 0.03, "{rate}: {level}");
            let mut chunks = 0;
            let n = decode_stream(&path, rate, &mut |_| chunks += 1).unwrap();
            assert_eq!(n, low.len());
            assert!(chunks > 10);
        }
    }

    #[test]
    fn silence_and_empty_streams() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.opus");
        let mut w = OpusWriter::create(&path).unwrap();
        w.push_silence(FRAME * 3 + 10).unwrap();
        w.finish().unwrap();
        let out = decode_file(&path).unwrap();
        assert_eq!(out.len(), FRAME * 3 + 10);
        assert!(crate::audio::rms(&out) < 1e-3);

        let empty = dir.path().join("e.opus");
        OpusWriter::create(&empty).unwrap().finish().unwrap();
        assert!(decode_file(&empty).unwrap().is_empty());
    }

    #[test]
    fn rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.opus");
        std::fs::write(&path, b"not ogg at all").unwrap();
        assert!(decode_file(&path).is_err());
    }
}
