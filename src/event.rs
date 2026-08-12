use anyhow::Result;
use serde::Serialize;

use crate::config::{ConfigPathResolver, InitOutcome};
use crate::prompt_file::LoadedPrompt;

pub struct EventSink {
    quiet: bool,
    json: bool,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    event: &'a str,
    message: &'a str,
}

impl EventSink {
    pub fn new(quiet: bool, json: bool) -> Self {
        Self { quiet, json }
    }

    pub fn phase(&self, message: &str) -> Result<()> {
        if !self.quiet {
            self.stderr("phase", &format!("● {message}"))?;
        }
        Ok(())
    }

    pub fn tool_started(&self, name: &str, detail: &str) -> Result<()> {
        if !self.quiet {
            self.stderr("tool_started", &format!("→ {name}  {detail}"))?;
        }
        Ok(())
    }

    pub fn tool_finished(&self, name: &str, summary: &str, elapsed_ms: u128) -> Result<()> {
        if !self.quiet {
            self.stderr(
                "tool_finished",
                &format!("✓ {name}  {summary}  {elapsed_ms}ms"),
            )?;
        }
        Ok(())
    }

    pub fn tool_failed(&self, name: &str, error: &str, elapsed_ms: u128) -> Result<()> {
        self.stderr("tool_failed", &format!("✗ {name}  {error}  {elapsed_ms}ms"))
    }

    pub fn command_started(
        &self,
        cwd: &std::path::Path,
        command: &str,
        elevated: bool,
        timeout: u64,
    ) -> Result<()> {
        if !self.quiet {
            let level = if elevated {
                "sudo/root"
            } else {
                "standard privileges"
            };
            self.stderr(
                "command_started",
                &format!(
                    "→ shell [{level}]  cwd={}  timeout={}s\n  $ {}",
                    cwd.display(),
                    timeout,
                    redact(command)
                ),
            )?;
        }
        Ok(())
    }

    pub fn command_preview(
        &self,
        cwd: &std::path::Path,
        command: &str,
        elevated: bool,
        timeout: u64,
    ) -> Result<()> {
        if !self.quiet {
            let level = if elevated {
                "sudo/root"
            } else {
                "standard privileges"
            };
            self.stderr(
                "command_preview",
                &format!(
                    "-> Preparing command [{level}]  cwd={}  timeout={}s\n  $ {}",
                    cwd.display(),
                    timeout,
                    redact(command)
                ),
            )?;
        }
        Ok(())
    }

    pub fn command_output(&self, stream: &str, line: &str) -> Result<()> {
        if !self.quiet {
            self.stderr("command_output", &format!("  │ {stream}: {}", redact(line)))?;
        }
        Ok(())
    }

    pub fn command_heartbeat(&self, seconds: u64) -> Result<()> {
        if !self.quiet {
            self.stderr(
                "command_heartbeat",
                &format!("... Command still running  {seconds}s"),
            )?;
        }
        Ok(())
    }

    pub fn command_finished(&self, code: Option<i32>, elapsed_ms: u128) -> Result<()> {
        let ok = code == Some(0);
        self.stderr(
            if ok {
                "command_finished"
            } else {
                "command_failed"
            },
            &format!(
                "{} Command {}  exit={}  {:.2}s",
                if ok { "✓" } else { "✗" },
                if ok { "succeeded" } else { "failed" },
                code.map_or_else(|| "signal".into(), |v| v.to_string()),
                elapsed_ms as f64 / 1000.0
            ),
        )
    }

    pub fn approval(&self, message: &str) -> Result<()> {
        self.stderr("approval_required", &format!("? {message}"))
    }

    pub fn prompt_file_loaded(&self, loaded: &LoadedPrompt) -> Result<()> {
        if !self.quiet {
            let short_hash = &loaded.sha256[..12];
            self.stderr(
                "tool_finished",
                &format!(
                    "OK Loaded prompt file  path={}  bytes={}  sha256={}",
                    loaded.canonical_path.display(),
                    loaded.byte_len,
                    short_hash
                ),
            )?;
        }
        Ok(())
    }

    pub fn final_answer(&self, answer: &str) -> Result<()> {
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "event": "final_answer",
                    "answer": answer
                }))?
            );
        } else {
            println!("{answer}");
        }
        Ok(())
    }

    pub fn success(&self, message: &str) -> Result<()> {
        self.stderr("success", &format!("✓ {message}"))
    }

    pub fn init_outcome(&self, outcome: &InitOutcome) -> Result<()> {
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "created": outcome.created,
                    "scope": outcome.scope.label(),
                    "config_path": outcome.config_path,
                    "backup_path": outcome.backup_path,
                    "next_action": "Edit the model and embedding settings, then run qin config check"
                }))?
            );
            return Ok(());
        }

        if outcome.created {
            println!("OK qin configuration file created");
        } else {
            println!("OK qin configuration file already exists; no changes made");
        }
        println!("  Scope: {}", outcome.scope.label());
        println!("  Path: {}", outcome.config_path.display());
        if cfg!(target_os = "macos") {
            println!("  Edit: open -e {}", shell_quote(&outcome.config_path));
        } else {
            println!(
                "  Edit: ${{EDITOR:-vi}} {}",
                shell_quote(&outcome.config_path)
            );
        }
        if let Some(backup) = outcome.backup_path.as_ref() {
            println!("  Backup: {}", backup.display());
        }
        println!("  Next: edit the model and embedding settings, then run qin config check");
        Ok(())
    }

    pub fn config_path(&self, resolver: &ConfigPathResolver) -> Result<()> {
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "scope": resolver.scope().label(),
                    "config_path": resolver.config_path()
                }))?
            );
        } else {
            println!("Scope: {}", resolver.scope().label());
            println!("Configuration: {}", resolver.config_path().display());
        }
        Ok(())
    }

    fn stderr(&self, event: &str, message: &str) -> Result<()> {
        if self.json {
            eprintln!("{}", serde_json::to_string(&JsonEvent { event, message })?);
        } else {
            eprintln!("{message}");
        }
        Ok(())
    }
}

fn shell_quote(path: &std::path::Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn redact(value: &str) -> String {
    let mut output = value.to_string();
    for marker in ["sk-", "Bearer ", "token=", "password=", "api_key="] {
        let mut offset = 0;
        while let Some(found) = output[offset..].find(marker) {
            let start = offset + found + marker.len();
            let end = output[start..]
                .find(|c: char| c.is_whitespace() || c == '&' || c == '\'' || c == '"')
                .map(|n| start + n)
                .unwrap_or(output.len());
            output.replace_range(start..end, "[REDACTED]");
            offset = start + "[REDACTED]".len();
        }
    }
    output
}
