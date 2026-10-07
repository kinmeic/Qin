use std::io::{self, IsTerminal, Stdout};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use tokio::sync::watch;
use unicode_width::UnicodeWidthStr;

use crate::event::{self as qin_event, EventSink, TuiEvent};
use crate::state::{StateStore, StoredMessage};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;
const MAX_ENTRY_BYTES: usize = 128 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const DISPLAY_TRUNCATED: &str = "\n[Display truncated]";

enum WorkerRequest {
    Prompt {
        prompt: String,
        approval_mode: ApprovalMode,
        cancellation: watch::Receiver<bool>,
    },
    NewSession,
    Shutdown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApprovalMode {
    Always,
    OnRisk,
    Auto,
    Yolo,
}

impl ApprovalMode {
    fn from_config(policy: &str, assume_yes: bool) -> Self {
        if assume_yes {
            return Self::Auto;
        }
        match policy {
            "always" => Self::Always,
            "never" => Self::Auto,
            _ => Self::OnRisk,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Always => Self::OnRisk,
            Self::OnRisk => Self::Auto,
            Self::Auto => Self::Yolo,
            Self::Yolo => Self::Always,
        }
    }

    fn policy(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::OnRisk => "on_risk",
            Self::Auto | Self::Yolo => "never",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::OnRisk => "on risk",
            Self::Auto => "auto",
            Self::Yolo => "YOLO",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Always => Color::Yellow,
            Self::OnRisk => Color::Green,
            Self::Auto => Color::Yellow,
            Self::Yolo => Color::LightRed,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    User,
    Assistant,
    Activity,
}

struct Entry {
    kind: EntryKind,
    text: String,
}

struct PendingApproval {
    message: String,
    allow_all: bool,
    reply: mpsc::SyncSender<String>,
}

#[derive(Clone, Copy)]
enum Confirmation {
    NewSession,
}

struct App {
    entries: Vec<Entry>,
    streaming_index: Option<usize>,
    last_streamed_answer: Option<String>,
    input: String,
    input_history: Vec<String>,
    history_index: Option<usize>,
    scroll_from_bottom: usize,
    active_command_output: Option<(String, usize)>,
    busy: bool,
    cancellation: Option<watch::Sender<bool>>,
    pending_approval: Option<PendingApproval>,
    confirmation: Option<Confirmation>,
    status: String,
    session_id: String,
    backend: String,
    approval_mode: ApprovalMode,
    active_approval_mode: Option<ApprovalMode>,
    model_name: String,
    context_window: u64,
    context_tokens: Option<u64>,
    context_estimated: bool,
    input_token_usage: u64,
    output_token_usage: u64,
    token_usage_available: bool,
    token_usage_estimated: bool,
    should_exit: bool,
    terminal_suspended: bool,
}

impl App {
    fn new(
        session_id: String,
        backend: String,
        model_name: String,
        context_window: u64,
        messages: Vec<StoredMessage>,
        approval_mode: ApprovalMode,
    ) -> Self {
        let mut entries = Vec::new();
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    if let Some(content) = message.content {
                        entries.push(Entry {
                            kind: EntryKind::User,
                            text: qin_event::sanitize_terminal(&qin_event::redact(&content)),
                        });
                    }
                }
                "assistant" => {
                    if let Some(content) = message.content.filter(|content| !content.is_empty()) {
                        entries.push(Entry {
                            kind: EntryKind::Assistant,
                            text: align_markdown_tables(&qin_event::sanitize_terminal(
                                &qin_event::redact(&content),
                            )),
                        });
                    }
                }
                _ => {}
            }
        }
        if entries.len() > 500 {
            entries.drain(..entries.len() - 500);
        }
        let mut app = Self {
            entries,
            streaming_index: None,
            last_streamed_answer: None,
            input: String::new(),
            input_history: Vec::new(),
            history_index: None,
            scroll_from_bottom: 0,
            active_command_output: None,
            busy: false,
            cancellation: None,
            pending_approval: None,
            confirmation: None,
            status: "Ready".into(),
            session_id,
            backend,
            approval_mode,
            active_approval_mode: None,
            model_name,
            context_window,
            context_tokens: None,
            context_estimated: true,
            input_token_usage: 0,
            output_token_usage: 0,
            token_usage_available: false,
            token_usage_estimated: false,
            should_exit: false,
            terminal_suspended: false,
        };
        for entry in &mut app.entries {
            truncate_display(&mut entry.text);
        }
        app.prune_entries();
        app
    }

    fn handle_key(&mut self, key: KeyEvent, requests: &Sender<WorkerRequest>) {
        if !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && (key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT)))
        {
            self.approval_mode = self.approval_mode.next();
            self.status = format!(
                "Approval: {}{}",
                self.approval_mode.label(),
                if self.busy {
                    " · applies to next turn"
                } else {
                    ""
                },
            );
            return;
        }

        if let Some(approval) = self.pending_approval.as_ref() {
            let answer = match key.code {
                KeyCode::Char('y' | 'Y') => Some("y"),
                KeyCode::Char('a' | 'A') if approval.allow_all => Some("a"),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => Some("n"),
                KeyCode::Enter => Some("n"),
                KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.request_cancel();
                    Some("cancel")
                }
                _ => None,
            };
            if let Some(answer) = answer {
                if let Some(approval) = self.pending_approval.take() {
                    let _ = approval.reply.send(answer.to_string());
                }
                self.status = if answer == "y" || answer == "a" {
                    "Approval granted".into()
                } else {
                    "Approval declined".into()
                };
            }
            return;
        }

        if self.confirmation.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.confirmation = None;
                    self.start_new_session(requests);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter => {
                    self.confirmation = None;
                    self.status = "Cancelled".into();
                }
                _ => {}
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.busy {
                        self.request_cancel();
                    } else {
                        self.should_exit = true;
                    }
                    return;
                }
                KeyCode::Char('d') if !self.busy && self.input.is_empty() => {
                    self.should_exit = true;
                    return;
                }
                KeyCode::Char('j') => {
                    self.input.push('\n');
                    self.scroll_from_bottom = 0;
                    return;
                }
                KeyCode::Char('n') if !self.busy => {
                    self.begin_new_session(requests);
                    return;
                }
                KeyCode::Char('u') => {
                    self.input.clear();
                    return;
                }
                KeyCode::Char('w') => {
                    remove_previous_word(&mut self.input);
                    return;
                }
                KeyCode::Char('l') => {
                    self.scroll_from_bottom = 0;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.push('\n');
                self.scroll_from_bottom = 0;
            }
            KeyCode::Enter => self.submit(requests),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.input.push(character);
                self.history_index = None;
                self.scroll_from_bottom = 0;
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.scroll_from_bottom = 0;
            }
            KeyCode::Up if !self.busy && !self.input.contains('\n') => self.previous_prompt(),
            KeyCode::Down if !self.busy && self.history_index.is_some() => self.next_prompt(),
            KeyCode::PageUp => self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(8),
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(8)
            }
            KeyCode::Esc => self.input.clear(),
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: String) {
        self.input.extend(text.chars().filter(|character| {
            *character == '\n' || *character == '\t' || !character.is_control()
        }));
        self.scroll_from_bottom = 0;
    }

    fn submit(&mut self, requests: &Sender<WorkerRequest>) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        if matches!(prompt.as_str(), "/help" | "/new" | "/exit" | "/quit") {
            self.input.clear();
            self.run_slash_command(&prompt, requests);
            return;
        }
        if self.busy {
            self.status = "Wait for the current turn to finish; Ctrl+C cancels it".into();
            return;
        }
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        if requests
            .send(WorkerRequest::Prompt {
                prompt: prompt.clone(),
                approval_mode: self.approval_mode,
                cancellation: cancel_receiver,
            })
            .is_err()
        {
            self.status = "Agent worker is unavailable".into();
            return;
        }
        self.push_entry(Entry {
            kind: EntryKind::User,
            text: qin_event::sanitize_terminal(&qin_event::redact(&prompt)),
        });
        self.input_history.push(prompt);
        if self.input_history.len() > 100 {
            self.input_history.remove(0);
        }
        self.input.clear();
        self.history_index = None;
        self.busy = true;
        self.active_approval_mode = Some(self.approval_mode);
        self.streaming_index = None;
        self.last_streamed_answer = None;
        self.cancellation = Some(cancel_sender);
        self.input_token_usage = 0;
        self.output_token_usage = 0;
        self.token_usage_available = false;
        self.token_usage_estimated = false;
        self.context_tokens = None;
        self.context_estimated = true;
        self.status = "Working…".into();
        self.scroll_from_bottom = 0;
    }

    fn run_slash_command(&mut self, prompt: &str, requests: &Sender<WorkerRequest>) {
        match prompt.trim() {
            "/help" => self.push_activity(
                "Commands: /new starts a session, /help shows this help, /exit quits. Enter sends; Ctrl+J inserts a newline; Ctrl+C cancels the current turn. Shift+Tab cycles approval: always → on risk → auto → YOLO. Changes apply to the next turn. Auto still asks for high-risk actions; YOLO skips all tool approvals but keeps tool restrictions and forbidden-command rules. Mode changes last for this TUI session.",
            ),
            "/new" => self.begin_new_session(requests),
            "/exit" | "/quit" => {
                if self.busy {
                    self.status = "Wait for the active turn to finish, or press Ctrl+C to cancel".into();
                } else {
                    self.should_exit = true;
                }
            }
            _ => self.push_activity(&format!("Unknown command: {prompt}. Try /help.")),
        }
    }

    fn begin_new_session(&mut self, requests: &Sender<WorkerRequest>) {
        if self.busy {
            self.status = "Wait for the active turn to finish before starting a new session".into();
        } else if self.backend != "sqlite"
            && self
                .entries
                .iter()
                .any(|entry| entry.kind != EntryKind::Activity)
        {
            self.confirmation = Some(Confirmation::NewSession);
        } else {
            self.start_new_session(requests);
        }
    }

    fn start_new_session(&mut self, requests: &Sender<WorkerRequest>) {
        if requests.send(WorkerRequest::NewSession).is_err() {
            self.status = "Agent worker is unavailable".into();
            return;
        }
        self.confirmation = None;
        self.status = "Starting a new session…".into();
        self.busy = true;
    }

    fn request_cancel(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            let _ = cancellation.send(true);
            self.status = "Cancellation requested…".into();
        }
    }

    fn previous_prompt(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let index = self
            .history_index
            .unwrap_or(self.input_history.len())
            .saturating_sub(1);
        self.history_index = Some(index);
        self.input = self.input_history[index].clone();
    }

    fn next_prompt(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.input_history.len() {
            self.history_index = None;
            self.input.clear();
        } else {
            self.history_index = Some(index + 1);
            self.input = self.input_history[index + 1].clone();
        }
    }

    fn push_entry(&mut self, mut entry: Entry) {
        truncate_display(&mut entry.text);
        self.entries.push(entry);
        self.prune_entries();
    }

    fn prune_entries(&mut self) {
        let mut bytes: usize = self.entries.iter().map(|entry| entry.text.len()).sum();
        let mut remove = 0;
        while self.entries.len().saturating_sub(remove) > 500 || bytes > MAX_TRANSCRIPT_BYTES {
            let Some(entry) = self.entries.get(remove) else {
                break;
            };
            bytes = bytes.saturating_sub(entry.text.len());
            remove += 1;
        }
        if remove > 0 {
            self.entries.drain(..remove);
            self.streaming_index = self
                .streaming_index
                .and_then(|index| index.checked_sub(remove));
            self.active_command_output =
                self.active_command_output
                    .take()
                    .and_then(|(tool_call_id, index)| {
                        index.checked_sub(remove).map(|index| (tool_call_id, index))
                    });
        }
    }

    fn push_activity(&mut self, message: &str) {
        self.push_entry(Entry {
            kind: EntryKind::Activity,
            text: qin_event::sanitize_terminal(&qin_event::redact(message)),
        });
        self.scroll_from_bottom = 0;
    }

    fn append_command_output(&mut self, message: &str, data: Option<&serde_json::Value>) {
        let Some(tool_call_id) = data
            .and_then(|data| data.get("tool_call_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            self.push_activity(message);
            return;
        };
        let index = match self.active_command_output.as_ref() {
            Some((active_call_id, index))
                if active_call_id == &tool_call_id && self.entries.get(*index).is_some() =>
            {
                *index
            }
            _ => {
                self.push_entry(Entry {
                    kind: EntryKind::Activity,
                    text: String::new(),
                });
                let index = self.entries.len() - 1;
                self.active_command_output = Some((tool_call_id, index));
                index
            }
        };
        if let Some(entry) = self.entries.get_mut(index)
            && !entry.text.ends_with(DISPLAY_TRUNCATED)
        {
            entry
                .text
                .push_str(&qin_event::sanitize_terminal(&qin_event::redact(message)));
            truncate_display(&mut entry.text);
        }
        self.prune_entries();
        self.scroll_from_bottom = 0;
    }

    fn handle_event(&mut self, event: TuiEvent, screen: &mut ScreenGuard) -> Result<()> {
        let message = event.message;
        match event.event.as_str() {
            "final_answer" => {
                let mut displayed_answer = message.clone();
                truncate_display(&mut displayed_answer);
                if self.last_streamed_answer.as_deref() != Some(displayed_answer.as_str()) {
                    self.push_entry(Entry {
                        kind: EntryKind::Assistant,
                        text: align_markdown_tables(&message),
                    });
                }
                self.streaming_index = None;
                self.last_streamed_answer = None;
                self.status = "Ready".into();
                self.scroll_from_bottom = 0;
            }
            "assistant_stream_start" => {
                self.push_entry(Entry {
                    kind: EntryKind::Assistant,
                    text: String::new(),
                });
                self.streaming_index = Some(self.entries.len() - 1);
                self.last_streamed_answer = None;
                self.scroll_from_bottom = 0;
            }
            "assistant_delta" => {
                if let Some(index) = self.streaming_index {
                    if let Some(entry) = self.entries.get_mut(index)
                        && !entry.text.ends_with(DISPLAY_TRUNCATED)
                    {
                        entry.text.push_str(&message);
                        truncate_display(&mut entry.text);
                    }
                    self.prune_entries();
                    self.scroll_from_bottom = 0;
                }
            }
            "assistant_stream_complete" => {
                if let Some(index) = self.streaming_index.take() {
                    let answer = self.entries[index].text.clone();
                    if answer.is_empty() {
                        self.entries.remove(index);
                    } else {
                        self.entries[index].text = align_markdown_tables(&answer);
                        truncate_display(&mut self.entries[index].text);
                        self.last_streamed_answer = Some(answer);
                    }
                }
            }
            "context_progress" => {
                if let Some(data) = event.data.as_ref() {
                    self.context_tokens =
                        data.get("used_tokens").and_then(serde_json::Value::as_u64);
                    self.context_window = data
                        .get("context_window")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(self.context_window);
                    self.context_estimated = data
                        .get("estimated")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true);
                }
            }
            "model_usage" => {
                if let Some(data) = event.data.as_ref() {
                    self.input_token_usage = self.input_token_usage.saturating_add(
                        data.get("input_tokens")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                    );
                    self.output_token_usage = self.output_token_usage.saturating_add(
                        data.get("output_tokens")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                    );
                    self.context_tokens =
                        data.get("used_tokens").and_then(serde_json::Value::as_u64);
                    self.context_window = data
                        .get("context_window")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(self.context_window);
                    self.context_estimated = data
                        .get("estimated")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true);
                    self.token_usage_available = true;
                    self.token_usage_estimated |= data
                        .get("estimated")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true);
                }
            }
            "approval_required" => {
                let allow_all = event
                    .data
                    .as_ref()
                    .and_then(|data| data.get("allow_all"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if let Some(reply) = event.approval_reply {
                    self.pending_approval = Some(PendingApproval {
                        message: qin_event::sanitize_terminal(&qin_event::redact(&message)),
                        allow_all,
                        reply,
                    });
                    self.status = "Approval required".into();
                } else {
                    self.push_activity(&message);
                }
            }
            "terminal_handoff" | "command_started" => {
                self.active_command_output = None;
                if let Some(ack) = event.terminal_ack {
                    let result = screen.suspend().map_err(|error| format!("{error:#}"));
                    if result.is_ok() {
                        self.terminal_suspended = true;
                        self.status = "Shell command has terminal control".into();
                    }
                    let _ = ack.send(result.clone());
                    result.map_err(anyhow::Error::msg)?;
                }
                if !message.is_empty() {
                    self.push_activity(&message);
                }
            }
            "terminal_resume" | "command_finished" | "command_failed" => {
                self.active_command_output = None;
                self.resume_terminal(screen)?;
                if event.event != "terminal_resume" && !message.is_empty() {
                    self.push_activity(&message);
                }
            }
            "turn_finished" => {
                self.pending_approval = None;
                self.resume_terminal(screen)?;
                self.streaming_index = None;
                self.active_command_output = None;
                self.busy = false;
                self.active_approval_mode = None;
                self.cancellation = None;
                if !message.is_empty() {
                    if message.contains("canceled by the user")
                        || message.contains("cancelled by the user")
                    {
                        self.status = "Cancelled".into();
                        self.push_activity("Turn cancelled by the user");
                    } else {
                        self.status = "Turn ended with an error".into();
                        self.push_activity(&message);
                    }
                } else {
                    self.status = "Ready".into();
                }
            }
            "session_changed" => {
                self.session_id = message;
                self.entries.clear();
                self.streaming_index = None;
                self.last_streamed_answer = None;
                self.active_command_output = None;
                self.status = "New session".into();
                self.scroll_from_bottom = 0;
                self.busy = false;
                self.active_approval_mode = None;
                self.cancellation = None;
                self.input_token_usage = 0;
                self.output_token_usage = 0;
                self.token_usage_available = false;
                self.token_usage_estimated = false;
                self.context_tokens = None;
                self.context_estimated = true;
            }
            "command_output" => self.append_command_output(&message, event.data.as_ref()),
            "tool_failed" => {
                self.active_command_output = None;
                if self.terminal_suspended {
                    self.resume_terminal(screen)?;
                }
                self.push_activity(&message);
            }
            "tool_started" | "tool_finished" | "command_heartbeat" | "phase" | "warning"
            | "tool_warning" | "success" | "approval_decided" => self.push_activity(&message),
            _ => {
                if !message.is_empty() {
                    self.push_activity(&message);
                }
            }
        }
        Ok(())
    }

    fn resume_terminal(&mut self, screen: &mut ScreenGuard) -> Result<()> {
        if self.terminal_suspended {
            screen.resume()?;
            self.terminal_suspended = false;
            self.status = if self.busy {
                "Working…".into()
            } else {
                "Ready".into()
            };
        }
        Ok(())
    }
}

