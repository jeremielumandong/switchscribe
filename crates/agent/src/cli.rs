//! Running the user's coding CLI headless: one prompt in on stdin, one answer out. For
//! the Ask panel the CLI also gets switchscribe's MCP server, read-only except for
//! changes it can only propose.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use switchscribe_store::CliKind;

use crate::process::{self, RunError};
use crate::{Error, MCP_SERVER_NAME, Result};

/// How long a summary may take before it is stopped.
pub const SUMMARY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long an Ask answer may take.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// A CLI ready to run.
#[derive(Clone, Debug)]
pub struct Cli {
    /// Which one.
    pub kind: CliKind,
    /// Its executable.
    pub program: PathBuf,
}

/// The MCP server a CLI is given: switchscribe's own executable with `mcp` arguments.
#[derive(Clone, Debug)]
pub struct McpServer {
    /// `switchscribe` (or `switchscribe.exe`).
    pub command: PathBuf,
    /// `["mcp", "--ask", ...]`.
    pub args: Vec<String>,
}

impl Cli {
    /// The CLI to run: `path` when set, else `kind`'s program found on `PATH`.
    pub fn find(kind: CliKind, path: Option<&Path>) -> Result<Cli> {
        let program = match path {
            Some(p) if p.is_file() => Some(p.to_path_buf()),
            Some(p) => process::which(&p.to_string_lossy()),
            None => process::which(kind.program()),
        };
        program
            .map(|program| Cli { kind, program })
            .ok_or_else(|| Error::CliNotFound(kind.label().to_owned()))
    }

    /// Run with `prompt` in `workdir` (an empty folder of switchscribe's, so the CLI
    /// does not pick up some project's context), giving it `mcp` when set. Returns the
    /// answer text.
    pub fn run(
        &self,
        prompt: &str,
        workdir: &Path,
        mcp: Option<&McpServer>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String> {
        std::fs::create_dir_all(workdir).map_err(|e| Error::Io(workdir.to_path_buf(), e))?;
        let answer_file = workdir.join("answer.txt");
        let _ = std::fs::remove_file(&answer_file);
        let cmd = self.command(workdir, mcp, &answer_file)?;
        tracing::info!(
            cli = self.kind.label(),
            mcp = mcp.is_some(),
            "running the CLI"
        );
        let out = process::run(cmd, prompt, timeout, cancel).map_err(|e| match e {
            RunError::Cancelled => Error::Cancelled,
            e => Error::Cli(e.to_string()),
        })?;
        if !out.success {
            let detail = out
                .stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no output")
                .trim()
                .to_owned();
            return Err(Error::Cli(format!(
                "{} exited with {}: {detail}",
                self.kind.label(),
                out.code
                    .map_or("a signal".to_owned(), |c| format!("code {c}"))
            )));
        }
        // Codex writes its last message to a file; the others answer on stdout.
        let text = std::fs::read_to_string(&answer_file).unwrap_or(out.stdout);
        if text.trim().is_empty() {
            return Err(Error::Cli(format!("{} gave no answer", self.kind.label())));
        }
        Ok(text)
    }

    fn command(&self, workdir: &Path, mcp: Option<&McpServer>, answer: &Path) -> Result<Command> {
        let mut cmd = process::command(&self.program);
        cmd.current_dir(workdir);
        match self.kind {
            CliKind::ClaudeCode => {
                cmd.args(["-p", "--output-format", "text"]);
                if let Some(m) = mcp {
                    let config = serde_json::json!({
                        "mcpServers": { MCP_SERVER_NAME: { "command": m.command, "args": m.args } }
                    });
                    cmd.arg("--mcp-config")
                        .arg(config.to_string())
                        .arg("--strict-mcp-config")
                        // Headless runs cannot ask for permission; allow only our tools.
                        .arg("--allowedTools")
                        .arg(format!("mcp__{MCP_SERVER_NAME}"));
                }
            }
            CliKind::Codex => {
                cmd.args(["exec", "--skip-git-repo-check", "--output-last-message"])
                    .arg(answer);
                if let Some(m) = mcp {
                    let command = serde_json::to_string(&m.command)?;
                    let args = serde_json::to_string(&m.args)?;
                    cmd.arg("-c")
                        .arg(format!("mcp_servers.{MCP_SERVER_NAME}.command={command}"))
                        .arg("-c")
                        .arg(format!("mcp_servers.{MCP_SERVER_NAME}.args={args}"));
                }
                // The prompt comes on stdin.
                cmd.arg("-");
            }
            CliKind::GeminiCli => {
                // Gemini reads MCP servers from the project's settings file; `trust`
                // skips its confirmations for our (read-only) tools only.
                let settings = workdir.join(".gemini");
                std::fs::create_dir_all(&settings).map_err(|e| Error::Io(settings.clone(), e))?;
                let servers = match mcp {
                    Some(m) => serde_json::json!({
                        MCP_SERVER_NAME: { "command": m.command, "args": m.args, "trust": true }
                    }),
                    None => serde_json::json!({}),
                };
                let file = settings.join("settings.json");
                std::fs::write(
                    &file,
                    serde_json::json!({ "mcpServers": servers }).to_string(),
                )
                .map_err(|e| Error::Io(file, e))?;
                // The prompt comes on stdin; `-p` makes the run non-interactive.
                cmd.args(["-p", "Follow the instructions above."]);
            }
        }
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_missing_cli_is_reported() {
        let err = Cli::find(
            CliKind::Codex,
            Some(Path::new("/nonexistent/switchscribe-codex")),
        )
        .unwrap_err();
        assert!(matches!(err, Error::CliNotFound(_)));
    }

    #[test]
    fn each_cli_gets_the_mcp_server() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = McpServer {
            command: PathBuf::from("/app/switchscribe"),
            args: vec!["mcp".into(), "--ask".into()],
        };
        let answer = dir.path().join("answer.txt");
        let cli = |kind| Cli {
            kind,
            program: PathBuf::from("x"),
        };
        let claude = args(
            &cli(CliKind::ClaudeCode)
                .command(dir.path(), Some(&mcp), &answer)
                .unwrap(),
        );
        assert!(claude.contains(&"--strict-mcp-config".to_owned()));
        assert!(claude.iter().any(|a| a.contains("/app/switchscribe")));
        let codex = args(
            &cli(CliKind::Codex)
                .command(dir.path(), Some(&mcp), &answer)
                .unwrap(),
        );
        assert!(codex.contains(&"mcp_servers.switchscribe.args=[\"mcp\",\"--ask\"]".to_owned()));
        assert_eq!(codex.last().map(String::as_str), Some("-"));
        cli(CliKind::GeminiCli)
            .command(dir.path(), Some(&mcp), &answer)
            .unwrap();
        let settings = std::fs::read_to_string(dir.path().join(".gemini/settings.json")).unwrap();
        assert!(settings.contains("\"trust\":true"));
    }

    #[cfg(unix)]
    #[test]
    fn runs_a_cli_and_reads_its_answer() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        std::fs::write(&fake, "#!/bin/sh\necho \"answer: $(cat)\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = Cli::find(CliKind::ClaudeCode, Some(&fake)).unwrap();
        let text = cli
            .run(
                "hi",
                &dir.path().join("run"),
                None,
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(text.trim(), "answer: hi");
    }
}
