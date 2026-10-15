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
/// sherpa-onnx's speaker segmentation models.
const SEGMENTATION_BASE: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models";
/// sherpa-onnx's speaker embedding models (the release tag is spelled this way).
const EMBEDDING_BASE: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models";

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
    /// The published file's name, when it differs from [`ModelInfo::file`].
    remote: Option<&'static str>,
    /// The file inside a `.tar.bz2` download to keep.
    extract: Option<&'static str>,
    /// Known SHA-256 of the download; otherwise the host's `X-Linked-ETag` is used.
    sha256: Option<&'static str>,
}

impl ModelInfo {
    /// Download URL.
    pub fn url(&self) -> String {
        format!("{}/{}", self.base, self.remote.unwrap_or(self.file))
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
        remote: None,
        extract: None,
        sha256: None,
    },
    ModelInfo {
        id: "small",
        file: "ggml-small.bin",
        label: "Small",
        size_mb: 488,
        note: "Good on CPU; about real time on a laptop",
        base: WHISPER_BASE,
        remote: None,
        extract: None,
        sha256: None,
    },
    ModelInfo {
        id: "large-v3-turbo-q5",
        file: "ggml-large-v3-turbo-q5_0.bin",
        label: "Large v3 Turbo (quantized)",
        size_mb: 574,
        note: "Best accuracy for its size; wants a GPU",
        base: WHISPER_BASE,
        remote: None,
        extract: None,
        sha256: None,
    },
    ModelInfo {
        id: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        label: "Large v3 Turbo",
        size_mb: 1624,
        note: "Most accurate; GPU strongly recommended",
        base: WHISPER_BASE,
        remote: None,
        extract: None,
        sha256: None,
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
    remote: None,
    extract: None,
    sha256: None,
};

/// Speaker segmentation (pyannote 3.0, ONNX), for diarizing the Teams track.
pub const SEGMENTATION_MODEL: ModelInfo = ModelInfo {
    id: "pyannote-segmentation-3",
    file: "pyannote-segmentation-3.0.onnx",
    label: "Speaker segmentation",
    size_mb: 7,
    note: "Finds where one speaker hands over to another",
    base: SEGMENTATION_BASE,
    remote: Some("sherpa-onnx-pyannote-segmentation-3-0.tar.bz2"),
    extract: Some("model.onnx"),
    sha256: Some("24615ee884c897d9d2ba09bb4d30da6bb1b15e685065962db5b02e76e4996488"),
};

/// Speaker embeddings (NVIDIA NeMo TitaNet-small), for clustering and voiceprints.
pub const EMBEDDING_MODEL: ModelInfo = ModelInfo {
    id: "titanet-small",
    file: "nemo_en_titanet_small.onnx",
    label: "Voiceprints",
    size_mb: 40,
    note: "Tells voices apart and recognizes them next time",
    base: EMBEDDING_BASE,
    remote: None,
    extract: None,
    sha256: Some("ad4a1802485d8b34c722d2a9d04249662f2ece5d28a7a039063ca22f515a789e"),
};

/// The models fetched alongside the first transcription model.
pub const SUPPORT_MODELS: [ModelInfo; 3] = [VAD_MODEL, SEGMENTATION_MODEL, EMBEDDING_MODEL];

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
/// Archives are unpacked and only the model file is kept.
pub fn download(
    model: &ModelInfo,
    dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let url = model.url();
    let expected = match model.sha256 {
        Some(s) => Some(s.to_owned()),
        None => expected_sha256(&url),
    };
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
    match model.extract {
        Some(member) => {
            let unpacked = unpack(&part, member, &path);
            let _ = std::fs::remove_file(&part);
            unpacked?;
        }
        None => std::fs::rename(&part, &path).map_err(|e| Error::io(&path, e))?,
    }
    tracing::info!(model = model.id, bytes = done, "model downloaded");
    Ok(path)
}

/// Copy the entry of a `.tar.bz2` whose path ends in `member` to `to`.
fn unpack(archive: &Path, member: &str, to: &Path) -> Result<()> {
    let file = File::open(archive).map_err(|e| Error::io(archive, e))?;
    let mut tar = tar::Archive::new(bzip2::read::BzDecoder::new(std::io::BufReader::new(file)));
    let bad = |e: std::io::Error| Error::Download(format!("{}: {e}", archive.display()));
    for entry in tar.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let name = entry.path().map_err(bad)?.into_owned();
        if name.file_name().is_some_and(|n| n == member) {
            let tmp = to.with_extension("unpack");
            let mut out = File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| Error::io(&tmp, e))?;
            drop(out);
            return std::fs::rename(&tmp, to).map_err(|e| Error::io(to, e));
        }
    }
    Err(Error::Download(format!(
        "{}: no {member} inside",
        archive.display()
    )))
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
        assert_eq!(
            parse_etag(&format!("W/\"{}\"", hex.to_uppercase())),
            Some(hex)
        );
    }

    #[test]
    fn speaker_models_come_from_github() {
        assert!(SEGMENTATION_MODEL.url().ends_with(".tar.bz2"));
        assert!(
            EMBEDDING_MODEL
                .url()
                .ends_with("/nemo_en_titanet_small.onnx")
        );
        assert!(SUPPORT_MODELS.iter().all(|m| find(m.id).is_none()));
    }

    #[test]
    fn unpacks_one_member() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.bz2");
        {
            let enc = bzip2::write::BzEncoder::new(
                File::create(&archive).unwrap(),
                bzip2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(enc);
            for (name, data) in [
                ("pkg/README.md", &b"readme"[..]),
                ("pkg/model.onnx", b"onnx"),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                tar.append_data(&mut h, name, data).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let to = dir.path().join("seg.onnx");
        unpack(&archive, "model.onnx", &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"onnx");
        assert!(unpack(&archive, "missing.onnx", &to).is_err());
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
