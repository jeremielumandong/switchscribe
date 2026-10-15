//! Integrations (M5): Outlook calendar through Microsoft Graph, Jira, and exports.

use serde::{Deserialize, Serialize};

/// Export formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportFormat {
    /// Markdown (summary, decisions, action items, transcript).
    Markdown,
    /// Word document.
    Word,
    /// SubRip captions.
    Srt,
    /// WebVTT captions.
    Vtt,
    /// The meeting as JSON.
    Json,
}

impl ExportFormat {
    /// Every format, in the Export menu's order.
    pub const ALL: [ExportFormat; 5] = [
        ExportFormat::Markdown,
        ExportFormat::Word,
        ExportFormat::Srt,
        ExportFormat::Vtt,
        ExportFormat::Json,
    ];

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Word => "Word",
            ExportFormat::Srt => "SRT captions",
            ExportFormat::Vtt => "VTT captions",
            ExportFormat::Json => "JSON",
        }
    }

    /// File extension.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Word => "docx",
            ExportFormat::Srt => "srt",
            ExportFormat::Vtt => "vtt",
            ExportFormat::Json => "json",
        }
    }
}

/// A transcript timestamp as SRT writes it (`HH:MM:SS,mmm`).
pub fn srt_timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_times() {
        assert_eq!(srt_timestamp(0), "00:00:00,000");
        assert_eq!(srt_timestamp(3_723_045), "01:02:03,045");
        assert_eq!(srt_timestamp(-5), "00:00:00,000");
    }

    #[test]
    fn extensions_are_unique() {
        let mut exts: Vec<&str> = ExportFormat::ALL.iter().map(|f| f.extension()).collect();
        exts.dedup();
        assert_eq!(exts.len(), ExportFormat::ALL.len());
    }
}
