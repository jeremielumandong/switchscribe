//! The settings file: one JSON document in the data folder, read once at startup and
//! written whole (to a temporary file, then renamed) when something changes.
//!
//! Every field has a default and unknown fields are ignored, so an older or hand-edited
//! file still loads; a file that is not JSON at all loads as the defaults (and is logged).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, io_err};

/// Everything in Settings, grouped like the Settings screen in the UI spec.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Audio capture.
    pub audio: AudioSettings,
    /// Meeting detection.
    pub detection: DetectionSettings,
    /// Speech models.
    pub models: ModelSettings,
    /// The coding CLI that writes summaries and answers the Ask panel.
    pub agent: AgentSettings,
    /// Storage and privacy.
    pub storage: StorageSettings,
    /// Window layout remembered between runs.
    pub layout: LayoutSettings,
    /// Calendar and Jira (tokens are not here: they are sealed in the store's secrets).
    #[serde(default)]
    pub integrations: IntegrationSettings,
}

/// What the Teams track captures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureMode {
    /// Process loopback of Teams and its child processes.
    #[default]
    TeamsOnly,
    /// Classic loopback of the default playback device.
    WholeSystem,
}

/// Settings → Audio.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Teams capture mode.
    pub capture_mode: CaptureMode,
    /// Microphone device id; `None` is the default communications device.
    pub mic_device: Option<String>,
}

/// Settings → Detection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectionSettings {
    /// Watch for Teams meetings.
    pub auto_detect: bool,
    /// Record without asking when a meeting is detected.
    pub auto_record: bool,
    /// Stop after the Teams session has been silent this long.
    pub auto_stop_minutes: u32,
    /// Meeting titles to record without asking ("Always record this meeting").
    pub always_record: Vec<String>,
}

impl Default for DetectionSettings {
    fn default() -> Self {
        Self {
            auto_detect: true,
            auto_record: false,
            auto_stop_minutes: 3,
            always_record: Vec::new(),
        }
    }
}

/// Settings → Integrations. Secrets (OAuth tokens, the Jira API token) are kept sealed
/// in the store, never in this file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationSettings {
    /// The user's own Entra ID app registration (public client) for Microsoft Graph.
    pub graph_client_id: Option<String>,
    /// The Entra tenant to sign in to ("organizations", "common" or a tenant id).
    pub graph_tenant: String,
    /// The signed-in Graph account, as shown in Settings.
    pub graph_account: Option<String>,
    /// A published calendar (ICS) URL, the fallback when Graph is not allowed.
    pub ics_url: Option<String>,
    /// Jira Cloud site, `https://example.atlassian.net`.
    pub jira_site: Option<String>,
    /// The Atlassian account email the API token belongs to.
    pub jira_email: Option<String>,
    /// Project key issues are created in ("OPS").
    pub jira_project: Option<String>,
    /// Issue type name for new issues.
    pub jira_issue_type: String,
}

impl Default for IntegrationSettings {
    fn default() -> Self {
        Self {
            graph_client_id: None,
            graph_tenant: "organizations".to_owned(),
            graph_account: None,
            ics_url: None,
            jira_site: None,
            jira_email: None,
            jira_project: None,
            jira_issue_type: "Task".to_owned(),
        }
    }
}

/// Settings → Models.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    /// The post-call transcription model; `None` until one is downloaded.
    pub transcription_model: Option<String>,
    /// Use the GPU when there is one.
    pub use_gpu: bool,
    /// Live captions while recording.
    pub live_captions: bool,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            transcription_model: None,
            use_gpu: true,
            live_captions: false,
        }
    }
}

/// A coding CLI switchscribe can drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliKind {
    /// Claude Code (`claude`).
    #[default]
    ClaudeCode,
    /// Codex CLI (`codex`).
    Codex,
    /// Gemini CLI (`gemini`).
    GeminiCli,
}