pub async fn run(
    explicit_config: Option<std::path::PathBuf>,
    assume_yes: bool,
    dry_run: bool,
    quiet: bool,
    verbose: bool,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("qin tui requires an interactive terminal on stdin and stdout")
    }

    let startup_events = EventSink::new(quiet, false, verbose);
    let (config, resolver, mut store) = crate::open(&explicit_config, &startup_events)?;
    let cwd = std::env::current_dir()?;
    let session_id = store.ensure_current_session(&cwd)?;
    let initial_messages = store.load_messages(&session_id)?;
    let backend = store.backend_label().to_string();
    let model = config.primary_model()?;
    let model_name = qin_event::sanitize_terminal(&qin_event::redact(&model.model));
    let context_window = model.context_window;
    let approval_mode = ApprovalMode::from_config(&config.permissions.approval, assume_yes);
    let agents_md = crate::load_agents_md(&resolver, &config, &startup_events);

    let (events, event_receiver) = EventSink::new_tui(quiet, verbose || !quiet);
    events.configure(&config.ui);
    let event_sender = events
        .tui_sender()
        .context("Unable to initialize the TUI event channel")?;
    let (request_sender, request_receiver) = mpsc::channel();
    let mut screen = ScreenGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let worker = start_worker(
        config,
        store,
        session_id.clone(),
        agents_md,
        cwd.clone(),
        events,
        event_sender,
        request_receiver,
        dry_run,
    )?;

    let mut app = App::new(
        session_id,
        backend,
        model_name,
        context_window,
        initial_messages,
        approval_mode,
    );
    app.push_activity(&format!("Working directory: {}", cwd.display()));
    let result = event_loop(
        &mut terminal,
        &mut screen,
        &mut app,
        &request_sender,
        &event_receiver,
    );

    if let Some(approval) = app.pending_approval.take() {
        let _ = approval.reply.send("n".into());
    }
    if app.busy
        && let Some(cancellation) = &app.cancellation
    {
        let _ = cancellation.send(true);
    }
    let _ = request_sender.send(WorkerRequest::Shutdown);
    // Release queued approval replies and terminal acknowledgments before joining.
    drop(event_receiver);
    drop(terminal);
    drop(screen);
    if worker.join().is_err() && result.is_ok() {
        bail!("The TUI agent worker stopped unexpectedly")
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn start_worker(
    mut config: crate::config::Config,
    mut store: StateStore,
    mut session_id: String,
    agents_md: Option<String>,
    cwd: std::path::PathBuf,
    events: EventSink,
    event_sender: mpsc::SyncSender<TuiEvent>,
    requests: Receiver<WorkerRequest>,
    dry_run: bool,
) -> Result<thread::JoinHandle<()>> {
    Ok(thread::Builder::new()
        .name("qin-agent".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    send_ui_event(
                        &event_sender,
                        "turn_finished",
                        &format!("Unable to start agent runtime: {error}"),
                        None,
                    );
                    return;
                }
            };
            while let Ok(request) = requests.recv() {
                match request {
                    WorkerRequest::Prompt {
                        prompt,
                        approval_mode,
                        cancellation,
                    } => {
                        // Snapshot the selected policy for the entire turn, including its
                        // system prompt. Later UI changes take effect on the next request.
                        config.permissions.approval = approval_mode.policy().into();
                        config.permissions.yolo = approval_mode == ApprovalMode::Yolo;
                        let result = runtime.block_on(crate::execute_with(
                            &config,
                            &mut store,
                            &session_id,
                            prompt,
                            "tui",
                            None,
                            agents_md.as_deref(),
                            &events,
                            false,
                            dry_run,
                            Some(cancellation),
                        ));
                        let message = result
                            .err()
                            .map(|error| qin_event::redact(&format!("{error:#}")))
                            .unwrap_or_default();
                        send_ui_event(&event_sender, "turn_finished", &message, None);
                    }
                    WorkerRequest::NewSession => {
                        let result = store
                            .new_session(&cwd, Some("New session"))
                            .context("Unable to create a new session");
                        match result {
                            Ok(id) => {
                                session_id = id.clone();
                                send_ui_event(&event_sender, "session_changed", &id, None);
                            }
                            Err(error) => send_ui_event(
                                &event_sender,
                                "turn_finished",
                                &format!("{error:#}"),
                                None,
                            ),
                        }
                    }
                    WorkerRequest::Shutdown => break,
                }
            }
        })?)
}

