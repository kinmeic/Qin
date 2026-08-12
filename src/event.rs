use anyhow::Result;
use serde::Serialize;
use std::cell::Cell;

use crate::config::{ConfigPathResolver, InitOutcome, UiConfig};
use crate::prompt_file::LoadedPrompt;

pub struct EventSink {
    quiet: bool,
    json: bool,
    show_tool_events: Cell<bool>,
    show_commands: Cell<bool>,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    event: &'a str,
    message: &'a str,
}

impl EventSink {
    pub fn new(quiet: bool, json: bool) -> Self {
        Self {
            quiet,
            json,
            show_tool_events: Cell::new(true),
            show_commands: Cell::new(true),
        }
    }

    pub fn configure(&self, ui: &UiConfig) {
        self.show_tool_events.set(ui.show_tool_events);
        self.show_commands.set(ui.show_commands);
    }

    pub fn phase(&self, message: &str) -> Result<()> {
        if !self.quiet && self.show_tool_events.get() {
            self.stderr("phase", &format!("● {message}"))?;
        }
        Ok(())
    }

    pub fn tool_started(&self, name: &str, detail: &str) -> Result<()> {
        if !self.quiet && self.show_tool_events.get() {
            self.stderr("tool_started", &format!("→ {name}  {detail}"))?;
        }
        Ok(())
    }

    pub fn tool_finished(&self, name: &str, summary: &str, elapsed_ms: u128) -> Result<()> {
        if !self.quiet && self.show_tool_events.get() {
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
        if !self.quiet && self.show_commands.get() {
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
        if !self.quiet && self.show_commands.get() {
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
        if !self.quiet && self.show_commands.get() {
            self.stderr("command_output", &format!("  │ {stream}: {}", redact(line)))?;
        }
        Ok(())
    }

    pub fn command_heartbeat(&self, seconds: u64) -> Result<()> {
        if !self.quiet && self.show_commands.get() {
            self.stderr(
                "command_heartbeat",
                &format!("... Command still running  {seconds}s"),
            )?;
        }
        Ok(())
    }

    pub fn command_finished(&self, code: Option<i32>, elapsed_ms: u128) -> Result<()> {
        let ok = code == Some(0);
        if self.quiet || (ok && !self.show_commands.get()) {
            return Ok(());
        }
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
        if !self.quiet && self.show_tool_events.get() {
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
        let answer = redact(answer);
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "event": "final_answer",
                    "answer": answer
                }))?
            );
        } else {
            println!("{}", sanitize_terminal(&answer));
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
        let path = sanitize_terminal(&outcome.config_path.display().to_string());
        println!("  Scope: {}", outcome.scope.label());
        println!("  Path: {path}");
        if cfg!(target_os = "macos") {
            println!(
                "  Edit: open -e {}",
                sanitize_terminal(&shell_quote(&outcome.config_path))
            );
        } else {
            println!(
                "  Edit: ${{EDITOR:-vi}} {}",
                sanitize_terminal(&shell_quote(&outcome.config_path))
            );
        }
        if let Some(backup) = outcome.backup_path.as_ref() {
            println!(
                "  Backup: {}",
                sanitize_terminal(&backup.display().to_string())
            );
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
            println!(
                "Configuration: {}",
                sanitize_terminal(&resolver.config_path().display().to_string())
            );
        }
        Ok(())
    }

    fn stderr(&self, event: &str, message: &str) -> Result<()> {
        let message = redact(message);
        if self.json {
            eprintln!(
                "{}",
                serde_json::to_string(&JsonEvent {
                    event,
                    message: &message
                })?
            );
        } else {
            eprintln!("{}", sanitize_terminal(&message));
        }
        Ok(())
    }
}

pub fn sanitize_terminal(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            matches!(character, '\n' | '\t')
                || (!character.is_control()
                    && !matches!(
                        *character,
                        '\u{200e}'
                            | '\u{200f}'
                            | '\u{202a}'..='\u{202e}'
                            | '\u{2066}'..='\u{2069}'
                    ))
        })
        .collect()
}

fn shell_quote(path: &std::path::Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn redact(value: &str) -> String {
    let mut output = value.to_string();
    for marker in [
        "sk-",
        "bearer ",
        "authorization=",
        "authorization:",
        "token=",
        "token:",
        "password=",
        "password:",
        "api_key=",
        "api_key:",
        "api-key=",
        "api-key:",
        "qin_api_key=",
    ] {
        let mut offset = 0;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(relative) = lower[offset..].find(marker) else {
                break;
            };
            let found = offset + relative;
            if marker == "sk-"
                && (found > 0
                    && output[..found]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric))
            {
                offset = found + marker.len();
                continue;
            }
            let start = if marker == "sk-" {
                found
            } else {
                found + marker.len()
            };
            let end = output[start..]
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '&' | '\'' | '"' | ',' | '}' | ']')
                })
                .map(|length| start + length)
                .unwrap_or(output.len());
            if start == end {
                offset = end.saturating_add(1).min(output.len());
                continue;
            }
            if marker == "sk-" && end.saturating_sub(start) < 20 {
                offset = end;
                continue;
            }
            if output[start..].starts_with("[REDACTED]") {
                offset = start + "[REDACTED]".len();
                continue;
            }
            output.replace_range(start..end, "[REDACTED]");
            offset = start + "[REDACTED]".len();
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_case_insensitively_and_sanitizes_terminal_controls() {
        let redacted = redact("Authorization: Bearer SECRET token=abc");
        assert!(!redacted.contains("SECRET"));
        assert!(!redacted.contains("abc"));
        assert_eq!(sanitize_terminal("ok\u{1b}[31m\u{202e}"), "ok[31m");
        assert_eq!(sanitize_terminal("left\rright"), "leftright");
    }
}
