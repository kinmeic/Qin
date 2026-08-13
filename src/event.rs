use anyhow::Result;
use serde::Serialize;
use std::cell::Cell;

use crate::config::{ConfigPathResolver, InitOutcome, UiConfig};
use crate::prompt_file::LoadedPrompt;

pub struct EventSink {
    quiet: bool,
    json: bool,
    /// Stream live command output lines (hidden unless --verbose).
    verbose: bool,
    show_tool_events: Cell<bool>,
    show_commands: Cell<bool>,
    /// stderr is an interactive terminal: heartbeats rewrite one line in place.
    terminal: bool,
    /// ANSI colors for system event lines (disabled by NO_COLOR).
    color: bool,
    status_line_open: Cell<bool>,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    event: &'a str,
    message: &'a str,
}

impl EventSink {
    pub fn new(quiet: bool, json: bool, verbose: bool) -> Self {
        let terminal =
            !json && !cfg!(windows) && std::io::IsTerminal::is_terminal(&std::io::stderr());
        let color = terminal && std::env::var_os("NO_COLOR").is_none();
        Self {
            quiet,
            json,
            verbose,
            show_tool_events: Cell::new(true),
            show_commands: Cell::new(true),
            terminal,
            color,
            status_line_open: Cell::new(false),
        }
    }

    pub fn configure(&self, ui: &UiConfig) {
        self.show_tool_events.set(ui.show_tool_events);
        self.show_commands.set(ui.show_commands);
    }

    /// Whether command preview lines are visible, so approval prompts can
    /// refer to "this command" instead of repeating the full command line.
    pub fn shows_command_details(&self) -> bool {
        !self.quiet && self.show_commands.get()
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
        elevated: bool,
        timeout: u64,
    ) -> Result<()> {
        if self.quiet || !self.show_commands.get() {
            return Ok(());
        }
        // The command itself was already shown by tool_started; here we
        // only report the execution context as the run begins.
        let level = if elevated {
            "sudo/root"
        } else {
            "standard privileges"
        };
        let message = format!(
            "▸ Running [{level}]  cwd={}  timeout={}s",
            cwd.display(),
            timeout
        );
        if self.terminal {
            // Transient status line: the heartbeat or command_finished
            // rewrites this same line instead of appending a new one.
            if self.status_line_open.replace(false) {
                eprint!("\r\x1b[2K");
            }
            let message = sanitize_terminal(&redact(&message));
            if self.color {
                eprint!("\x1b[34m{INDENT}{message}\x1b[0m");
            } else {
                eprint!("{INDENT}{message}");
            }
            self.status_line_open.set(true);
            return Ok(());
        }
        self.stderr("command_started", &message)
    }

    pub fn command_output(&self, stream: &str, line: &str) -> Result<()> {
        // Live command output is hidden unless --verbose; JSON consumers
        // always receive it as structured events.
        if self.json || (self.verbose && !self.quiet && self.show_commands.get()) {
            self.stderr("command_output", &format!("│ {stream}: {}", redact(line)))?;
        }
        Ok(())
    }

    pub fn command_heartbeat(&self, seconds: u64) -> Result<()> {
        if self.quiet || !self.show_commands.get() {
            return Ok(());
        }
        let message = format!("... Command still running  {seconds}s");
        if self.terminal {
            // Rewrite a single status line in place instead of appending.
            let message = sanitize_terminal(&redact(&message));
            if self.color {
                eprint!("\r\x1b[2K\x1b[2m{INDENT}{message}\x1b[0m");
            } else {
                eprint!("\r\x1b[2K{INDENT}{message}");
            }
            self.status_line_open.set(true);
            return Ok(());
        }
        self.stderr("command_heartbeat", &message)
    }

