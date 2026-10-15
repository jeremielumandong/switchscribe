//! The model manager: the whisper.cpp models switchscribe offers, where they live, and
//! downloading them with a SHA-256 check.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// Where whisper.cpp's ggml models are published.
const WHISPER_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
/// Where the ggml Silero VAD model is published.
const VAD_BASE: &str = "https://huggingface.co/ggml-org/whisper-vad/resolve/main";

/// A downloadable model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// Saved in settings (`large-v3-turbo-q5`).
    pub id: &'static str,
    /// File name in the models folder.
    pub file: &'static str,
    /// Name shown in Settings.
    pub label: &'static str,
    /// Download size, MB.
    pub size_mb: u32,
    /// Speed and quality in a few words.
    pub note: &'static str,
    base: &'static str,
}

impl ModelInfo {
    /// Download URL.
    pub fn url(&self) -> String {
        format!("{}/{}", self.base, self.file)
    }

    /// Path in a models folder.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(self.file)
    }

    /// Whether it is in `dir` (a finished download; partial ones end in `.part`).
    pub fn is_installed(&self, dir: &Path) -> bool {
        std::fs::metadata(self.path(dir)).is_ok_and(|m| m.is_file() && m.len() > 0)
    }
}

/// Transcription models, smallest first. All multilingual, so the language can be
/// detected per meeting.
pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "base",
        file: "ggml-base.bin",
        label: "Base",
        size_mb: 148,
        note: "Fast on any CPU; rough with names and accents",
        base: WHISPER_BASE,
    },
    ModelInfo {
        id: "small",
        file: "ggml-small.bin",
        label: "Small",
        size_mb: 488,
        note: "Good on CPU; about real time on a laptop",
        base: WHISPER_BASE,
    },
    ModelInfo {
        id: "large-v3-turbo-q5",
        file: "ggml-large-v3-turbo-q5_0.bin",
        label: "Large v3 Turbo (quantized)",
        size_mb: 574,
        note: "Best accuracy for its size; wants a GPU",
        base: WHISPER_BASE,
    },
    ModelInfo {
        id: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        label: "Large v3 Turbo",
        size_mb: 1624,
        note: "Most accurate; GPU strongly recommended",
        base: WHISPER_BASE,
    },
];

/// The voice activity model, fetched alongside the first transcription model.
pub const VAD_MODEL: ModelInfo = ModelInfo {
    id: "silero-vad",
    file: "ggml-silero-v5.1.2.bin",
    label: "Silero VAD",
    size_mb: 1,
    note: "Finds speech so silence is skipped",
    base: VAD_BASE,
};

/// A model by id.
pub fn find(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

/// The model to suggest: the quantized large model with a GPU, Small without.
pub fn recommended(gpu: bool) -> &'static ModelInfo {
    let id = if gpu { "large-v3-turbo-q5" } else { "small" };
    find(id).unwrap_or(&MODELS[0])
}

/// Hex SHA-256 from the hub's `X-Linked-ETag` (`"<hex>"`), if it carries one.
fn parse_etag(value: &str) -> Option<String> {
    let v = value.trim().trim_start_matches("W/").trim_matches('"');
    (v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())).then(|| v.to_ascii_lowercase())
}

/// Ask the hub for the file's SHA-256 without following the redirect to the CDN (the
/// header lives on the redirect).
fn expected_sha256(url: &str) -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let resp = agent.head(url).call().ok()?;
    let header = resp.headers().get("x-linked-etag")?.to_str().ok()?;
    parse_etag(header)
}

/// Download `model` into `dir`, reporting `(bytes so far, total if known)`. Written to a
/// `.part` file and renamed once the SHA-256 matches, so a finished file is always whole.
pub fn download(
    model: &ModelInfo,
    dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let url = model.url();
    let expected = expected_sha256(&url);
    if expected.is_none() {
        tracing::warn!(%url, "no checksum offered; the download is not verified");
    }
    let mut resp = ureq::get(&url)
        .call()
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let part = dir.join(format!("{}.part", model.file));
    let mut out = File::create(&part).map_err(|e| Error::io(&part, e))?;
    let mut body = resp.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(out);
            let _ = std::fs::remove_file(&part);
            return Err(Error::Cancelled);
        }
        let n = body
            .read(&mut buf)
            .map_err(|e| Error::Download(format!("{url}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| Error::io(&part, e))?;
        done += n as u64;
        progress(done, total);
    }
    out.flush().map_err(|e| Error::io(&part, e))?;
    drop(out);
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if let Some(expected) = expected
        && expected != got
    {
        let _ = std::fs::remove_file(&part);
        return Err(Error::Verify { expected, got });
    }
    let path = model.path(dir);
    std::fs::rename(&part, &path).map_err(|e| Error::io(&path, e))?;
    tracing::info!(model = model.id, bytes = done, "model downloaded");
    Ok(path)
}

/// Delete an installed model.
pub fn remove(model: &ModelInfo, dir: &Path) -> Result<()> {
    let path = model.path(dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(&path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        assert!(MODELS.windows(2).all(|w| w[0].size_mb < w[1].size_mb));
        assert_eq!(recommended(false).id, "small");
        assert_eq!(recommended(true).id, "large-v3-turbo-q5");
        assert!(find("nope").is_none());
        assert!(
            VAD_MODEL
                .url()
                .ends_with("/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin")
        );
    }

    #[test]
    fn etags() {
        let hex = "a".repeat(64);
        assert_eq!(parse_etag(&format!("\"{hex}\"")), Some(hex.clone()));
        assert_eq!(parse_etag("\"abc\""), None);
        assert_eq!(parse_etag(&format!("W/\"{}\"", hex.to_uppercase())), Some(hex));
    }

    #[test]
    fn installed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let m = &MODELS[0];
        assert!(!m.is_installed(dir.path()));
        std::fs::write(m.path(dir.path()), b"ggml").unwrap();
        assert!(m.is_installed(dir.path()));
        remove(m, dir.path()).unwrap();
        assert!(!m.is_installed(dir.path()));
        remove(m, dir.path()).unwrap();
    }
}
