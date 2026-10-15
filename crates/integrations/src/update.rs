//! Update checks against the GitHub Releases of `jeremielumandong/switchscribe` (M6).
//!
//! The release workflow (`.github/workflows/release.yml`) publishes, for tag `vX.Y.Z`,
//! an Inno Setup installer named [`installer_name`] (`switchscribe-setup-X.Y.Z-x64.exe`)
//! and a `<installer>.sha256` sidecar holding its lowercase hex SHA-256. [`latest`] asks
//! GitHub for the newest published release and returns it when it is newer than the
//! running build; [`download`] streams the installer next to a `.part` file, checks the
//! hash and only then gives it its real name. The app then runs it with
//! [`INSTALLER_ARGS`] through `switchscribe_agent::process::command` and quits.
//!
//! The sidecar comes from the same release as the installer, so it guards against a
//! truncated or corrupted download, not against a compromised release: the Authenticode
//! signature on the installer is the trust anchor (see `docs/DECISIONS.md`).
//!
//! Calls block; run them off the UI thread.

use std::cmp::Ordering;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Error, Result, http};

/// The GitHub repository releases come from.
pub const REPO: &str = "jeremielumandong/switchscribe";

/// The GitHub API endpoint for the newest published (non-draft, non-prerelease) release.
pub const LATEST_URL: &str =
    "https://api.github.com/repos/jeremielumandong/switchscribe/releases/latest";

/// Arguments for a silent upgrade with the downloaded Inno Setup installer: no wizard,
/// no message boxes, no reboot, and running copies of switchscribe closed first (the
/// script sets `CloseApplications=yes`). The installer is per-user, so no UAC prompt.
pub const INSTALLER_ARGS: [&str; 4] = [
    "/VERYSILENT",
    "/SUPPRESSMSGBOXES",
    "/NORESTART",
    "/CLOSEAPPLICATIONS",
];

/// Service name in network errors.
const SERVICE: &str = "GitHub";

/// A downloadable file attached to a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    /// File name, e.g. `switchscribe-setup-0.2.0-x64.exe`.
    pub name: String,
    /// The public download URL (`browser_download_url`).
    pub url: String,
    /// Size in bytes, as GitHub reports it.
    pub size: u64,
}

/// A release newer than the running build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version without the `v`, e.g. `0.2.0`.
    pub version: String,
    /// Release notes (Markdown), possibly empty.
    pub notes: String,
    /// The release's page on GitHub.
    pub page_url: String,
    /// The Windows installer.
    pub installer: Asset,
    /// Lowercase hex SHA-256 of the installer, from its `.sha256` sidecar; `None` when
    /// the release has no sidecar (then [`download`] refuses it).
    pub sha256: Option<String>,
}

/// What [`pick`] finds in a release document: the release (its `sha256` still unset)
/// and the sidecar asset to read the hash from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The release, with `sha256: None`.
    pub release: Release,
    /// The `<installer>.sha256` asset, when the release has one.
    pub sidecar: Option<Asset>,
}