    pub fn command_finished(&self, code: Option<i32>, elapsed_ms: u128) -> Result<()> {
        let ok = code == Some(0);
        if self.quiet || (ok && !self.show_commands.get()) {
            return Ok(());
        }
        // Success stays minimal: exit=0 is implied. Failures show the code.
        let message = if ok {
            format!("✓ Command succeeded  {:.2}s", elapsed_ms as f64 / 1000.0)
        } else {
            format!(
                "✗ Command failed  exit={}  {:.2}s",
                code.map_or_else(|| "signal".into(), |v| v.to_string()),
                elapsed_ms as f64 / 1000.0
            )
        };
        self.stderr(
            if ok {
                "command_finished"
            } else {
                "command_failed"
            },
            &message,
        )
    }

    /// Prints the approval prompt exactly once, without a trailing newline so
    /// the user's answer stays on the same line. JSON mode additionally emits
    /// the machine-readable event.
    pub fn approval_prompt(&self, message: &str) -> Result<()> {
        let message = format!("? {message}");
        if self.json {
            self.stderr("approval_required", &message)?;
        }
        if self.status_line_open.replace(false) {
            eprint!("\r\x1b[2K");
        }
        let message = sanitize_terminal(&redact(&message));
        if self.color {
            eprint!("\x1b[33m{INDENT}{message}\x1b[0m");
        } else {
            eprint!("{INDENT}{message}");
        }
        std::io::Write::flush(&mut std::io::stderr())?;
        Ok(())
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
            let answer = sanitize_terminal(&answer);
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                // Render Markdown into readable terminal text; piped output
                // keeps the raw Markdown for downstream tools.
                print!(
                    "{}",
                    crate::markdown::render_for_terminal(&answer, self.color)
                );
            } else {
                println!("{answer}");
            }
        }
        Ok(())
    }

    pub fn success(&self, message: &str) -> Result<()> {
        self.stderr("success", &format!("✓ {message}"))
    }

    pub fn warning(&self, message: &str) -> Result<()> {
        self.stderr("warning", &format!("⚠ {message}"))
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
            // The transient status line (Running / heartbeat) is replaced by the next event.
            if self.status_line_open.replace(false) {
                eprint!("\r\x1b[2K");
            }
            let message = sanitize_terminal(&message);
            let indent = if indented_event(event) { INDENT } else { "" };
            match self.color.then(|| event_color(event)).flatten() {
                Some(code) => eprintln!("\x1b[{code}m{indent}{message}\x1b[0m"),
                None => eprintln!("{indent}{message}"),
            }
        }
        Ok(())
    }
}

/// ANSI color per system event kind; command output itself stays uncolored so
/// it stands apart from qin's own messages.
fn event_color(event: &str) -> Option<&'static str> {
    Some(match event {
        "phase" => "36",                                          // cyan
        "tool_started" | "command_started" => "34",               // blue
        "tool_finished" | "command_finished" | "success" => "32", // green
        "tool_failed" | "command_failed" => "31",                 // red
        "approval_required" | "warning" => "33",                  // yellow
        "command_heartbeat" => "2",                               // dim
        _ => return None,
    })
}

/// Two-column indent for events that belong to a tool/command invocation
/// under the current "● Requesting the model" phase line.
const INDENT: &str = "  ";

fn indented_event(event: &str) -> bool {
    matches!(
        event,
        "tool_started"
            | "tool_finished"
            | "tool_failed"
            | "command_started"
            | "command_output"
            | "command_heartbeat"
            | "command_finished"
            | "command_failed"
            | "approval_required"
    )
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

    #[test]
    fn assigns_colors_to_system_events_only() {
        assert_eq!(event_color("command_finished"), Some("32"));
        assert_eq!(event_color("tool_failed"), Some("31"));
        assert_eq!(event_color("approval_required"), Some("33"));
        assert_eq!(event_color("command_heartbeat"), Some("2"));
        assert_eq!(event_color("command_output"), None);
    }

    #[test]
    fn indents_tool_and_command_events_only() {
        for event in [
            "tool_started",
            "tool_finished",
            "tool_failed",
            "command_started",
            "command_output",
            "command_heartbeat",
            "command_finished",
            "command_failed",
            "approval_required",
        ] {
            assert!(indented_event(event), "{event}");
        }
        for event in ["phase", "success", "session", "final_answer"] {
            assert!(!indented_event(event), "{event}");
        }
    }
}