fn send_ui_event(
    sender: &mpsc::SyncSender<TuiEvent>,
    event: &str,
    message: &str,
    data: Option<serde_json::Value>,
) {
    let _ = sender.send(TuiEvent {
        event: event.into(),
        message: qin_event::sanitize_terminal(&qin_event::redact(message)),
        data,
        approval_reply: None,
        terminal_ack: None,
    });
}

fn event_loop(
    terminal: &mut TuiTerminal,
    screen: &mut ScreenGuard,
    app: &mut App,
    requests: &Sender<WorkerRequest>,
    events: &Receiver<TuiEvent>,
) -> Result<()> {
    while !app.should_exit {
        // Bound each drain so a busy producer cannot starve keys or redraws.
        for _ in 0..128 {
            match events.try_recv() {
                Ok(event) => {
                    let was_active = screen.active;
                    app.handle_event(event, screen)?;
                    if !was_active && screen.active {
                        terminal.clear()?;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    bail!("The TUI agent event channel has closed")
                }
            }
        }
        if app.should_exit {
            break;
        }

        if screen.active {
            if event::poll(Duration::from_millis(40))? {
                match event::read()? {
                    Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                        app.handle_key(key, requests);
                    }
                    Event::Paste(text) => app.handle_paste(text),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            terminal.draw(|frame| draw(frame, app))?;
        } else {
            match events.recv_timeout(Duration::from_millis(40)) {
                Ok(event) => {
                    let was_active = screen.active;
                    app.handle_event(event, screen)?;
                    if !was_active && screen.active {
                        terminal.clear()?;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("The TUI agent event channel has closed")
                }
            }
        }
    }
    Ok(())
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let (input_lines, cursor_column) =
        input_layout(&app.input, area.width.saturating_sub(2).max(1));
    let input_height = input_lines.len().clamp(3, 7) as u16 + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(input_height),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Line::from(vec![
        Span::styled(
            " qin ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}  ", short_id(&app.session_id)),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("Interactive agent"),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        chunks[0],
    );

    let conversation_block = Block::default()
        .title(" Conversation ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = conversation_block.inner(chunks[1]);
    let lines = wrapped_lines(conversation_lines(&app.entries), inner.width.max(1));
    let max_scroll = lines.len().saturating_sub(inner.height as usize);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom);
    let visible: Vec<_> = lines
        .into_iter()
        .skip(scroll)
        .take(inner.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible).block(conversation_block), chunks[1]);

    let input_block = Block::default()
        .title(if app.busy {
            " Message · agent working "
        } else {
            " Message "
        })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let input_inner = input_block.inner(chunks[2]);
    let input_scroll = input_lines
        .len()
        .saturating_sub(input_inner.height as usize);
    let cursor_y = input_inner
        .y
        .saturating_add(
            input_lines
                .len()
                .saturating_sub(input_scroll)
                .saturating_sub(1) as u16,
        )
        .min(input_inner.bottom().saturating_sub(1));
    frame.render_widget(
        Paragraph::new(
            input_lines
                .into_iter()
                .skip(input_scroll)
                .collect::<Vec<_>>(),
        )
        .block(input_block),
        chunks[2],
    );
    if !app.terminal_suspended && input_inner.width > 0 && input_inner.height > 0 {
        frame.set_cursor_position(Position::new(input_inner.x + cursor_column, cursor_y));
    }

    let info_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(chunks[3]);
    let next_turn = app
        .active_approval_mode
        .is_some_and(|mode| mode != app.approval_mode);
    let approval_label = format!(
        "Approval {}{} · ",
        app.approval_mode.label(),
        if next_turn { " (next)" } else { "" },
    );
    let info_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length((approval_label.width() as u16).min(info_rows[0].width / 3)),
            Constraint::Fill(1),
            Constraint::Fill(1),
        ])
        .split(info_rows[0]);
    frame.render_widget(
        Paragraph::new(approval_label).style(Style::default().fg(app.approval_mode.color())),
        info_columns[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Model ", Style::default().fg(Color::DarkGray)),
            Span::styled(app.model_name.clone(), Style::default().fg(Color::Cyan)),
        ])),
        info_columns[1],
    );
    let token_usage = if app.token_usage_available {
        let estimate_marker = if app.token_usage_estimated { "~" } else { "" };
        format!(
            "Tokens this turn: {estimate_marker}{} in · {estimate_marker}{} out",
            format_token_count(app.input_token_usage),
            format_token_count(app.output_token_usage)
        )
    } else {
        "Tokens this turn: waiting".into()
    };
    frame.render_widget(
        Paragraph::new(token_usage).style(Style::default().fg(Color::DarkGray)),
        info_columns[2],
    );

    let context_ratio = app
        .context_tokens
        .map(|tokens| tokens as f64 / app.context_window.max(1) as f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let context_label = if let Some(tokens) = app.context_tokens {
        let estimate_marker = if app.context_estimated { "~" } else { "" };
        let percent = (context_ratio * 100.0).round() as u64;
        format!(
            "Context {estimate_marker}{} / {} tokens ({percent}%)",
            format_token_count(tokens),
            format_token_count(app.context_window)
        )
    } else {
        format!(
            "Context waiting · window {} tokens",
            format_token_count(app.context_window)
        )
    };
    frame.render_widget(
        Gauge::default()
            .ratio(context_ratio)
            .label(context_label)
            .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray)),
        info_rows[1],
    );

    let status_style = if app.busy {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {} ", app.status), status_style),
            Span::styled(
                "Shift+Tab approval · Enter send · Ctrl+J newline · ↑↓ history · PgUp/PgDn scroll · /help",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        chunks[4],
    );

    if let Some(approval) = &app.pending_approval {
        draw_approval(frame, approval, area);
    } else if app.confirmation.is_some() {
        draw_confirmation(frame, area);
    }
}