/// A `MAJOR.MINOR.PATCH[-PRE][+BUILD]` version, ordered by semver precedence (build
/// metadata ignored; pre-release identifiers compared numerically or as text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// Major.
    pub major: u64,
    /// Minor.
    pub minor: u64,
    /// Patch.
    pub patch: u64,
    /// Pre-release identifiers (`rc.1` → `["rc", "1"]`); empty for a release.
    pub pre: Vec<String>,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, `1.2.3-rc.1` or `1.2.3+build`. `None` if it is not one.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);
        let s = s.split_once('+').map_or(s, |(v, _)| v);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut num = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let (major, minor, patch) = (num()?, num()?, num()?);
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => {
                let ids: Vec<String> = pre.split('.').map(str::to_owned).collect();
                let valid = |id: &String| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                };
                if !ids.iter().all(valid) {
                    return None;
                }
                ids
            }
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Whether this is a pre-release (`1.0.0-rc.1`).
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => cmp_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// Semver pre-release precedence: numeric identifiers below text ones, numbers compared
/// as numbers, and a shorter list first when one is a prefix of the other.
fn cmp_pre(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Whether `candidate` is a newer version than `current`. Unparseable versions are
/// never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// The installer asset name the release workflow publishes for `version` (no `v`).
pub fn installer_name(version: &str) -> String {
    format!("switchscribe-setup-{version}-x64.exe")
}

/// Pick the update out of a GitHub release document (`GET .../releases/latest`): `None`
/// for drafts, prereleases, tags that are not `vX.Y.Z`, versions not newer than
/// `current`, and releases without the installer asset. Pure, for tests.
pub fn pick(json: &Value, current: &str) -> Option<Candidate> {
    if json.get("draft").and_then(Value::as_bool).unwrap_or(false)
        || json
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    let tag = json.get("tag_name")?.as_str()?;
    let version = Version::parse(tag)?;
    if version.is_prerelease() {
        return None;
    }
    let current = Version::parse(current)?;
    if version <= current {
        return None;
    }
    let version = version.to_string();
    let name = installer_name(&version);
    let sidecar_name = format!("{name}.sha256");
    let assets = json.get("assets")?.as_array()?;
    let find = |wanted: &str| {
        assets.iter().find_map(|a| {
            let asset_name = a.get("name")?.as_str()?;
            if !asset_name.eq_ignore_ascii_case(wanted) {
                return None;
            }
            Some(Asset {
                // Our own spelling: the name becomes a file name on disk.
                name: wanted.to_owned(),
                url: a.get("browser_download_url")?.as_str()?.to_owned(),
                size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
            })
        })
    };
    let installer = find(&name)?;
    let sidecar = find(&sidecar_name);
    let text = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(Candidate {
        release: Release {
            version,
            notes: text("body"),
            page_url: text("html_url"),
            installer,
            sha256: None,
        },
        sidecar,
    })
}

/// The hash in a `.sha256` sidecar: the first word, when it is 64 hex digits (so both a
/// bare hash and `sha256sum`'s `<hash>  <file>` work). Lowercased.
pub fn parse_sha256(text: &str) -> Option<String> {
    let word = text
        .trim_start_matches('\u{feff}')
        .split_whitespace()
        .next()?;
    (word.len() == 64 && word.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| word.to_ascii_lowercase())
}

/// Ask GitHub for the newest release and return it when it is newer than `current`
/// (the running build's version, e.g. `env!("CARGO_PKG_VERSION")`), with the installer's
/// SHA-256 read from its sidecar. `Ok(None)` when up to date or when the repository has
/// no release yet.
pub fn latest(current: &str) -> Result<Option<Release>> {
    let agent = http::agent();
    let ua = user_agent(current);
    let reply = http::finish(
        SERVICE,
        agent
            .get(LATEST_URL)
            .header("User-Agent", &ua)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call(),
    )?;
    match reply.status {
        404 => {
            tracing::debug!("no published release yet");
            return Ok(None);
        }
        403 | 429 => {
            return Err(Error::Rejected(format!(
                "GitHub is limiting update checks; try again later ({})",
                http::first_line(&reply.body)
            )));
        }
        _ if !reply.ok() => {
            return Err(Error::BadResponse {
                service: SERVICE,
                detail: format!("HTTP {}: {}", reply.status, http::first_line(&reply.body)),
            });
        }
        _ => {}
    }
    let json: Value = serde_json::from_str(&reply.body).map_err(|e| Error::BadResponse {
        service: SERVICE,
        detail: e.to_string(),
    })?;
    let Some(Candidate {
        mut release,
        sidecar,
    }) = pick(&json, current)
    else {
        tracing::debug!(current, "up to date");
        return Ok(None);
    };
    if let Some(sidecar) = sidecar {
        let reply = http::finish(
            SERVICE,
            agent.get(&sidecar.url).header("User-Agent", &ua).call(),
        )?;
        if reply.ok() {
            release.sha256 = parse_sha256(&reply.body);
        }
        if release.sha256.is_none() {
            tracing::warn!(status = reply.status, asset = %sidecar.name, "unreadable checksum sidecar");
        }
    } else {
        tracing::warn!(version = %release.version, "release has no checksum sidecar");
    }
    tracing::info!(current, version = %release.version, "update available");
    Ok(Some(release))
}

/// Download the release's installer into `dir`, reporting `(bytes so far, total)` (total
/// is the asset size, or 0 when unknown). Streams to `<dir>/<name>.part`, checks the
/// SHA-256 and renames it to `<dir>/<name>`, which it returns. Refuses a release without
/// a hash, and deletes the partial file on a mismatch or error.
pub fn download(
    release: &Release,
    dir: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let Some(expected) = release.sha256.as_deref() else {
        return Err(Error::Update(format!(
            "switchscribe {} has no checksum; download it from {}",
            release.version, release.page_url
        )));
    };
    let asset = &release.installer;
    if asset.name.contains(['/', '\\']) || asset.name.starts_with('.') {
        return Err(Error::Update(format!("unsafe file name {:?}", asset.name)));
    }
    std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
    let path = dir.join(&asset.name);
    let part = dir.join(format!("{}.part", asset.name));

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(Duration::from_secs(30 * 60)))
        .build()
        .into();
    let net = |e: &dyn fmt::Display| Error::Network {
        service: SERVICE,
        detail: e.to_string(),
    };
    let mut resp = agent
        .get(&asset.url)
        .header("User-Agent", &user_agent(&release.version))
        .call()
        .map_err(|e| net(&e))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::BadResponse {
            service: SERVICE,
            detail: format!("HTTP {status} downloading {}", asset.name),
        });
    }
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(asset.size);

    let streamed = (|| -> Result<String> {
        let mut out = File::create(&part).map_err(|e| io_error(&part, &e))?;
        let mut body = resp.body_mut().as_reader();
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = body.read(&mut buf).map_err(|e| net(&e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| io_error(&part, &e))?;
            done += n as u64;
            progress(done, total);
        }
        out.flush().map_err(|e| io_error(&part, &e))?;
        out.sync_all().map_err(|e| io_error(&part, &e))?;
        Ok(hex(&hasher.finalize()))
    })();
    let got = match streamed {
        Ok(got) => got,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    seal(&part, &path, expected, &got)?;
    tracing::info!(version = %release.version, path = %path.display(), "update downloaded");
    Ok(path)
}

