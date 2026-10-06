use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::cell::Cell;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

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
    /// Both stdin and stderr are terminals, so approval answers can stay on
    /// the same line as their prompt. This is separate from status-line
    /// support because Windows and mixed stdin/stderr setups differ.
    approval_inline: bool,
    /// ANSI colors for system event lines (disabled by NO_COLOR).
    color: bool,
    status_line_open: Cell<bool>,
    /// Whether the current command may safely use a transient status line.
    /// Interactive child prompts must never compete with a line rewritten by
    /// qin's heartbeat renderer.
    transient_command_status_enabled: Cell<bool>,
    /// True while a child shell command temporarily owns terminal input.
    terminal_handed_off: Cell<bool>,
    /// Structured events are routed to the interactive renderer instead of
    /// stdout/stderr when qin is running in TUI mode.
    tui_events: Option<Sender<TuiEvent>>,
}

pub struct TuiEvent {
    pub event: String,
    pub message: String,
    pub data: Option<Value>,
    /// The renderer replies to an approval prompt while the agent worker waits.
    pub approval_reply: Option<SyncSender<String>>,
    /// Shell tools wait for the renderer to release the terminal before they
    /// start a child process that inherits terminal input.
    pub terminal_ack: Option<SyncSender<std::result::Result<(), String>>>,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    event: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl EventSink {
    pub fn new(quiet: bool, json: bool, verbose: bool) -> Self {
        let stdin_terminal = std::io::IsTerminal::is_terminal(&std::io::stdin());
        let stderr_terminal = std::io::IsTerminal::is_terminal(&std::io::stderr());
        let terminal = !json && !cfg!(windows) && stderr_terminal;
        let approval_inline = approval_prompt_is_inline(json, stdin_terminal, stderr_terminal);
        let color = terminal && std::env::var_os("NO_COLOR").is_none();
        Self {
            quiet,
            json,
            verbose,
            show_tool_events: Cell::new(true),
            show_commands: Cell::new(true),
            terminal,
            approval_inline,
            color,
            status_line_open: Cell::new(false),
            transient_command_status_enabled: Cell::new(true),
            terminal_handed_off: Cell::new(false),
            tui_events: None,
        }
    }

    pub fn new_tui(quiet: bool, verbose: bool) -> (Self, Receiver<TuiEvent>) {
        let (sender, receiver) = mpsc::channel();
        let mut sink = Self::new(quiet, false, verbose);
        sink.tui_events = Some(sender);
        (sink, receiver)
    }

    pub fn tui_sender(&self) -> Option<Sender<TuiEvent>> {
        self.tui_events.clone()
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

    pub fn terminal_handed_off(&self) -> bool {
        self.terminal_handed_off.get()
    }

    pub fn is_tui(&self) -> bool {
        self.tui_events.is_some()
    }

    pub fn phase(&self, message: &str) -> Result<()> {
        if !self.quiet && self.show_tool_events.get() {
            self.stderr("phase", &format!("● {message}"))?;
        }
        Ok(())
    }

    pub fn tool_started(&self, name: &str, detail: &str) -> Result<()> {
        self.tool_started_with_data(name, detail, None)
    }

    /// Emits a tool start with renderer-only metadata. The metadata is never
    /// sent to the model and is intentionally limited to safe locations,
    /// sizes, and presentation hints rather than file contents.
    pub fn tool_started_with_data(
        &self,
        name: &str,
        detail: &str,
        data: Option<Value>,
    ) -> Result<()> {
        if !self.quiet && self.show_tool_events.get() {
            self.stderr_with_data("tool_started", &format!("→ {name}  {detail}"), data)?;
        }
        Ok(())
    }

    pub fn tool_finished_with_data(
        &self,
        name: &str,
        summary: &str,
        elapsed_ms: u128,
        data: Option<Value>,
    ) -> Result<()> {
        if !self.quiet && self.show_tool_events.get() {
            self.stderr_with_data(
                "tool_finished",
                &tool_finished_message(name, summary, elapsed_ms),
                data,
            )?;
        }
        Ok(())
    }

    pub fn tool_failed_with_data(
        &self,
        name: &str,
        error: &str,
        elapsed_ms: u128,
        data: Option<Value>,
    ) -> Result<()> {
        self.terminal_handed_off.set(false);
        self.stderr_with_data(
            "tool_failed",
            &format!("✗ {name}  {error}  {}", format_elapsed(elapsed_ms)),
            data,
        )
    }

    pub fn command_started_with_data(
        &self,
        cwd: &std::path::Path,
        elevated: bool,
        timeout: u64,
        interactive_terminal: bool,
        child_can_prompt: bool,
        data: Option<Value>,
    ) -> Result<()> {
        self.transient_command_status_enabled
            .set(transient_command_status_allowed(
                self.terminal,
                interactive_terminal,
                child_can_prompt,
            ));
        let level = if elevated {
            "sudo/root"
        } else {
            "standard privileges"
        };
        let message = format!(
            "▸ Running [{level}]  cwd={}  timeout={}",
            cwd.display(),
            format_elapsed(timeout as u128 * 1_000)
        );
        if self.tui_events.is_some() {
            if child_can_prompt {
                self.terminal_handed_off.set(true);
            }
            let result = if !self.quiet && self.show_commands.get() {
                self.emit_tui_event(
                    "command_started",
                    &message,
                    data.clone(),
                    None,
                    child_can_prompt,
                )
            } else if child_can_prompt {
                self.emit_tui_event("terminal_handoff", "", data.clone(), None, true)
            } else {
                Ok(())
            };
            if result.is_err() {
                self.terminal_handed_off.set(false);
            }
            result?;
            return Ok(());
        }
        if self.quiet || !self.show_commands.get() {
            return Ok(());
        }
        // The command itself was already shown by tool_started; here we
        // only report the execution context as the run begins.
        if transient_command_status_allowed(self.terminal, interactive_terminal, child_can_prompt) {
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
        self.stderr_with_data("command_started", &message, data)
    }

    pub fn command_output_with_data(
        &self,
        stream: &str,
        line: &str,
        data: Option<Value>,
    ) -> Result<()> {
        if self.tui_events.is_some() {
            if self.terminal_handed_off.get() {
                let output = sanitize_terminal(&redact(line));
                match stream {
                    "stderr" => eprint!("{output}"),
                    _ => print!("{output}"),
                }
                std::io::Write::flush(&mut std::io::stdout())?;
                std::io::Write::flush(&mut std::io::stderr())?;
            } else {
                self.emit_tui_event(
                    "command_output",
                    &format!("│ {stream}: {}", redact(line)),
                    data,
                    None,
                    false,
                )?;
            }
            return Ok(());
        }
        // Live command output is hidden unless --verbose; JSON consumers
        // always receive it as structured events.
        if self.json || (self.verbose && !self.quiet && self.show_commands.get()) {
            self.stderr_with_data(
                "command_output",
                &format!("│ {stream}: {}", redact(line)),
                data,
            )?;
        }
        Ok(())
    }

    pub fn command_heartbeat_with_data(&self, seconds: u64, data: Option<Value>) -> Result<()> {
        if self.quiet || !self.show_commands.get() {
            return Ok(());
        }
        let message = format!(
            "... Command still running  {}",
            format_elapsed(seconds as u128 * 1_000)
        );
        if self.terminal && self.transient_command_status_enabled.get() {
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
        self.stderr_with_data("command_heartbeat", &message, data)
    }

    pub fn command_finished_with_data(
        &self,
        code: Option<i32>,
        elapsed_ms: u128,
        data: Option<Value>,
    ) -> Result<()> {
        self.transient_command_status_enabled.set(true);
        self.terminal_handed_off.set(false);
        let ok = code == Some(0);
        if self.tui_events.is_some() && (self.quiet || (ok && !self.show_commands.get())) {
            self.emit_tui_event("terminal_resume", "", data, None, false)?;
            return Ok(());
        }
        if self.quiet || (ok && !self.show_commands.get()) {
            return Ok(());
        }
        // Success stays minimal: exit=0 is implied. Failures show the code.
        let message = if ok {
            format!("✓ Command succeeded  {}", format_elapsed(elapsed_ms))
        } else {
            format!(
                "✗ Command failed  exit={}  {}",
                code.map_or_else(|| "signal".into(), |v| v.to_string()),
                format_elapsed(elapsed_ms)
            )
        };
        self.stderr_with_data(
            if ok {
                "command_finished"
            } else {
                "command_failed"
            },
            &message,
            data,
        )
    }

    /// Prints the approval prompt exactly once. Interactive terminals keep the
    /// user's answer on the same line; event-stream and JSON output use a
    /// complete line so consumers can render the whole prompt.
    pub fn approval_prompt(&self, message: &str) -> Result<()> {
        self.approval_prompt_with_data(message, None).map(|_| ())
    }

    /// Emits an approval prompt and, for structured consumers, the stable
    /// request metadata needed to attach it to a tool call.
    pub fn approval_prompt_with_data(
        &self,
        message: &str,
        data: Option<Value>,
    ) -> Result<Option<String>> {
        let message = format!("? {message}");
        if self.tui_events.is_some() {
            let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
            self.emit_tui_event(
                "approval_required",
                &message,
                data,
                Some(reply_sender),
                false,
            )?;
            return Ok(Some(
                reply_receiver
                    .recv()
                    .map_err(|_| anyhow::anyhow!("The TUI closed during approval"))?,
            ));
        }
        if !self.approval_inline {
            self.stderr_with_data("approval_required", &message, data)?;
            return Ok(None);
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
        Ok(None)
    }

    /// Notifies structured/non-interactive consumers that an approval request
    /// has reached a closed outcome. Interactive TTYs do not need an extra
    /// line after the user's answer.
    pub fn approval_decided(
        &self,
        approval_id: &str,
        tool_call_id: &str,
        outcome: &str,
    ) -> Result<()> {
        if self.approval_inline {
            return Ok(());
        }
        self.stderr_with_data(
            "approval_decided",
            &format!("Approval {outcome}"),
            Some(serde_json::json!({
                "approval_id": approval_id,
                "tool_call_id": tool_call_id,
                "outcome": outcome,
            })),
        )
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
        if self.tui_events.is_some() {
            return self.emit_tui_event("final_answer", &answer, None, None, false);
        }
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

    pub fn assistant_stream_start(&self) -> Result<()> {
        if self.tui_events.is_some() {
            self.emit_tui_event("assistant_stream_start", "", None, None, false)?;
        }
        Ok(())
    }

    pub fn assistant_delta(&self, text: &str) -> Result<()> {
        if self.tui_events.is_some() && !text.is_empty() {
            self.emit_tui_event("assistant_delta", text, None, None, false)?;
        }
        Ok(())
    }

    pub fn assistant_stream_complete(&self) -> Result<()> {
        if self.tui_events.is_some() {
            self.emit_tui_event("assistant_stream_complete", "", None, None, false)?;
        }
        Ok(())
    }

    pub fn context_progress(&self, used_tokens: u64, context_window: u64) -> Result<()> {
        if self.tui_events.is_some() {
            self.emit_tui_event(
                "context_progress",
                "",
                Some(serde_json::json!({
                    "used_tokens": used_tokens,
                    "context_window": context_window,
                    "estimated": true,
                })),
                None,
                false,
            )?;
        }
        Ok(())
    }

    pub fn model_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
        estimated: bool,
    ) -> Result<()> {
        if self.tui_events.is_some() {
            self.emit_tui_event(
                "model_usage",
                "",
                Some(serde_json::json!({
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "used_tokens": input_tokens.saturating_add(output_tokens),
                    "context_window": context_window,
                    "estimated": estimated,
                })),
                None,
                false,
            )?;
        }
        Ok(())
    }

    pub fn success(&self, message: &str) -> Result<()> {
        self.stderr("success", &format!("✓ {message}"))
    }

    pub fn warning(&self, message: &str) -> Result<()> {
        self.stderr("warning", &format!("⚠ {message}"))
    }

    /// A warning that belongs to the current tool/command invocation, so it
    /// is indented under the phase line like tool and command events.
    pub fn tool_warning(&self, message: &str) -> Result<()> {
        self.stderr("tool_warning", &format!("⚠ {message}"))
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
        self.stderr_with_data(event, message, None)
    }

    fn stderr_with_data(&self, event: &str, message: &str, data: Option<Value>) -> Result<()> {
        let message = redact(message);
        if self.tui_events.is_some() {
            return self.emit_tui_event(event, &message, data, None, false);
        }
        if self.json {
            eprintln!(
                "{}",
                serde_json::to_string(&JsonEvent {
                    event,
                    message: &message,
                    data,
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

    fn emit_tui_event(
        &self,
        event: &str,
        message: &str,
        data: Option<Value>,
        approval_reply: Option<SyncSender<String>>,
        wait_for_terminal_ack: bool,
    ) -> Result<()> {
        let Some(sender) = &self.tui_events else {
            return Ok(());
        };
        let (terminal_ack, ack_receiver) = if wait_for_terminal_ack {
            let (ack_sender, ack_receiver) = mpsc::sync_channel(1);
            (Some(ack_sender), Some(ack_receiver))
        } else {
            (None, None)
        };
        sender
            .send(TuiEvent {
                event: event.to_string(),
                message: sanitize_terminal(&redact(message)),
                data,
                approval_reply,
                terminal_ack,
            })
            .map_err(|_| anyhow::anyhow!("The TUI event receiver has closed"))?;
        if let Some(receiver) = ack_receiver {
            receiver
                .recv()
                .map_err(|_| anyhow::anyhow!("The TUI closed before releasing the terminal"))?
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

fn format_elapsed(elapsed_ms: u128) -> String {
    const CENTISECONDS_PER_MINUTE: u128 = 6_000;
    const CENTISECONDS_PER_HOUR: u128 = 60 * CENTISECONDS_PER_MINUTE;

    // Round once before splitting into units so values such as 59.999s
    // normalize to 1m 00.00s rather than producing an invalid 60.00s field.
    let centiseconds = elapsed_ms.saturating_add(5) / 10;
    let hours = centiseconds / CENTISECONDS_PER_HOUR;
    let after_hours = centiseconds % CENTISECONDS_PER_HOUR;
    let minutes = after_hours / CENTISECONDS_PER_MINUTE;
    let after_minutes = after_hours % CENTISECONDS_PER_MINUTE;
    let seconds = after_minutes / 100;
    let hundredths = after_minutes % 100;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}.{hundredths:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}.{hundredths:02}s")
    } else {
        format!("{seconds}.{hundredths:02}s")
    }
}

fn tool_finished_message(name: &str, summary: &str, elapsed_ms: u128) -> String {
    let elapsed = format_elapsed(elapsed_ms);
    if summary.is_empty() {
        format!("✓ {name}  {elapsed}")
    } else {
        format!("✓ {name}  {elapsed}  {summary}")
    }
}

fn approval_prompt_is_inline(json: bool, stdin_terminal: bool, stderr_terminal: bool) -> bool {
    !json && stdin_terminal && stderr_terminal
}

fn transient_command_status_allowed(
    terminal: bool,
    interactive_terminal: bool,
    child_can_prompt: bool,
) -> bool {
    terminal && !interactive_terminal && !child_can_prompt
}

/// ANSI color per system event kind; command output itself stays uncolored so
/// it stands apart from qin's own messages.
fn event_color(event: &str) -> Option<&'static str> {
    Some(match event {
        "phase" => "36",                                          // cyan
        "tool_started" | "command_started" => "34",               // blue
        "tool_finished" | "command_finished" | "success" => "32", // green
        "tool_failed" | "command_failed" => "31",                 // red
        "approval_required" | "approval_decided" | "warning" | "tool_warning" => "33", // yellow
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
            | "approval_decided"
            | "tool_warning"
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

const SECRET_MARKERS: &[&str] = &[
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
    "token\":",
    "password\":",
    "api_key\":",
    "api-key\":",
    "authorization\":",
    "token':",
    "password':",
    "api_key':",
    "api-key':",
    "authorization':",
];

// Byte offsets always originate from ASCII markers or UTF-8 character boundaries.
// An incomplete value is retained by the streaming redactor until it terminates.
fn secret_ranges(value: &str) -> Vec<(usize, usize, usize, bool)> {
    let lower = value.to_ascii_lowercase();
    let mut ranges = Vec::new();
    for &marker in SECRET_MARKERS {
        for (found, _) in lower.match_indices(marker) {
            if marker == "sk-"
                && found > 0
                && value[..found]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
            {
                continue;
            }
            let mut start = if marker == "sk-" {
                found
            } else {
                found + marker.len()
            };
            if marker != "sk-" {
                start += value[start..].len() - value[start..].trim_start().len();
                if marker.starts_with("authorization") && lower[start..].starts_with("bearer ") {
                    start += "bearer ".len();
                    start += value[start..].len() - value[start..].trim_start().len();
                }
            }
            let quote = value[start..]
                .chars()
                .next()
                .filter(|c| matches!(c, '\'' | '"'));
            if let Some(quote) = quote {
                start += quote.len_utf8();
            }
            let end = value[start..]
                .char_indices()
                .find(|(index, c)| match quote {
                    Some(quote) => {
                        *c == quote
                            && value[start..start + index]
                                .bytes()
                                .rev()
                                .take_while(|byte| *byte == b'\\')
                                .count()
                                % 2
                                == 0
                    }
                    None => c.is_whitespace() || matches!(c, '&' | '\'' | '"' | ',' | '}' | ']'),
                })
                .map_or(value.len(), |(index, _)| start + index);
            let incomplete = end == value.len();
            if marker == "sk-" && end - start < 20 && !incomplete {
                continue;
            }
            ranges.push((found, start, end, incomplete));
        }
    }
    ranges
}

pub fn redact(value: &str) -> String {
    let mut ranges: Vec<_> = secret_ranges(value)
        .into_iter()
        .filter(|(found, start, end, _)| {
            start < end
                && !(value[*found..].to_ascii_lowercase().starts_with("sk-") && end - start < 20)
                && !value[*start..].starts_with("[REDACTED]")
        })
        .map(|(_, start, end, _)| (start, end))
        .collect();
    ranges.sort_unstable();
    let mut output = String::new();
    let mut offset = 0;
    for (start, end) in ranges {
        if end <= offset {
            continue;
        }
        if start >= offset {
            output.push_str(&value[offset..start]);
            output.push_str("[REDACTED]");
        }
        offset = end;
    }
    output.push_str(&value[offset..]);
    output
}

#[derive(Default)]
pub(crate) struct StreamRedactor {
    pending: String,
    suppressed: bool,
}

impl StreamRedactor {
    pub(crate) fn push(&mut self, text: &str) -> String {
        if self.suppressed {
            return String::new();
        }
        self.pending.push_str(text);
        let lower = self.pending.to_ascii_lowercase();
        let mut end = self.pending.len();
        for &marker in SECRET_MARKERS {
            for length in 1..marker.len() {
                if lower.ends_with(&marker[..length]) {
                    end = end.min(lower.len() - length);
                }
            }
        }
        for (found, _, _, incomplete) in secret_ranges(&self.pending) {
            if incomplete {
                end = end.min(found);
            }
        }
        // Retain the preceding character for the sk- word-boundary check.
        if end < self.pending.len() {
            end = self.pending[..end]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
        }
        let ready = redact(&self.pending[..end]);
        self.pending.drain(..end);
        if self.pending.len() > 65_536 {
            self.pending.clear();
            self.suppressed = true;
            return format!("{ready}[REDACTED oversized stream; further output suppressed]");
        }
        ready
    }

    pub(crate) fn finish(&mut self) -> String {
        let ready = redact(&self.pending);
        self.pending.clear();
        ready
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn redacts_spaced_quoted_and_unicode_secret_values() {
        for text in [
            "password: secret",
            "token=\"secret with spaces\"",
            r#"{"token": "secret with spaces", "password":"secret\"value"}"#,
            "api_key:\u{3000}秘密",
            "Authorization: Bearer secret",
        ] {
            let masked = redact(text);
            assert!(
                !masked.contains("secret") && !masked.contains("秘密"),
                "{masked}"
            );
            assert!(masked.contains("[REDACTED]"));
        }
        assert_eq!(redact("task-123 sk-short"), "task-123 sk-short");
    }

    #[test]
    fn streaming_redaction_is_independent_of_chunk_boundaries() {
        for input in [
            "hello password: secret bye",
            "token=\"secret value\" end",
            "Authorization: Bearer secret end",
            "密钥 api_key:\u{3000}秘密 结束",
            "sk-123456789012345678901234567890 end",
        ] {
            for boundary in input.char_indices().map(|(i, _)| i).chain([input.len()]) {
                let mut redactor = StreamRedactor::default();
                let mut output = redactor.push(&input[..boundary]);
                output.push_str(&redactor.push(&input[boundary..]));
                output.push_str(&redactor.finish());
                assert_eq!(output, redact(input), "boundary={boundary}, input={input}");
            }
            let mut redactor = StreamRedactor::default();
            let mut output = String::new();
            for character in input.chars() {
                output.push_str(&redactor.push(&character.to_string()));
            }
            output.push_str(&redactor.finish());
            assert_eq!(output, redact(input));
        }
    }

    #[test]
    fn streaming_redaction_bounds_unterminated_secrets() {
        let mut redactor = StreamRedactor::default();
        let output = redactor.push(&format!("token={}", "s".repeat(70_000)));
        assert!(output.contains("REDACTED") && redactor.pending.is_empty());
        assert!(redactor.push("more secret").is_empty());
    }

    #[test]
    fn abandoned_tui_approval_and_terminal_handoff_unblock_worker() {
        for handoff in [false, true] {
            let (sink, receiver) = EventSink::new_tui(true, false);
            let (done, completion) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = if handoff {
                    sink.emit_tui_event("terminal_handoff", "", None, None, true)
                } else {
                    sink.approval_prompt("Allow?").map(|_| ())
                };
                done.send(result.is_err()).unwrap();
            });
            let event = receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            drop(event);
            drop(receiver);
            assert!(
                completion
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
            );
            worker.join().unwrap();
        }
    }
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
        assert_eq!(event_color("approval_decided"), Some("33"));
        assert_eq!(event_color("tool_warning"), Some("33"));
        assert_eq!(event_color("command_heartbeat"), Some("2"));
        assert_eq!(event_color("command_output"), None);
    }

    #[test]
    fn indents_tool_and_command_events_only() {
        for event in [
            "tool_started",
            "tool_finished",
            "tool_failed",
            "tool_warning",
            "command_started",
            "command_output",
            "command_heartbeat",
            "command_finished",
            "command_failed",
            "approval_required",
            "approval_decided",
        ] {
            assert!(indented_event(event), "{event}");
        }
        for event in ["phase", "success", "session", "final_answer"] {
            assert!(!indented_event(event), "{event}");
        }
    }

    #[test]
    fn formats_elapsed_times_in_seconds_minutes_and_hours() {
        assert_eq!(format_elapsed(0), "0.00s");
        assert_eq!(format_elapsed(247), "0.25s");
        assert_eq!(format_elapsed(1_862), "1.86s");
        assert_eq!(format_elapsed(59_994), "59.99s");
        assert_eq!(format_elapsed(59_999), "1m 00.00s");
        assert_eq!(format_elapsed(60_000), "1m 00.00s");
        assert_eq!(format_elapsed(125_180), "2m 05.18s");
        assert_eq!(format_elapsed(3_600_000), "1h 00m 00.00s");
        assert_eq!(format_elapsed(3_787_420), "1h 03m 07.42s");
    }

    #[test]
    fn tool_completion_puts_elapsed_time_before_summary() {
        assert_eq!(
            tool_finished_message("web_search", "8 results", 1_699),
            "✓ web_search  1.70s  8 results"
        );
        assert_eq!(
            tool_finished_message("read_file", "", 247),
            "✓ read_file  0.25s"
        );
    }

    #[test]
    fn approval_prompts_are_inline_only_for_plain_interactive_terminals() {
        assert!(approval_prompt_is_inline(false, true, true));
        assert!(!approval_prompt_is_inline(false, false, true));
        assert!(!approval_prompt_is_inline(false, true, false));
        assert!(!approval_prompt_is_inline(false, false, false));
        assert!(!approval_prompt_is_inline(true, true, true));
    }

    #[test]
    fn transient_command_status_is_disabled_when_child_can_prompt() {
        assert!(transient_command_status_allowed(true, false, false));
        assert!(!transient_command_status_allowed(true, false, true));
        assert!(!transient_command_status_allowed(true, true, false));
        assert!(!transient_command_status_allowed(false, false, false));
    }

    #[test]
    fn structured_event_data_is_present_only_when_supplied() {
        let with_data = serde_json::to_value(JsonEvent {
            event: "approval_required",
            message: "? Modify file requirements.txt? [y/N] ",
            data: Some(serde_json::json!({"approval_id": "a-1"})),
        })
        .unwrap();
        assert_eq!(with_data["data"]["approval_id"], "a-1");

        let without_data = serde_json::to_value(JsonEvent {
            event: "tool_finished",
            message: "done",
            data: None,
        })
        .unwrap();
        assert!(without_data.get("data").is_none());
    }
}