impl CliKind {
    /// Every CLI, in the order the selector lists them.
    pub const ALL: [CliKind; 3] = [CliKind::ClaudeCode, CliKind::Codex, CliKind::GeminiCli];

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "Claude Code",
            CliKind::Codex => "Codex",
            CliKind::GeminiCli => "Gemini CLI",
        }
    }

    /// The executable looked up on `PATH` when no path is set.
    pub fn program(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude",
            CliKind::Codex => "codex",
            CliKind::GeminiCli => "gemini",
        }
    }

    /// The next CLI in [`CliKind::ALL`], wrapping around.
    pub fn next(self) -> CliKind {
        let i = CliKind::ALL.iter().position(|c| *c == self).unwrap_or(0);
        CliKind::ALL[(i + 1) % CliKind::ALL.len()]
    }
}

/// Settings → Agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Which CLI.
    pub cli: CliKind,
    /// Its executable; `None` looks up [`CliKind::program`] on `PATH`.
    pub cli_path: Option<PathBuf>,
    /// Summarize every meeting after it is transcribed.
    pub auto_summarize: bool,
    /// The user's own guidance for summary templates, by template key ("planning");
    /// templates not listed use the built-in guidance.
    pub templates: std::collections::BTreeMap<String, String>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            cli: CliKind::default(),
            cli_path: None,
            auto_summarize: true,
            templates: std::collections::BTreeMap::new(),
        }
    }
}

/// Settings → Storage & privacy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    /// Delete meeting audio after this many days; `None` keeps it.
    pub keep_audio_days: Option<u32>,
    /// The notice to paste into a meeting's chat before recording.
    pub consent_notice: String,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            keep_audio_days: Some(30),
            consent_notice: "Heads up: I'm recording this meeting locally with switchscribe \
                             to take notes. Let me know if you'd rather I didn't."
                .to_owned(),
        }
    }
}

/// Panels and window state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    /// The meetings sidebar is shown.
    pub sidebar_open: bool,
    /// The Ask panel is shown; `None` until the user toggles it (then the app opens it on
    /// wide windows only).
    pub ask_open: Option<bool>,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            ask_open: None,
        }
    }
}

impl Settings {
    /// Read the settings file. A missing file is the defaults; an unreadable or malformed
    /// one is logged and also gives the defaults, so the app always starts.
    pub fn load(path: &Path) -> Settings {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "settings file is not valid; using defaults");
                Settings::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read settings; using defaults");
                Settings::default()
            }
        }
    }

    /// Write the settings file atomically (temporary file, then rename), creating its
    /// folder when needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(io_err(&tmp))?;
        std::fs::rename(&tmp, path).map_err(io_err(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::load(&dir.path().join("settings.json"));
        assert_eq!(s, Settings::default());
        assert!(s.layout.sidebar_open);
        assert_eq!(s.agent.cli, CliKind::ClaudeCode);
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let mut s = Settings::default();
        s.agent.cli = CliKind::Codex;
        s.layout.ask_open = Some(false);
        s.audio.capture_mode = CaptureMode::WholeSystem;
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn partial_and_unknown_fields_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"cli":"gemini-cli"},"later":1}"#).unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.agent.cli, CliKind::GeminiCli);
        assert_eq!(s.detection, DetectionSettings::default());
    }

    #[test]
    fn integrations_default_and_round_trip() {
        let s = Settings::default();
        assert_eq!(s.integrations.graph_tenant, "organizations");
        assert_eq!(s.integrations.jira_issue_type, "Task");
        assert!(s.detection.always_record.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // A file with part of the section loads the rest as defaults.
        std::fs::write(&path, r#"{"integrations":{"jira_project":"OPS"}}"#).unwrap();
        let mut s = Settings::load(&path);
        assert_eq!(s.integrations.jira_project.as_deref(), Some("OPS"));
        assert_eq!(s.integrations.jira_issue_type, "Task");
        s.detection.always_record.push("Weekly sync".into());
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
    }

    #[test]
    fn malformed_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn cli_cycles() {
        assert_eq!(CliKind::ClaudeCode.next(), CliKind::Codex);
        assert_eq!(CliKind::GeminiCli.next(), CliKind::ClaudeCode);
    }
}