/// Rename a finished `.part` file to `path` when `got` matches `expected` (hex,
/// case-insensitive); otherwise delete it and fail.
fn seal(part: &Path, path: &Path, expected: &str, got: &str) -> Result<()> {
    if !expected.eq_ignore_ascii_case(got) {
        let _ = std::fs::remove_file(part);
        tracing::warn!(expected, got, "update checksum mismatch");
        return Err(Error::Update(format!(
            "the download is damaged (SHA-256 {got}, expected {expected}); it was deleted"
        )));
    }
    // Windows' rename fails onto an existing file (an earlier download of the same
    // version); the hash matched, so the new copy replaces it.
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| io_error(path, &e))?;
    }
    std::fs::rename(part, path).map_err(|e| io_error(path, &e))
}

/// Lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| io_error(path, &e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).map_err(|e| io_error(path, &e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `switchscribe/<version>` (GitHub's API refuses requests without a User-Agent).
fn user_agent(version: &str) -> String {
    format!("switchscribe/{version} (+https://github.com/{REPO})")
}

/// An I/O failure on `path` as an update error.
fn io_error(path: &Path, e: &std::io::Error) -> Error {
    Error::Update(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release_json(tag: &str, version: &str) -> Value {
        let name = installer_name(version);
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "html_url": format!("https://github.com/{REPO}/releases/tag/{tag}"),
            "body": "## What's changed\n- Live captions",
            "assets": [
                {
                    "name": format!("{name}.sha256"),
                    "browser_download_url": format!("https://example.invalid/{name}.sha256"),
                    "size": 64
                },
                {
                    "name": name,
                    "browser_download_url": format!("https://example.invalid/{name}"),
                    "size": 123_456
                }
            ]
        })
    }

    #[test]
    fn picks_a_newer_release() {
        let c = pick(&release_json("v0.2.0", "0.2.0"), "0.1.9").unwrap();
        assert_eq!(c.release.version, "0.2.0");
        assert_eq!(c.release.installer.name, "switchscribe-setup-0.2.0-x64.exe");
        assert_eq!(
            c.release.installer.url,
            "https://example.invalid/switchscribe-setup-0.2.0-x64.exe"
        );
        assert_eq!(c.release.installer.size, 123_456);
        assert_eq!(c.release.sha256, None);
        assert!(c.release.notes.contains("Live captions"));
        assert!(c.release.page_url.ends_with("/releases/tag/v0.2.0"));
        assert_eq!(
            c.sidecar.unwrap().name,
            "switchscribe-setup-0.2.0-x64.exe.sha256"
        );
    }

    #[test]
    fn ignores_same_and_older() {
        assert_eq!(pick(&release_json("v0.2.0", "0.2.0"), "0.2.0"), None);
        assert_eq!(pick(&release_json("v0.2.0", "0.2.0"), "0.10.0"), None);
        assert_eq!(pick(&release_json("v0.2.0", "0.2.0"), "nonsense"), None);
    }

    #[test]
    fn ignores_prereleases_and_drafts() {
        let mut j = release_json("v0.3.0", "0.3.0");
        j["prerelease"] = json!(true);
        assert_eq!(pick(&j, "0.2.0"), None);
        let mut j = release_json("v0.3.0", "0.3.0");
        j["draft"] = json!(true);
        assert_eq!(pick(&j, "0.2.0"), None);
        // A pre-release tag not flagged as one.
        assert_eq!(
            pick(&release_json("v0.3.0-rc.1", "0.3.0-rc.1"), "0.2.0"),
            None
        );
        // Not a version tag at all.
        assert_eq!(pick(&release_json("nightly", "nightly"), "0.2.0"), None);
    }

    #[test]
    fn needs_the_installer_asset() {
        let mut j = release_json("v0.3.0", "0.3.0");
        j["assets"] = json!([{
            "name": "switchscribe-0.3.0-linux.tar.gz",
            "browser_download_url": "https://example.invalid/x",
            "size": 1
        }]);
        assert_eq!(pick(&j, "0.2.0"), None);

        // The installer without its sidecar is still offered (download refuses it).
        let mut j = release_json("v0.3.0", "0.3.0");
        let installer = j["assets"][1].clone();
        j["assets"] = json!([installer]);
        let c = pick(&j, "0.2.0").unwrap();
        assert_eq!(c.sidecar, None);
    }

    #[test]
    fn compares_versions() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-beta"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert_eq!(v("v1.2.3+build.7"), v("1.2.3"));
        assert_eq!(v("V2.0.0").to_string(), "2.0.0");
        assert_eq!(v("1.0.0-rc.1").to_string(), "1.0.0-rc.1");
        for bad in [
            "", "1", "1.2", "1.2.3.4", "1..3", "a.b.c", "1.2.3-", "-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert!(is_newer("v0.0.2", "0.0.1"));
        assert!(!is_newer("v0.0.1", "0.0.1"));
        assert!(!is_newer("garbage", "0.0.1"));
    }

    #[test]
    fn reads_sidecars() {
        let h = "AB".repeat(32);
        assert_eq!(
            parse_sha256(&format!("{h}\n")),
            Some(h.to_ascii_lowercase())
        );
        assert_eq!(
            parse_sha256(&format!("{h}  switchscribe-setup-0.2.0-x64.exe\r\n")),
            Some(h.to_ascii_lowercase())
        );
        assert_eq!(parse_sha256("deadbeef"), None);
        assert_eq!(parse_sha256(""), None);
    }

    #[test]
    fn verifies_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("setup.exe.part");
        let path = dir.path().join("setup.exe");
        std::fs::write(&part, b"abc").unwrap();
        let got = sha256_file(&part).unwrap();
        // The NIST test vector for "abc".
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(got, abc);

        // A mismatch deletes the partial file and leaves nothing behind.
        let wrong = "0".repeat(64);
        assert!(matches!(
            seal(&part, &path, &wrong, &got),
            Err(Error::Update(_))
        ));
        assert!(!part.exists() && !path.exists());

        // A match renames it, replacing an older copy.
        std::fs::write(&part, b"abc").unwrap();
        std::fs::write(&path, b"stale").unwrap();
        seal(&part, &path, &abc.to_ascii_uppercase(), &got).unwrap();
        assert!(!part.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    }

    #[test]
    fn download_needs_a_hash() {
        let c = pick(&release_json("v0.2.0", "0.2.0"), "0.1.0").unwrap();
        let dir = tempfile::tempdir().unwrap();
        // No network: the missing hash is refused before any request.
        let err = download(&c.release, dir.path(), |_, _| {}).unwrap_err();
        assert!(matches!(err, Error::Update(_)));
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn installer_args_are_silent() {
        assert!(INSTALLER_ARGS.contains(&"/VERYSILENT"));
        assert!(INSTALLER_ARGS.contains(&"/CLOSEAPPLICATIONS"));
    }
}