fn conversation_lines(entries: &[Entry]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        let (label, style) = match entry.kind {
            EntryKind::User => (
                "you ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            EntryKind::Assistant => (
                "qin ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            EntryKind::Activity => ("  · ", Style::default().fg(Color::DarkGray)),
        };
        let mut parts = entry.text.split('\n');
        if let Some(first) = parts.next() {
            lines.push(Line::from(vec![
                Span::styled(label.to_string(), style),
                Span::raw(first.to_string()),
            ]));
        }
        for part in parts {
            lines.push(Line::from(vec![
                Span::raw("    ".to_string()),
                Span::raw(part.to_string()),
            ]));
        }
        lines.push(Line::raw(""));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Describe what you want qin to do.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

fn truncate_display(text: &mut String) {
    if text.len() <= MAX_ENTRY_BYTES {
        return;
    }
    let mut boundary = MAX_ENTRY_BYTES - DISPLAY_TRUNCATED.len();
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(DISPLAY_TRUNCATED);
}

fn align_markdown_tables(text: &str) -> String {
    crate::markdown::align_tables(text)
        .trim_end_matches('\n')
        .to_string()
}

fn wrapped_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut output = Vec::new();
    for line in lines {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut occupied = 0;
        for grapheme in line.styled_graphemes(Style::default()) {
            if grapheme.symbol == "\t" {
                let tab_width = 8 - occupied % 8;
                for _ in 0..tab_width {
                    if occupied == width {
                        output.push(Line::from(std::mem::take(&mut spans)));
                        occupied = 0;
                    }
                    if let Some(last) = spans.last_mut().filter(|span| span.style == grapheme.style)
                    {
                        last.content.to_mut().push(' ');
                    } else {
                        spans.push(Span::styled(" ".to_string(), grapheme.style));
                    }
                    occupied += 1;
                }
                continue;
            }
            let size = UnicodeWidthStr::width(grapheme.symbol);
            if size > width {
                continue;
            }
            if occupied + size > width {
                output.push(Line::from(std::mem::take(&mut spans)));
                occupied = 0;
            }
            if let Some(last) = spans.last_mut().filter(|span| span.style == grapheme.style) {
                last.content.to_mut().push_str(grapheme.symbol);
            } else {
                spans.push(Span::styled(grapheme.symbol.to_string(), grapheme.style));
            }
            occupied += size;
        }
        output.push(Line::from(spans));
    }
    output
}

fn input_layout(text: &str, width: u16) -> (Vec<Line<'static>>, u16) {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut occupied = 0;
    let mut hard_lines = text.split('\n').peekable();
    while let Some(line) = hard_lines.next() {
        for grapheme in Line::raw(line).styled_graphemes(Style::default()) {
            let symbol = if grapheme.symbol == "\t" {
                "    "
            } else {
                grapheme.symbol
            };
            let size = UnicodeWidthStr::width(symbol);
            if occupied + size > width && occupied > 0 {
                lines.push(Line::raw(std::mem::take(&mut current)));
                occupied = 0;
            }
            current.push_str(symbol);
            occupied += size.min(width);
            if occupied >= width {
                lines.push(Line::raw(std::mem::take(&mut current)));
                occupied = 0;
            }
        }
        if !current.is_empty() || line.is_empty() || hard_lines.peek().is_none() {
            lines.push(Line::raw(std::mem::take(&mut current)));
        }
        occupied = 0;
    }
    // The final row's cell width is the cursor position.
    let column = lines.last().map_or(0, |line| line.width()).min(width - 1) as u16;
    (lines, column)
}

fn draw_approval(frame: &mut ratatui::Frame<'_>, approval: &PendingApproval, area: Rect) {
    let modal = centered_rect(72, 38, area);
    frame.render_widget(Clear, modal);
    let mut text = vec![Line::raw(approval.message.clone()), Line::raw("")];
    text.push(Line::from(vec![
        Span::styled(
            "[y] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("Allow once"),
    ]));
    if approval.allow_all {
        text.push(Line::from(vec![
            Span::styled(
                "[a] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Allow ordinary shell commands for this task"),
        ]));
    }
    text.push(Line::from(vec![
        Span::styled(
            "[n] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("Deny"),
    ]));
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .title(" Approval required ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .wrap(Wrap { trim: true }),
        modal,
    );
}

fn draw_confirmation(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let modal = centered_rect(70, 30, area);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw("The active temporary session has no separate history store."),
            Line::raw("Starting a new session replaces its current transcript."),
            Line::raw(""),
            Line::from(vec![
                Span::styled("[y] ", Style::default().fg(Color::Yellow)),
                Span::raw("Continue     "),
                Span::styled("[n] ", Style::default().fg(Color::Green)),
                Span::raw("Keep current session"),
            ]),
        ])
        .block(
            Block::default()
                .title(" Start a new session? ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: true }),
        modal,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn format_token_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn remove_previous_word(input: &mut String) {
    while input.ends_with(char::is_whitespace) {
        input.pop();
    }
    while input.ends_with(|character: char| !character.is_whitespace()) {
        input.pop();
    }
}

struct ScreenGuard {
    active: bool,
}

impl ScreenGuard {
    fn enter() -> Result<Self> {
        let mut guard = Self { active: false };
        enable_raw_mode().context("Unable to enable terminal raw mode")?;
        guard.active = true;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Hide
        )
        .context("Unable to enter the TUI screen")?;
        Ok(guard)
    }

    fn suspend(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode().context("Unable to release terminal raw mode")?;
        execute!(
            io::stdout(),
            LeaveAlternateScreen,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        )
        .context("Unable to release the TUI screen")?;
        self.active = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode().context("Unable to restore terminal raw mode")?;
        self.active = true;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Hide
        )
        .context("Unable to restore the TUI screen")?;
        Ok(())
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                LeaveAlternateScreen,
                crossterm::event::DisableBracketedPaste,
                crossterm::cursor::Show
            );
            self.active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    fn ui_event(name: &str, message: &str) -> TuiEvent {
        TuiEvent {
            event: name.into(),
            message: message.into(),
            data: None,
            approval_reply: None,
            terminal_ack: None,
        }
    }

    #[test]
    fn completed_turn_dismisses_stale_approval() {
        let mut app = app();
        let mut screen = ScreenGuard { active: false };
        let (reply, _receiver) = mpsc::sync_channel(1);
        app.pending_approval = Some(PendingApproval {
            message: "Allow?".into(),
            allow_all: false,
            reply,
        });
        app.busy = true;
        app.handle_event(ui_event("turn_finished", "runtime limit"), &mut screen)
            .unwrap();
        assert!(app.pending_approval.is_none());
        assert!(!app.busy);
    }

    #[test]
    fn streamed_large_answers_are_not_duplicated_at_completion() {
        let mut app = app();
        let mut screen = ScreenGuard { active: false };
        let answer = "a".repeat(MAX_ENTRY_BYTES + 100);
        for event in [
            ui_event("assistant_stream_start", ""),
            ui_event("assistant_delta", &answer),
            ui_event("assistant_stream_complete", ""),
            ui_event("final_answer", &answer),
        ] {
            app.handle_event(event, &mut screen).unwrap();
        }
        assert_eq!(app.entries.len(), 1);
        assert!(app.entries[0].text.ends_with(DISPLAY_TRUNCATED));
    }

    #[test]
    fn restored_answers_and_chunked_command_output_keep_alignment() {
        let table = "| 名称 | 数量 |\n| --- | --- |\n| 中文 | 2 |";
        let restored = App::new(
            "session".into(),
            "sqlite".into(),
            "test".into(),
            4096,
            vec![StoredMessage {
                role: "assistant".into(),
                content: Some(table.into()),
                tool_calls: None,
                tool_call_id: None,
            }],
            ApprovalMode::OnRisk,
        );
        assert_eq!(restored.entries[0].text, align_markdown_tables(table));
        let mut app = app();
        let data = serde_json::json!({"tool_call_id":"command"});
        app.append_command_output("a  ", Some(&data));
        app.push_activity("heartbeat");
        app.append_command_output("b\n中\t2\n", Some(&data));
        assert_eq!(app.entries[0].text, "a  b\n中\t2\n");
    }
    #[test]
    fn long_transcripts_render_the_latest_text_beyond_u16_scroll_limits() {
        let mut app = app();
        app.push_entry(Entry {
            kind: EntryKind::Assistant,
            text: format!("{}END", "a".repeat(70_000)),
        });
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(3, 25)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text: String = (4..16)
            .map(|row| terminal.backend().buffer()[(1, row)].symbol())
            .collect();
        assert!(text.contains("END"), "{text}");
        for _ in 0..30 {
            app.push_activity(&"x".repeat(MAX_ENTRY_BYTES));
        }
        assert!(
            app.entries
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>()
                <= MAX_TRANSCRIPT_BYTES
        );
    }

    use super::*;

    fn app() -> App {
        App::new(
            "session".into(),
            "tmpfs-json".into(),
            "test".into(),
            16_384,
            Vec::new(),
            ApprovalMode::OnRisk,
        )
    }

    #[test]
    fn history_retains_the_latest_hundred_prompts() {
        let mut app = app();
        let (sender, _receiver) = mpsc::channel();
        for index in 0..105 {
            app.busy = false;
            app.input = format!("prompt {index}");
            app.submit(&sender);
        }
        assert_eq!(app.input_history.len(), 100);
        assert_eq!(app.input_history[0], "prompt 5");
        app.previous_prompt();
        assert_eq!(app.input, "prompt 104");
    }

    #[test]
    fn pruning_activity_keeps_the_streaming_entry_index_valid() {
        let mut app = app();
        for _ in 0..499 {
            app.push_activity("activity");
        }
        app.push_entry(Entry {
            kind: EntryKind::Assistant,
            text: "hello".into(),
        });
        app.streaming_index = Some(499);
        for _ in 0..20 {
            app.push_activity("more activity");
        }
        assert_eq!(app.entries.len(), 500);
        assert_eq!(app.entries[app.streaming_index.unwrap()].text, "hello");
    }

    #[test]
    fn absolute_path_prompts_are_sent_and_displayed_with_secrets_masked() {
        let mut app = app();
        let (sender, receiver) = mpsc::channel();
        app.input = "/tmp/config token=secret".into();
        app.submit(&sender);
        match receiver.try_recv().unwrap() {
            WorkerRequest::Prompt { prompt, .. } => assert!(prompt.contains("secret")),
            _ => panic!("Expected a model prompt"),
        }
        assert!(!app.entries[0].text.contains("secret"));
    }

    #[test]
    fn enter_does_not_confirm_temporary_session_replacement() {
        let mut app = app();
        let (sender, receiver) = mpsc::channel();
        app.confirmation = Some(Confirmation::NewSession);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &sender);
        assert!(app.confirmation.is_none());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn input_cursor_accounts_for_wrapping_chinese_and_trailing_newlines() {
        let (lines, column) = input_layout("你好世界a", 6);
        assert_eq!(lines.len(), 2);
        assert_eq!(column, 3);
        let (lines, column) = input_layout("abcdef\n", 6);
        assert_eq!(lines.len(), 2);
        assert_eq!(column, 0);
        let (lines, column) = input_layout("abcdef", 6);
        assert_eq!(lines.len(), 2);
        assert_eq!(column, 0);
        let (lines, column) = input_layout("a\n\n", 6);
        assert_eq!(lines.len(), 3);
        assert_eq!(column, 0);
    }

    #[test]
    fn small_terminal_and_input_footer_render_without_panics() {
        use ratatui::backend::TestBackend;
        let mut app = app();
        app.input = "你好\n".into();
        app.context_tokens = Some(2_000);
        for (width, height) in [(1, 1), (12, 8), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            if width == 100 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains("Model test"));
                assert!(text.contains("Context"));
                assert!(text.contains("Tokens this turn"));
            }
        }
    }
}
