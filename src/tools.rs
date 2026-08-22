use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

use crate::approval::{ApprovalOutcome, ApprovalRequest};
use crate::checkpoint::Recorder;
use crate::config::Config;
use crate::event::{EventSink, redact};
use crate::knowledge;
use crate::state::StateStore;

pub struct ToolContext<'a> {
    pub config: &'a Config,
    pub events: &'a EventSink,
    pub store: &'a mut StateStore,
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub cwd: &'a Path,
    pub assume_yes: bool,
    pub approve_all_commands: &'a mut bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub exit_code: Option<i32>,
    pub completion_summary: Option<String>,
    /// Renderer-only metadata. The model receives only `content`; this value
    /// is safe for event consumers because it contains locations, sizes, and
    /// status hints rather than file contents or raw commands.
    pub presentation: Option<Value>,
}

#[derive(Clone, Copy)]
enum ToolAvailability {
    Always,
    WorkspaceWrite,
    Shell,
    Knowledge,
    KnowledgeWrite,
    WebSearch,
}

impl ToolAvailability {
    fn enabled(self, config: &Config) -> bool {
        match self {
            Self::Always => true,
            Self::WorkspaceWrite => config.permissions.workspace_write,
            Self::Shell => config.permissions.allow_shell,
            Self::Knowledge => config.knowledge_active(),
            Self::KnowledgeWrite => config.knowledge_active() && config.permissions.workspace_write,
            Self::WebSearch => {
                config.search.exa.enabled
                    || config.search.brave.enabled
                    || config.search.native.enabled
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ToolHandler {
    ListDirectory,
    ReadFile,
    StatPath,
    CreateDirectory,
    WriteFile,
    MovePath,
    CopyPath,
    RemovePath,
    ApplyPatch,
    Shell,
    SearchMemory,
    SaveMemory,
    WebSearch,
}

#[derive(Clone)]
struct ToolDefinition {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    allowed_keys: &'static [&'static str],
    availability: ToolAvailability,
    handler: ToolHandler,
}

fn tool_registry() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "list_directory",
            description: "List directory contents",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            allowed_keys: &["path"],
            availability: ToolAvailability::Always,
            handler: ToolHandler::ListDirectory,
        },
        ToolDefinition {
            name: "read_file",
            description: "Read a UTF-8 text file",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"max_bytes":{"type":"integer"}},"required":["path"]}),
            allowed_keys: &["path", "max_bytes"],
            availability: ToolAvailability::Always,
            handler: ToolHandler::ReadFile,
        },
        ToolDefinition {
            name: "stat_path",
            description: "Inspect a path's type, size, and permissions",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            allowed_keys: &["path"],
            availability: ToolAvailability::Always,
            handler: ToolHandler::StatPath,
        },
        ToolDefinition {
            name: "create_directory",
            description: "Create a directory",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            allowed_keys: &["path"],
            availability: ToolAvailability::WorkspaceWrite,
            handler: ToolHandler::CreateDirectory,
        },
        ToolDefinition {
            name: "write_file",
            description: "Write a UTF-8 file, replacing existing content; prefer apply_patch for localized edits",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
            allowed_keys: &["path", "content"],
            availability: ToolAvailability::WorkspaceWrite,
            handler: ToolHandler::WriteFile,
        },
        ToolDefinition {
            name: "move_path",
            description: "Move or rename a file or directory",
            parameters: json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"},"overwrite":{"type":"boolean"}},"required":["source","destination"]}),
            allowed_keys: &["source", "destination", "overwrite"],
            availability: ToolAvailability::WorkspaceWrite,
            handler: ToolHandler::MovePath,
        },
        ToolDefinition {
            name: "copy_path",
            description: "Copy a file",
            parameters: json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"},"overwrite":{"type":"boolean"}},"required":["source","destination"]}),
            allowed_keys: &["source", "destination", "overwrite"],
            availability: ToolAvailability::WorkspaceWrite,
            handler: ToolHandler::CopyPath,
        },
        ToolDefinition {
            name: "remove_path",
            description: "Delete a file or directory (dangerous operation)",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"recursive":{"type":"boolean"}},"required":["path"]}),
            allowed_keys: &["path", "recursive"],
            availability: ToolAvailability::WorkspaceWrite,
            handler: ToolHandler::RemovePath,
        },
        ToolDefinition {
            name: "apply_patch",
            description: "Inspect an existing UTF-8 file, then replace old_text exactly once with new_text; if it is not unique, reread with more context instead of guessing; preserve unrelated content and verify the result",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}),
            allowed_keys: &["path", "old_text", "new_text"],
            availability: ToolAvailability::WorkspaceWrite,
            handler: ToolHandler::ApplyPatch,
        },
        ToolDefinition {
            name: "shell",
            description: "Run a shell command; approval is risk-based, policy denials and rejected approvals are final for this command, and workarounds are not allowed; interactive commands must run directly, never through timeout/setsid/nohup wrappers, because qin rejects wrappers that can detach terminal input; use timeout_seconds instead",
            parameters: json!({"type":"object","properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer"},"elevated":{"type":"boolean"}},"required":["command"]}),
            allowed_keys: &["command", "timeout_seconds", "elevated"],
            availability: ToolAvailability::Shell,
            handler: ToolHandler::Shell,
        },
        ToolDefinition {
            name: "search_memory",
            description: "Semantically search long-term memory",
            parameters: json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}),
            allowed_keys: &["query", "limit"],
            availability: ToolAvailability::Knowledge,
            handler: ToolHandler::SearchMemory,
        },
        ToolDefinition {
            name: "save_memory",
            description: "Save a user preference, fact, or reusable procedure to long-term memory",
            parameters: json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"]}),
            allowed_keys: &["content"],
            availability: ToolAvailability::KnowledgeWrite,
            handler: ToolHandler::SaveMemory,
        },
        ToolDefinition {
            name: "web_search",
            description: "Search the internet using Exa, then Brave, then model-native search",
            parameters: json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}),
            allowed_keys: &["query", "limit"],
            availability: ToolAvailability::WebSearch,
            handler: ToolHandler::WebSearch,
        },
    ]
}

fn find_tool_definition(name: &str) -> Option<ToolDefinition> {
    tool_registry()
        .into_iter()
        .find(|definition| definition.name == name)
}

pub fn definitions(config: &Config) -> Vec<Value> {
    tool_registry()
        .into_iter()
        .filter(|definition| definition.availability.enabled(config))
        .map(|definition| {
            tool(
                definition.name,
                definition.description,
                definition.parameters,
            )
        })
        .collect()
}

fn tool(name: &str, description: &str, mut parameters: Value) -> Value {
    if let Some(object) = parameters.as_object_mut() {
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
    json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
}

pub async fn execute(
    call_id: &str,
    name: &str,
    arguments: &str,
    ctx: &mut ToolContext<'_>,
) -> Result<ToolResult> {
    let prepared = prepare_tool(name, arguments, ctx)?;
    let started = Instant::now();
    ctx.events.tool_started_with_data(
        name,
        &display_args(name, &prepared.args),
        Some(prepared.presentation.clone()),
    )?;
    let authorization = authorize_tool(name, &prepared.definition, ctx);
    let mut result = match authorization {
        Ok(()) => dispatch_tool(prepared.definition.handler, &prepared.args, ctx).await,
        Err(error) => Err(error),
    };
    normalize_result(&mut result, ctx.config.permissions.max_output_bytes);
    finalize_result(
        call_id,
        name,
        &prepared.args,
        ctx.cwd,
        &mut result,
        started.elapsed().as_millis() as u64,
    );
    observe_tool(
        call_id,
        name,
        &prepared.args,
        &prepared.audit_args,
        &result,
        started,
        ctx,
    )?;
    result
}

struct PreparedTool {
    definition: ToolDefinition,
    args: Value,
    audit_args: String,
    presentation: Value,
}

fn prepare_tool(name: &str, arguments: &str, ctx: &ToolContext<'_>) -> Result<PreparedTool> {
    let definition = find_tool_definition(name).with_context(|| format!("Unknown tool: {name}"))?;
    let args: Value = serde_json::from_str(arguments)
        .with_context(|| format!("Arguments for tool {name} are not valid JSON"))?;
    if !args.is_object() {
        bail!("Arguments for tool {name} must be a JSON object");
    }
    validate_argument_keys(name, &args)?;
    Ok(PreparedTool {
        presentation: tool_presentation(ctx.tool_call_id, name, &args, ctx.cwd),
        audit_args: truncate(safe_args(name, &args), 8_192),
        definition,
        args,
    })
}

fn authorize_tool(
    name: &str,
    definition: &ToolDefinition,
    ctx: &mut ToolContext<'_>,
) -> Result<()> {
    if !definition.availability.enabled(ctx.config) {
        bail!("Tool {name} is disabled by the current configuration");
    }
    if ctx.config.permissions.approval == "always" && risk(name) == "read_only" {
        approve(ctx, &format!("Allow read-only tool {name}? [y/N] "), false)?;
    }
    Ok(())
}

async fn dispatch_tool(
    handler: ToolHandler,
    args: &Value,
    ctx: &mut ToolContext<'_>,
) -> Result<ToolResult> {
    match handler {
        ToolHandler::ListDirectory => list_directory(args, ctx),
        ToolHandler::ReadFile => read_file(args, ctx),
        ToolHandler::StatPath => stat_path(args, ctx),
        ToolHandler::CreateDirectory => create_directory(args, ctx),
        ToolHandler::WriteFile => write_file(args, ctx),
        ToolHandler::MovePath => move_path(args, ctx),
        ToolHandler::CopyPath => copy_path(args, ctx),
        ToolHandler::RemovePath => remove_path(args, ctx),
        ToolHandler::ApplyPatch => apply_patch(args, ctx),
        ToolHandler::Shell => shell(args, ctx).await,
        ToolHandler::SearchMemory => search_memory(args, ctx).await,
        ToolHandler::SaveMemory => save_memory(args, ctx).await,
        ToolHandler::WebSearch => web_search(args, ctx).await,
    }
}

fn normalize_result(result: &mut Result<ToolResult>, max_output_bytes: usize) {
    if let Ok(value) = result {
        value.content = truncate(std::mem::take(&mut value.content), max_output_bytes);
    }
}

fn finalize_result(
    call_id: &str,
    name: &str,
    args: &Value,
    cwd: &Path,
    result: &mut Result<ToolResult>,
    duration_ms: u64,
) {
    if let Ok(value) = result {
        let mut presentation = tool_presentation(call_id, name, args, cwd);
        presentation["status"] = Value::String("completed".into());
        presentation["duration_ms"] = Value::Number(duration_ms.into());
        if let Some(exit_code) = value.exit_code {
            presentation["exit_code"] = Value::Number(exit_code.into());
        }
        if let Some(summary) = &value.completion_summary {
            presentation["summary"] = Value::String(one_line(summary));
        }
        value.presentation = Some(presentation);
    }
}

fn observe_tool(
    call_id: &str,
    name: &str,
    args: &Value,
    audit_args: &str,
    result: &Result<ToolResult>,
    started: Instant,
    ctx: &mut ToolContext<'_>,
) -> Result<()> {
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(value) => {
            // shell emits command_finished itself; dry runs still need the
            // generic completion event because no process was started.
            if name != "shell" || ctx.dry_run {
                let summary = value
                    .completion_summary
                    .clone()
                    .unwrap_or_else(|| one_line(&value.content));
                ctx.events.tool_finished_with_data(
                    name,
                    &summary,
                    elapsed,
                    Some(value.presentation.clone().unwrap_or_else(|| {
                        json!({
                            "tool_call_id": call_id,
                            "status": "completed",
                            "exit_code": value.exit_code,
                        })
                    })),
                )?;
            }
        }
        Err(error) => ctx.events.tool_failed_with_data(
            name,
            &error.to_string(),
            elapsed,
            Some(json!({
                "tool_call_id": call_id,
                "status": "failed",
            })),
        )?,
    }
    let owned_error = result.as_ref().err().map(ToString::to_string);
    let (status, audit_text, exit) = match result {
        Ok(value) => ("completed", value.content.as_str(), value.exit_code),
        Err(_) => (
            "failed",
            owned_error.as_deref().unwrap_or("unknown error"),
            None,
        ),
    };
    ctx.store.audit_tool(
        ctx.session_id,
        call_id,
        name,
        audit_args,
        &truncate(redact(audit_text), 8_192),
        status,
        risk_for(name, args),
        exit,
        elapsed as u64,
    )?;
    Ok(())
}

pub fn audit_interrupted(
    call_id: &str,
    name: &str,
    arguments: &str,
    ctx: &mut ToolContext<'_>,
    error: &str,
    duration_ms: u64,
) -> Result<()> {
    let args = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
    ctx.events.tool_failed_with_data(
        name,
        error,
        duration_ms as u128,
        Some(json!({
            "tool_call_id": call_id,
            "status": "interrupted",
        })),
    )?;
    ctx.store.audit_tool(
        ctx.session_id,
        call_id,
        name,
        &truncate(safe_args(name, &args), 8_192),
        &truncate(redact(error), 8_192),
        "failed",
        risk_for(name, &args),
        None,
        duration_ms,
    )
}

fn validate_argument_keys(name: &str, args: &Value) -> Result<()> {
    let definition = find_tool_definition(name).with_context(|| format!("Unknown tool: {name}"))?;
    let object = args
        .as_object()
        .context("Tool arguments must be an object")?;
    if let Some(key) = object
        .keys()
        .find(|key| !definition.allowed_keys.contains(&key.as_str()))
    {
        bail!("Tool {name} received an unsupported argument: {key}");
    }
    for key in ["max_bytes", "timeout_seconds", "limit"] {
        if let Some(value) = object.get(key) {
            if value.as_u64().is_none() {
                bail!("Tool argument {key} must be a nonnegative integer");
            }
        }
    }
    for key in ["overwrite", "recursive", "elevated"] {
        if let Some(value) = object.get(key) {
            if !value.is_boolean() {
                bail!("Tool argument {key} must be a boolean");
            }
        }
    }
    Ok(())
}

fn list_directory(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_existing(ctx.cwd, string(args, "path")?)?;
    approve_external_access(ctx, &path, "List directory")?;
    list_directory_at(&path)
}

fn list_directory_at(path: &Path) -> Result<ToolResult> {
    let mut entries = fs::read_dir(path)?
        .take(1000)
        .map(|entry| {
            let entry = entry?;
            let ty = entry.file_type()?;
            Ok(format!(
                "{}\t{}",
                if ty.is_dir() {
                    "dir"
                } else if ty.is_symlink() {
                    "symlink"
                } else {
                    "file"
                },
                entry.file_name().to_string_lossy()
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(text(entries.join("\n")))
}

fn read_file(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_existing(ctx.cwd, string(args, "path")?)?;
    approve_external_access(ctx, &path, "Read file")?;
    let max = args["max_bytes"]
        .as_u64()
        .unwrap_or(ctx.config.permissions.max_output_bytes as u64)
        .min(ctx.config.permissions.max_output_bytes as u64);
    read_file_at(&path, max)
}

fn read_file_at(path: &Path, max: u64) -> Result<ToolResult> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        bail!("Not a regular file: {}", path.display())
    }
    if metadata.len() > max {
        bail!(
            "File size {} bytes exceeds the read limit of {}",
            metadata.len(),
            max
        )
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    open_read_no_follow(path)?
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        bail!("The file grew beyond the read limit while being read");
    }
    text_result(String::from_utf8(bytes).context("The file is not valid UTF-8 text")?)
}

fn stat_path(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_existing(ctx.cwd, string(args, "path")?)?;
    approve_external_access(ctx, &path, "Inspect path")?;
    stat_path_at(&path)
}

fn stat_path_at(path: &Path) -> Result<ToolResult> {
    let md = fs::symlink_metadata(path)?;
    text_result(format!(
        "path={}\ntype={}\nbytes={}\nreadonly={}",
        path.display(),
        if md.is_dir() {
            "directory"
        } else if md.is_file() {
            "file"
        } else if md.file_type().is_symlink() {
            "symlink"
        } else {
            "other"
        },
        md.len(),
        md.permissions().readonly()
    ))
}

/// Returns true only for local, read-only calls that do not need shared
/// approval or state. The caller still rechecks this condition inside the
/// worker before touching the filesystem.
pub fn is_parallel_read_only(name: &str, arguments: &str, config: &Config, cwd: &Path) -> bool {
    if config.permissions.approval == "always"
        || !matches!(name, "list_directory" | "read_file" | "stat_path")
    {
        return false;
    }
    let Ok(args) = serde_json::from_str::<Value>(arguments) else {
        return false;
    };
    if !args.is_object() || validate_argument_keys(name, &args).is_err() {
        return false;
    }
    let Ok(path) = resolve_existing(cwd, args["path"].as_str().unwrap_or_default()) else {
        return false;
    };
    !is_external_path(cwd, &path)
}

/// Executes one previously classified local read-only call without touching
/// the shared StateStore or EventSink. It is intentionally synchronous so the
/// agent can run it on Tokio's blocking pool and obtain real filesystem
/// overlap rather than merely interleaving async futures.
pub fn execute_parallel_read_only(
    call_id: &str,
    name: &str,
    arguments: &str,
    config: &Config,
    cwd: &Path,
) -> Result<ToolResult> {
    if !is_parallel_read_only(name, arguments, config, cwd) {
        bail!("Tool {name} is not eligible for parallel read-only execution");
    }
    let args: Value = serde_json::from_str(arguments)?;
    let path = resolve_existing(cwd, string(&args, "path")?)?;
    let result = match name {
        "list_directory" => list_directory_at(&path),
        "read_file" => {
            let max = args["max_bytes"]
                .as_u64()
                .unwrap_or(config.permissions.max_output_bytes as u64)
                .min(config.permissions.max_output_bytes as u64);
            read_file_at(&path, max)
        }
        "stat_path" => stat_path_at(&path),
        _ => unreachable!("parallel eligibility checked above"),
    }?;
    let mut finalized = Ok(result);
    normalize_result(&mut finalized, config.permissions.max_output_bytes);
    finalize_result(call_id, name, &args, cwd, &mut finalized, 0);
    finalized
}

pub fn presentation_for_call(
    call_id: &str,
    name: &str,
    arguments: &str,
    cwd: &Path,
) -> Result<Value> {
    let args: Value = serde_json::from_str(arguments)
        .with_context(|| format!("Arguments for tool {name} are not valid JSON"))?;
    validate_argument_keys(name, &args)?;
    Ok(tool_presentation(call_id, name, &args, cwd))
}

pub struct ParallelAudit<'a> {
    pub store: &'a mut StateStore,
    pub session_id: &'a str,
    pub call_id: &'a str,
    pub name: &'a str,
    pub arguments: &'a str,
    pub result_text: &'a str,
    pub status: &'a str,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

pub fn record_parallel_audit(audit: ParallelAudit<'_>) -> Result<()> {
    let args = serde_json::from_str(audit.arguments).unwrap_or_else(|_| json!({}));
    audit.store.audit_tool(
        audit.session_id,
        audit.call_id,
        audit.name,
        &truncate(safe_args(audit.name, &args), 8_192),
        &truncate(redact(audit.result_text), 8_192),
        audit.status,
        risk_for(audit.name, &args),
        audit.exit_code,
        audit.duration_ms,
    )
}

fn create_directory(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_target(ctx.cwd, string(args, "path")?)?;
    approve_path_mutation(
        ctx,
        &format!("Create directory {}", path.display()),
        &[&path],
        true,
    )?;
    if !ctx.dry_run {
        fs::create_dir_all(&path)?;
    }
    text_result(if ctx.dry_run {
        "Dry run: directory not created".into()
    } else {
        format!("Created {}", path.display())
    })
}

fn write_file(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_target(ctx.cwd, string(args, "path")?)?;
    reject_symlink_target(&path)?;
    let creates_new_file = !path.exists();
    let content = string(args, "content")?;
    if content.len() > ctx.config.permissions.max_output_bytes {
        bail!("File content exceeds permissions.max_output_bytes");
    }
    approve_path_mutation(
        ctx,
        &format!("Write {} ({} bytes)", path.display(), content.len()),
        &[&path],
        creates_new_file,
    )?;
    if !ctx.dry_run {
        let recorder = Recorder::new(ctx, "write_file")?;
        if let Some(recorder) = &recorder {
            if creates_new_file {
                recorder.created(ctx.store, &path)?;
            } else {
                recorder.overwrite(ctx.store, &path)?;
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, content.as_bytes())?;
        if let Some(recorder) = &recorder {
            recorder.commit(ctx.store)?;
        }
    }
    text_result(if ctx.dry_run {
        "Dry run: file not written".into()
    } else {
        format!("Wrote {} bytes", content.len())
    })
}

fn move_path(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let src = resolve_target(ctx.cwd, string(args, "source")?)?;
    let dst = resolve_target(ctx.cwd, string(args, "destination")?)?;
    if !src.exists() {
        bail!("Source does not exist: {}", src.display());
    }
    reject_symlink_target(&dst)?;
    if dst.exists() && !args["overwrite"].as_bool().unwrap_or(false) {
        bail!("Destination already exists: {}", dst.display())
    }
    approve_path_mutation(
        ctx,
        &format!("Move {} -> {}", src.display(), dst.display()),
        &[&src, &dst],
        false,
    )?;
    if !ctx.dry_run {
        if dst.exists() && (src.is_dir() || dst.is_dir()) {
            bail!("Overwriting a directory with move_path is not supported safely");
        }
        let recorder = Recorder::new(ctx, "move_path")?;
        if let Some(recorder) = &recorder {
            if dst.exists() {
                recorder.overwrite(ctx.store, &dst)?;
            }
            recorder.moved(ctx.store, &src, &dst)?;
        }
        fs::rename(&src, &dst)?;
        if let Some(recorder) = &recorder {
            recorder.commit(ctx.store)?;
        }
    }
    text_result(if ctx.dry_run {
        "Dry run: path not moved".into()
    } else {
        "Move completed".into()
    })
}

fn copy_path(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let src = resolve_existing(ctx.cwd, string(args, "source")?)?;
    let dst = resolve_target(ctx.cwd, string(args, "destination")?)?;
    reject_symlink_target(&dst)?;
    if !src.is_file() {
        bail!("copy_path currently supports regular files only")
    }
    if dst.exists() && !args["overwrite"].as_bool().unwrap_or(false) {
        bail!("Destination already exists: {}", dst.display())
    }
    let creates_new_file = !dst.exists();
    approve_path_mutation(
        ctx,
        &format!("Copy {} -> {}", src.display(), dst.display()),
        &[&src, &dst],
        creates_new_file,
    )?;
    if !ctx.dry_run {
        let recorder = Recorder::new(ctx, "copy_path")?;
        if let Some(recorder) = &recorder {
            if !creates_new_file {
                recorder.overwrite(ctx.store, &dst)?;
            }
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_copy(&src, &dst)?;
        if let Some(recorder) = &recorder {
            recorder.commit(ctx.store)?;
        }
    }
    text_result(if ctx.dry_run {
        "Dry run: file not copied".into()
    } else {
        "Copy completed".into()
    })
}

fn remove_path(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_target(ctx.cwd, string(args, "path")?)?;
    guard_delete(&path, ctx.cwd)?;
    let use_trash = ctx.config.permissions.trash_instead_of_delete;
    if !ctx.dry_run {
        approve(
            ctx,
            &format!(
                "{} {}? [y/N] ",
                if use_trash {
                    "Move to the qin trash directory"
                } else {
                    "Permanently delete"
                },
                path.display()
            ),
            !use_trash || is_external_path(ctx.cwd, &path),
        )?;
    }
    if !ctx.dry_run {
        let recorder = Recorder::new(ctx, "remove_path")?;
        if use_trash {
            let parent = path.parent().context("The path has no parent directory")?;
            let trash = parent.join(".qin-trash");
            if path == trash {
                bail!("The qin trash directory itself cannot be removed");
            }
            fs::create_dir_all(&trash)?;
            set_private_directory(&trash)?;
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("item");
            let destination = trash.join(format!("{}-{name}", uuid::Uuid::new_v4()));
            fs::rename(&path, &destination).with_context(|| {
                format!(
                    "Unable to move {} to the recoverable trash location {}",
                    path.display(),
                    destination.display()
                )
            })?;
            if let Some(recorder) = &recorder {
                recorder.deleted(ctx.store, &path, Some(&destination))?;
            }
        } else {
            if let Some(recorder) = &recorder {
                // Snapshots cover regular files only; directory deletions are
                // recorded as unrecoverable metadata.
                recorder.deleted(ctx.store, &path, None)?;
            }
            if path.is_dir() {
                if !args["recursive"].as_bool().unwrap_or(false) {
                    bail!("Deleting a directory requires recursive=true")
                }
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
        if let Some(recorder) = &recorder {
            recorder.commit(ctx.store)?;
        }
    }
    text_result(if ctx.dry_run {
        "Dry run: path not removed".into()
    } else if use_trash {
        "Moved to the qin trash directory".into()
    } else {
        "Deletion completed".into()
    })
}

fn apply_patch(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_target(ctx.cwd, string(args, "path")?)?;
    reject_symlink_target(&path)?;
    let old = string(args, "old_text")?;
    let new = string(args, "new_text")?;
    let metadata = fs::metadata(&path)?;
    if metadata.len() > ctx.config.permissions.max_output_bytes as u64 {
        bail!("The file is too large for apply_patch");
    }
    let mut content = String::with_capacity(metadata.len() as usize);
    open_read_no_follow(&path)?.read_to_string(&mut content)?;
    let count = content.matches(old).count();
    if count != 1 {
        bail!("old_text must match exactly once; found {count} matches")
    }
    approve_path_mutation(
        ctx,
        &format!("Modify file {}", path.display()),
        &[&path],
        false,
    )?;
    if !ctx.dry_run {
        let recorder = Recorder::new(ctx, "apply_patch")?;
        if let Some(recorder) = &recorder {
            recorder.overwrite(ctx.store, &path)?;
        }
        atomic_write(&path, content.replacen(old, new, 1).as_bytes())?;
        if let Some(recorder) = &recorder {
            recorder.commit(ctx.store)?;
        }
    }
    text_result(if ctx.dry_run {
        "Dry run: file not modified".into()
    } else {
        "Patch applied".into()
    })
}

async fn shell(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let command = string(args, "command")?;
    let elevated = args["elevated"].as_bool().unwrap_or(false);
    let timeout = args["timeout_seconds"]
        .as_u64()
        .unwrap_or(ctx.config.permissions.command_timeout_seconds)
        .clamp(1, 3600);
    if ctx.dry_run {
        return text_result("Dry run: command not executed".into());
    }
    let child_can_prompt = io::stdin().is_terminal();
    validate_interactive_shell_command(command, child_can_prompt)?;
    if let Some(reason) = forbidden_reason(command) {
        bail!("Refusing forbidden command: {reason}");
    }
    let invokes_elevation = invokes_elevation(command);
    let high_risk = elevated || dangerous(command) || invokes_elevation;
    let needs_approval =
        high_risk || ctx.config.permissions.approval == "always" || !is_read_only_command(command);
    if needs_approval && !*ctx.approve_all_commands {
        let message = if ctx.events.shows_command_details() {
            // tool_started already displays the full command.
            "Allow this command? [y/N/All] ".to_string()
        } else {
            format!("Allow command `{}`? [y/N/All] ", redact(command))
        };
        if approve_command(ctx, &message, high_risk)? == ApprovalDecision::All {
            *ctx.approve_all_commands = true;
            ctx.events.tool_warning(
                "All subsequent shell commands in this task are approved; forbidden commands remain blocked",
            )?;
        }
    }
    // The child always inherits stdin below. Even commands that are not
    // elevated may pause for a username, password, confirmation, or another
    // interactive answer, so qin must not rewrite that terminal line.
    // The child is always placed in its own process group below. If stdin is
    // a TTY, every command that inherits it must become the foreground group:
    // commands such as ssh and passwd can prompt for multiple inputs even
    // when they are not elevated. Otherwise the first terminal read can stop
    // the child with SIGTTIN after the user submits the first answer.
    let interactive_terminal = child_needs_foreground_terminal(child_can_prompt);
    ctx.events.command_started_with_data(
        ctx.cwd,
        elevated || invokes_elevation,
        timeout,
        interactive_terminal,
        child_can_prompt,
        Some(json!({
            "tool_call_id": ctx.tool_call_id,
            "card": "terminal",
            "kind": "execute",
        })),
    )?;
    let started = Instant::now();
    let mut terminal_mode = TerminalModeGuard::capture(interactive_terminal);
    let shell = "/bin/sh";
    let mut process = if elevated && unsafe { libc::geteuid() } != 0 {
        let mut p = Command::new(elevation_program(&ctx.config.permissions.elevation)?);
        p.arg(shell).arg("-c").arg(command);
        p
    } else {
        let mut p = Command::new(shell);
        p.arg("-c").arg(command);
        p
    };
    #[cfg(unix)]
    process.process_group(0);
    remove_secret_environment(&mut process, ctx.config);
    remove_shell_startup_environment(&mut process);
    let mut child = process
        .current_dir(ctx.cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let child_pid = child.id();
    let mut foreground_group =
        match ForegroundProcessGroupGuard::attach(interactive_terminal, child_pid) {
            Ok(guard) => guard,
            Err(error) => {
                // A command that already exited took its process group with
                // it; nothing remains that could read the terminal, so the
                // missed handoff is harmless and must not fail the command.
                if matches!(child.try_wait(), Ok(Some(_))) {
                    ForegroundProcessGroupGuard::attach(false, None)?
                } else {
                    #[cfg(unix)]
                    if let Some(pid) = child_pid.and_then(|pid| i32::try_from(pid).ok()) {
                        // SAFETY: pid is the freshly spawned process-group identifier.
                        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                    }
                    child.kill().await.ok();
                    return Err(error);
                }
            }
        };
    let mut process_group = ProcessGroupGuard::new(child_pid);
    let stdout = child.stdout.take().context("Unable to capture stdout")?;
    let stderr = child.stderr.take().context("Unable to capture stderr")?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    spawn_chunk_reader("stdout", stdout, tx.clone());
    spawn_chunk_reader("stderr", stderr, tx.clone());
    drop(tx);
    let mut output = String::new();
    let mut output_truncated = false;
    let mut streamed_bytes = 0_usize;
    let mut stream_truncated_notice = false;
    let heartbeat_period =
        std::time::Duration::from_secs(ctx.config.ui.command_heartbeat_seconds.max(1));
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_period,
        heartbeat_period,
    );
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some((label, text)) => {
                    let piece = format!("{label}: {text}");
                    if append_capped(&mut output, &piece, ctx.config.permissions.max_output_bytes) {
                        output_truncated = true;
                    }
                    if ctx.config.ui.stream_command_output {
                        let remaining = ctx.config.ui.command_output_max_bytes.saturating_sub(streamed_bytes);
                        if remaining > 0 {
                            let visible = prefix_at_boundary(&text, remaining);
                            if !visible.is_empty() {
                                ctx.events.command_output_with_data(
                                    label,
                                    visible,
                                    Some(json!({"tool_call_id": ctx.tool_call_id})),
                                )?;
                                streamed_bytes = streamed_bytes.saturating_add(visible.len());
                            }
                            if visible.len() < text.len() {
                                streamed_bytes = ctx.config.ui.command_output_max_bytes;
                            }
                        }
                        if streamed_bytes >= ctx.config.ui.command_output_max_bytes && !stream_truncated_notice {
                            ctx.events.command_output_with_data(
                                "qin",
                                "[Live command output truncated]",
                                Some(json!({"tool_call_id": ctx.tool_call_id})),
                            )?;
                            stream_truncated_notice = true;
                        }
                    }
                }
                None => break,
            },
            // Do not emit a heartbeat while the child has an interactive
            // stdin: a prompt may be waiting on the current terminal line.
            _ = heartbeat.tick(), if !interactive_terminal && !child_can_prompt => {
                ctx.events.command_heartbeat_with_data(
                    started.elapsed().as_secs(),
                    Some(json!({"tool_call_id": ctx.tool_call_id})),
                )?
            },
            _ = &mut deadline => {
                child.kill().await.ok();
                bail!("Command timed out after {timeout}s")
            },
            _ = tokio::signal::ctrl_c() => {
                child.kill().await.ok();
                bail!("Command canceled by the user")
            }
        }
    }
    let status = child.wait().await?;
    process_group.disarm();
    foreground_group
        .restore()
        .context("Unable to return terminal control to qin")?;
    terminal_mode
        .restore()
        .context("Unable to restore the terminal input mode")?;
    // While the child held the terminal foreground, Ctrl-C reached only its
    // process group, so qin's own SIGINT handlers stayed silent. Translate
    // the child's SIGINT death back into the cancellation the user asked for.
    #[cfg(unix)]
    let canceled_by_user = sigint_death_is_user_cancel(interactive_terminal, status);
    #[cfg(not(unix))]
    let canceled_by_user = false;
    if output_truncated {
        append_truncation_marker(&mut output, ctx.config.permissions.max_output_bytes);
    }
    ctx.events.command_finished_with_data(
        status.code(),
        started.elapsed().as_millis(),
        Some(json!({
            "tool_call_id": ctx.tool_call_id,
            "status": if status.success() { "completed" } else { "failed" },
            "exit_code": status.code(),
        })),
    )?;
    if canceled_by_user {
        bail!("Command canceled by the user");
    }
    let output = format!(
        "exit_code={}\n{output}",
        status
            .code()
            .map_or_else(|| "signal".into(), |code| code.to_string())
    );
    Ok(ToolResult {
        content: redact(&truncate(output, ctx.config.permissions.max_output_bytes)),
        exit_code: status.code(),
        completion_summary: None,
        presentation: None,
    })
}

fn child_needs_foreground_terminal(child_can_prompt: bool) -> bool {
    child_can_prompt
}

fn validate_interactive_shell_command(command: &str, child_can_prompt: bool) -> Result<()> {
    if child_can_prompt {
        if let Some(reason) = interactive_shell_wrapper_reason(command) {
            bail!(
                "Refusing interactive shell wrapper: {reason}; run the target command directly and set timeout_seconds on the shell tool"
            );
        }
    }
    Ok(())
}

fn interactive_shell_wrapper_reason(command: &str) -> Option<&'static str> {
    interactive_shell_wrapper_reason_inner(command, 0)
}

fn interactive_shell_wrapper_reason_inner(command: &str, depth: usize) -> Option<&'static str> {
    if depth > 3 {
        return None;
    }
    let commands = shell_commands_for_guard(command)?;
    for raw_tokens in commands {
        let tokens = unwrap_interactive_command_prefixes(&raw_tokens);
        let Some(program) = tokens.first().and_then(|value| {
            Path::new(value)
                .file_name()
                .and_then(|value| value.to_str())
        }) else {
            continue;
        };
        let reason = match program {
            "timeout" => Some("timeout can create a separate process group"),
            "setsid" => Some("setsid creates a new session and process group"),
            "nohup" => Some("nohup can redirect terminal input"),
            _ => None,
        };
        if reason.is_some() {
            return reason;
        }
        if matches!(program, "sh" | "bash" | "zsh" | "ksh" | "dash") {
            let Some(index) = tokens.iter().position(|value| value == "-c") else {
                continue;
            };
            let Some(script) = tokens.get(index + 1) else {
                continue;
            };
            if let Some(reason) = interactive_shell_wrapper_reason_inner(
                &script.replace(QUOTED_REDIRECT, ">"),
                depth + 1,
            ) {
                return Some(reason);
            }
        }
    }
    None
}

fn unwrap_interactive_command_prefixes(mut tokens: &[String]) -> &[String] {
    loop {
        let unwrapped = unwrap_command_prefixes(tokens);
        if unwrapped.len() != tokens.len() {
            tokens = unwrapped;
            continue;
        }
        if tokens
            .first()
            .is_some_and(|value| is_shell_assignment(value))
        {
            tokens = &tokens[1..];
            continue;
        }
        return tokens;
    }
}

fn is_shell_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    matches!(
        characters.next(),
        Some(first) if first == '_' || first.is_ascii_alphabetic()
    ) && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

// Terminal-generated SIGINT only reaches the foreground process group, so
// while a child holds the terminal, qin never sees Ctrl-C. A foreground
// child dying from SIGINT therefore means the user pressed Ctrl-C.
#[cfg(unix)]
fn sigint_death_is_user_cancel(
    interactive_terminal: bool,
    status: std::process::ExitStatus,
) -> bool {
    use std::os::unix::process::ExitStatusExt as _;
    interactive_terminal && status.signal() == Some(libc::SIGINT)
}

struct ProcessGroupGuard {
    pid: Option<u32>,
}

#[cfg(unix)]
struct TerminalModeGuard {
    original: Option<libc::termios>,
}

#[cfg(unix)]
impl TerminalModeGuard {
    fn capture(enabled: bool) -> Self {
        if !enabled {
            return Self { original: None };
        }
        let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: mode points to valid writable storage and STDIN_FILENO is
        // checked as a terminal before this guard is enabled.
        let original = (unsafe { libc::tcgetattr(libc::STDIN_FILENO, mode.as_mut_ptr()) } == 0)
            .then(|| {
                // SAFETY: tcgetattr returned success and initialized mode.
                unsafe { mode.assume_init() }
            });
        Self { original }
    }

    fn restore(&mut self) -> io::Result<()> {
        if let Some(mode) = &self.original {
            // SAFETY: mode was returned by tcgetattr for this terminal.
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, mode) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.original = None;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        // Restores echo and other flags even if sudo is killed on timeout.
        let _ = self.restore();
    }
}

#[cfg(not(unix))]
struct TerminalModeGuard;

#[cfg(not(unix))]
impl TerminalModeGuard {
    fn capture(_enabled: bool) -> Self {
        Self
    }

    fn restore(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
struct ForegroundProcessGroupGuard {
    original: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ForegroundProcessGroupGuard {
    fn attach(enabled: bool, pid: Option<u32>) -> Result<Self> {
        if !enabled {
            return Ok(Self { original: None });
        }
        let pid = pid
            .and_then(|pid| i32::try_from(pid).ok())
            .context("Unable to determine the interactive command process group")?;
        // SAFETY: STDIN_FILENO is a terminal here and tcgetpgrp only queries it.
        let original = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if original < 0 {
            return Err(io::Error::last_os_error())
                .context("Unable to read the terminal foreground process group");
        }
        set_terminal_foreground_group(pid)
            .context("Unable to give the interactive command control of the terminal")?;
        // The child may have received SIGTTIN during the small handoff window.
        // SAFETY: pid is the freshly spawned process-group identifier.
        let _ = unsafe { libc::kill(-pid, libc::SIGCONT) };
        Ok(Self {
            original: Some(original),
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if let Some(original) = self.original {
            set_terminal_foreground_group(original)?;
            self.original = None;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ForegroundProcessGroupGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
fn set_terminal_foreground_group(group: libc::pid_t) -> io::Result<()> {
    // tcsetpgrp may be called while qin is temporarily in the background
    // during restoration. Ignore SIGTTOU only for the duration of this call.
    // SAFETY: signal handlers are restored immediately and group is a process
    // group obtained from tcgetpgrp or the freshly spawned child pid.
    let result = unsafe {
        let previous = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let result = libc::tcsetpgrp(libc::STDIN_FILENO, group);
        libc::signal(libc::SIGTTOU, previous);
        result
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
struct ForegroundProcessGroupGuard;

#[cfg(not(unix))]
impl ForegroundProcessGroupGuard {
    fn attach(_enabled: bool, _pid: Option<u32>) -> Result<Self> {
        Ok(Self)
    }

    fn restore(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: kill receives only the freshly spawned process-group identifier.
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
}

async fn search_memory(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let hits = knowledge::search(
        ctx.store,
        ctx.config,
        string(args, "query")?,
        Some("memory"),
        args["limit"].as_u64().unwrap_or(5) as usize,
    )
    .await?;
    text_result(serde_json::to_string_pretty(&hits)?)
}
async fn save_memory(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    if !ctx.config.permissions.workspace_write || !ctx.config.knowledge_active() {
        bail!("Long-term memory writes are disabled by the current configuration");
    }
    if ctx.dry_run {
        return text_result("Dry run: memory not saved".into());
    }
    approve(ctx, "Save an item to long-term memory? [y/N] ", false)?;
    let added = knowledge::add_memory(ctx.store, ctx.config, string(args, "content")?).await?;
    text_result(if added {
        "Memory saved".into()
    } else {
        "An identical memory already exists".into()
    })
}

async fn web_search(args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let query = string(args, "query")?;
    if query.trim().is_empty() || query.chars().count() > 2_000 {
        bail!("The web-search query must contain between 1 and 2,000 characters");
    }
    let limit = args["limit"]
        .as_u64()
        .unwrap_or(ctx.config.search.max_results as u64)
        .min(10) as usize;
    let mut errors = Vec::new();
    for provider in &ctx.config.search.order {
        let result = match provider.as_str() {
            "exa" if ctx.config.search.exa.enabled => search_exa(ctx.config, query, limit).await,
            "brave" if ctx.config.search.brave.enabled => {
                search_brave(ctx.config, query, limit).await
            }
            "native" if ctx.config.search.native.enabled => search_native(ctx.config, query).await,
            _ => continue,
        };
        match result {
            Ok(value) => {
                return Ok(ToolResult {
                    content: value.content,
                    exit_code: None,
                    completion_summary: Some(web_result_summary(value.result_count)),
                    presentation: None,
                });
            }
            Err(error) => errors.push(format!("{provider}: {error}")),
        }
    }
    if errors.is_empty() {
        bail!("No enabled search backend is present in search.order");
    }
    bail!("No search backend succeeded: {}", errors.join("; "))
}

struct WebSearchOutput {
    content: String,
    result_count: Option<usize>,
}

fn web_result_summary(result_count: Option<usize>) -> String {
    match result_count {
        Some(1) => "1 result".into(),
        Some(count) => format!("{count} results"),
        None => "completed".into(),
    }
}

async fn search_exa(config: &Config, query: &str, limit: usize) -> Result<WebSearchOutput> {
    let key = config.search.exa.secret("Exa")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.search.timeout_seconds,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .post("https://api.exa.ai/search")
        .header("x-api-key", key)
        .json(&json!({"query":query,"numResults":limit,"contents":{"text":{"maxCharacters":1000}}}))
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("HTTP {}", response.status())
    }
    let value = read_json_limited(
        response,
        config.permissions.max_output_bytes.saturating_mul(4),
    )
    .await?;
    let results = value["results"]
        .as_array()
        .context("Exa response did not contain a results array")?;
    Ok(WebSearchOutput {
        content: serde_json::to_string_pretty(results)?,
        result_count: Some(results.len()),
    })
}
async fn search_brave(config: &Config, query: &str, limit: usize) -> Result<WebSearchOutput> {
    let key = config.search.brave.secret("Brave")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.search.timeout_seconds,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", key)
        .query(&[("q", query), ("count", &limit.to_string())])
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("HTTP {}", response.status())
    }
    let value = read_json_limited(
        response,
        config.permissions.max_output_bytes.saturating_mul(4),
    )
    .await?;
    let results = value["web"]["results"]
        .as_array()
        .context("Brave response did not contain a web results array")?;
    Ok(WebSearchOutput {
        content: serde_json::to_string_pretty(results)?,
        result_count: Some(results.len()),
    })
}

async fn search_native(config: &Config, query: &str) -> Result<WebSearchOutput> {
    let model = config
        .search
        .native
        .model
        .as_deref()
        .and_then(|name| config.models.get(name))
        .unwrap_or(config.primary_model()?);
    if !model.supports_native_search {
        bail!("The selected native-search model does not declare supports_native_search=true");
    }
    let base = model
        .base_url
        .trim_end_matches('/')
        .strip_suffix("/chat/completions")
        .unwrap_or(model.base_url.trim_end_matches('/'));
    let endpoint = if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{base}/responses")
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.search.timeout_seconds,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .post(endpoint)
        .bearer_auth(model.resolve_api_key()?)
        .json(&json!({
            "model": model.model,
            "input": query,
            "tools": [{"type":"web_search_preview"}]
        }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        bail!("Model-native search returned HTTP {status}");
    }
    let value = read_json_limited(
        response,
        config.permissions.max_output_bytes.saturating_mul(4),
    )
    .await?;
    if let Some(text) = value["output_text"].as_str() {
        return Ok(WebSearchOutput {
            content: text.to_string(),
            result_count: None,
        });
    }
    Ok(WebSearchOutput {
        content: serde_json::to_string_pretty(&value["output"])?,
        result_count: None,
    })
}

async fn read_json_limited(response: reqwest::Response, max_bytes: usize) -> Result<Value> {
    let max_bytes = max_bytes.min(16 * 1024 * 1024);
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("Search response exceeded the configured size limit");
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("Search response exceeded the configured size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&body)?)
}

fn approve_path_mutation(
    ctx: &mut ToolContext<'_>,
    message: &str,
    paths: &[&Path],
    reversible_without_overwrite: bool,
) -> Result<()> {
    if !ctx.config.permissions.workspace_write {
        bail!("The configuration does not allow workspace writes")
    }
    if ctx.dry_run {
        return Ok(());
    }
    let external = paths.iter().any(|path| is_external_path(ctx.cwd, path));
    if auto_approves_path_mutation(ctx.config, ctx.cwd, paths, reversible_without_overwrite) {
        return Ok(());
    }
    let prompt = path_mutation_prompt(message);
    approve(ctx, &prompt, external)
}

fn path_mutation_prompt(message: &str) -> String {
    format!("{message}? [y/N] ")
}

fn auto_approves_path_mutation(
    config: &Config,
    cwd: &Path,
    paths: &[&Path],
    reversible_without_overwrite: bool,
) -> bool {
    config.permissions.approval == "on_risk"
        && reversible_without_overwrite
        && paths.iter().all(|path| !is_external_path(cwd, path))
}

fn approve_external_access(ctx: &mut ToolContext<'_>, path: &Path, action: &str) -> Result<()> {
    if is_external_path(ctx.cwd, path) {
        approve(
            ctx,
            &format!(
                "{action} outside the current workspace: {}? [y/N] ",
                path.display()
            ),
            true,
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalAnswer {
    Once,
    All,
    Rejected,
    Cancelled,
    Unavailable,
}

fn approve(ctx: &mut ToolContext<'_>, message: &str, high_risk: bool) -> Result<()> {
    if ctx.assume_yes && !high_risk {
        return Ok(());
    }
    if ctx.config.permissions.approval == "never" && !high_risk {
        return Ok(());
    }
    match request_approval(ctx, message, high_risk, false)? {
        ApprovalAnswer::Once => Ok(()),
        answer => bail!(approval_denial_message(answer)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalDecision {
    Once,
    All,
}

fn approve_command(
    ctx: &mut ToolContext<'_>,
    message: &str,
    high_risk: bool,
) -> Result<ApprovalDecision> {
    if ctx.assume_yes && !high_risk {
        return Ok(ApprovalDecision::Once);
    }
    if ctx.config.permissions.approval == "never" && !high_risk {
        return Ok(ApprovalDecision::Once);
    }
    match request_approval(ctx, message, high_risk, true)? {
        ApprovalAnswer::Once => Ok(ApprovalDecision::Once),
        ApprovalAnswer::All => Ok(ApprovalDecision::All),
        answer => bail!(approval_denial_message(answer)),
    }
}

fn request_approval(
    ctx: &mut ToolContext<'_>,
    message: &str,
    high_risk: bool,
    allow_all: bool,
) -> Result<ApprovalAnswer> {
    let approval_id = Uuid::new_v4().to_string();
    ctx.store.append_approval_asked(&ApprovalRequest {
        session_id: ctx.session_id,
        turn_id: ctx.turn_id,
        tool_call_id: ctx.tool_call_id,
        approval_id: &approval_id,
        tool_name: ctx.tool_name,
        reason: message,
        high_risk,
        allow_all,
    })?;

    let prompt_data = json!({
        "approval_id": approval_id.clone(),
        "tool_call_id": ctx.tool_call_id,
        "tool_name": ctx.tool_name,
        "high_risk": high_risk,
        "allow_all": allow_all,
    });
    if let Err(error) = ctx
        .events
        .approval_prompt_with_data(message, Some(prompt_data))
    {
        let _ = ctx.store.append_approval_decided(
            ctx.session_id,
            ctx.turn_id,
            ctx.tool_call_id,
            &approval_id,
            ApprovalOutcome::Unavailable,
        );
        let _ = ctx.events.approval_decided(
            &approval_id,
            ctx.tool_call_id,
            ApprovalOutcome::Unavailable.as_str(),
        );
        return Err(error).context("Unable to render the approval prompt");
    }
    let answer = if !io::stdin().is_terminal() {
        ApprovalAnswer::Unavailable
    } else {
        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(_) => parse_approval_answer(&input, allow_all),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => ApprovalAnswer::Cancelled,
            Err(_) => ApprovalAnswer::Unavailable,
        }
    };
    let outcome = approval_outcome(answer);
    ctx.store.append_approval_decided(
        ctx.session_id,
        ctx.turn_id,
        ctx.tool_call_id,
        &approval_id,
        outcome,
    )?;
    let _ = ctx
        .events
        .approval_decided(&approval_id, ctx.tool_call_id, outcome.as_str());
    Ok(answer)
}

fn approval_outcome(answer: ApprovalAnswer) -> ApprovalOutcome {
    match answer {
        ApprovalAnswer::Once => ApprovalOutcome::AllowedOnce,
        ApprovalAnswer::All => ApprovalOutcome::AllowedForTask,
        ApprovalAnswer::Rejected => ApprovalOutcome::Rejected,
        ApprovalAnswer::Cancelled => ApprovalOutcome::Cancelled,
        ApprovalAnswer::Unavailable => ApprovalOutcome::Unavailable,
    }
}

fn approval_denial_message(answer: ApprovalAnswer) -> &'static str {
    match answer {
        ApprovalAnswer::Rejected => "Execution was declined by the user",
        ApprovalAnswer::Cancelled => "Approval was canceled; execution was not performed",
        ApprovalAnswer::Unavailable => {
            "Approval was unavailable; execution was not performed (use --yes only for permitted non-high-risk actions)"
        }
        ApprovalAnswer::Once | ApprovalAnswer::All => "Approval was not granted",
    }
}

fn parse_approval_answer(answer: &str, allow_all: bool) -> ApprovalAnswer {
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" | "\u{662f}" => ApprovalAnswer::Once,
        "a" | "all" | "\u{5168}\u{90e8}" if allow_all => ApprovalAnswer::All,
        "" | "n" | "no" | "\u{4e0d}" | "\u{5426}" => ApprovalAnswer::Rejected,
        _ => ApprovalAnswer::Rejected,
    }
}

fn dangerous(command: &str) -> bool {
    let lower = command.to_lowercase();
    let compact: String = lower
        .chars()
        .filter(|value| !value.is_whitespace())
        .collect();
    [
        "rm ",
        "rm\t",
        "unlink ",
        "rmdir ",
        "shred ",
        "mkfs",
        "dd if=",
        "wipefs",
        "fdisk",
        "parted",
        "shutdown",
        "reboot",
        "poweroff",
        "halt",
        "chmod -r",
        "chown -r",
        "-delete",
        "> /etc/",
        ">/etc/",
        "/etc/shadow",
        "/etc/sudoers",
        ".ssh/",
        ".aws/credentials",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || ((compact.contains("curl") || compact.contains("wget"))
            && (compact.contains("|sh") || compact.contains("|bash")))
        || firewall_mutation(command)
}

fn forbidden_reason(command: &str) -> Option<&'static str> {
    forbidden_reason_inner(command, 0)
}

fn forbidden_reason_inner(command: &str, depth: usize) -> Option<&'static str> {
    if depth > 3 {
        return Some("nested shell execution exceeded the safety parser limit");
    }
    let compact: String = command
        .chars()
        .filter(|value| !value.is_whitespace())
        .collect();
    if compact.contains(":(){:|:&};:") {
        return Some("fork bomb");
    }
    let commands = shell_commands_for_guard(command)?;
    for tokens in commands {
        let tokens = unwrap_command_prefixes(&tokens);
        let Some(program) = tokens.first().and_then(|value| {
            Path::new(value)
                .file_name()
                .and_then(|value| value.to_str())
        }) else {
            continue;
        };
        if let Some(reason) = nested_shell_forbidden_reason(program, tokens, depth) {
            return Some(reason);
        }
        if program == "rm" && recursive_rm_targets_forbidden(tokens) {
            return Some("recursive deletion of a broad system or home directory");
        }
        if (program == "mkfs" || program.starts_with("mkfs."))
            && tokens.iter().any(|value| is_raw_block_device(value))
        {
            return Some("formatting a raw block device");
        }
        if program == "dd"
            && tokens
                .iter()
                .filter_map(|value| value.strip_prefix("of="))
                .any(is_raw_block_device)
        {
            return Some("overwriting a raw block device");
        }
        if program == "wipefs"
            && tokens
                .iter()
                .any(|value| matches!(value.as_str(), "-a" | "--all"))
            && tokens.iter().any(|value| is_raw_block_device(value))
        {
            return Some("erasing signatures from a raw block device");
        }
        if tokens.windows(2).any(|pair| {
            matches!(pair[0].as_str(), ">" | ">>" | ">|" | "1>" | "1>>")
                && is_raw_block_device(&pair[1])
        }) {
            return Some("redirecting output to a raw block device");
        }
        if program == "kill" && tokens.iter().skip(1).any(|value| value == "-1") {
            return Some("killing all accessible processes");
        }
    }
    None
}

fn nested_shell_forbidden_reason(
    program: &str,
    tokens: &[String],
    depth: usize,
) -> Option<&'static str> {
    if !matches!(program, "sh" | "bash" | "zsh" | "ksh" | "dash") {
        return None;
    }
    let index = tokens.iter().position(|value| value == "-c")?;
    let script = tokens.get(index + 1)?;
    forbidden_reason_inner(&script.replace(QUOTED_REDIRECT, ">"), depth + 1)
}

const QUOTED_REDIRECT: char = '\u{e000}';

fn shell_commands_for_guard(command: &str) -> Option<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        if escaped {
            token.push(if quote.is_some() && character == '>' {
                QUOTED_REDIRECT
            } else {
                character
            });
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else if character == '>' {
                    token.push(QUOTED_REDIRECT);
                } else {
                    token.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '>' {
                    token.push(QUOTED_REDIRECT);
                } else {
                    token.push(character);
                }
            }
            _ if character == '\\' => escaped = true,
            _ if matches!(character, '\'' | '"') => quote = Some(character),
            _ if character.is_whitespace() => push_guard_token(&mut tokens, &mut token),
            _ if character == '>' => {
                push_guard_token(&mut tokens, &mut token);
                let mut operator = String::from(">");
                if characters
                    .peek()
                    .is_some_and(|next| matches!(next, '>' | '|'))
                {
                    operator.push(characters.next().expect("peeked redirection operator"));
                }
                tokens.push(operator);
            }
            _ if matches!(character, ';' | '|' | '&') => {
                push_guard_token(&mut tokens, &mut token);
                if !tokens.is_empty() {
                    commands.push(std::mem::take(&mut tokens));
                }
            }
            _ => token.push(character),
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    push_guard_token(&mut tokens, &mut token);
    if !tokens.is_empty() {
        commands.push(tokens);
    }
    Some(commands)
}

fn push_guard_token(tokens: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        tokens.push(std::mem::take(token));
    }
}

fn unwrap_command_prefixes(mut tokens: &[String]) -> &[String] {
    loop {
        let Some(program) = tokens.first().map(String::as_str) else {
            return tokens;
        };
        if matches!(program, "command" | "exec") {
            tokens = &tokens[1..];
            continue;
        }
        if !matches!(program, "sudo" | "doas" | "env") {
            return tokens;
        }
        let mut index = 1;
        while index < tokens.len() {
            let value = tokens[index].as_str();
            let takes_value = matches!(
                value,
                "-u" | "-g"
                    | "-h"
                    | "-p"
                    | "-C"
                    | "-T"
                    | "-R"
                    | "-D"
                    | "--user"
                    | "--group"
                    | "--host"
                    | "--prompt"
                    | "--chdir"
                    | "--unset"
            );
            if takes_value {
                index = index.saturating_add(2);
            } else if value.starts_with('-') || (program == "env" && value.contains('=')) {
                index += 1;
            } else {
                break;
            }
        }
        tokens = &tokens[index.min(tokens.len())..];
    }
}

fn recursive_rm_targets_forbidden(tokens: &[String]) -> bool {
    let mut options = true;
    let mut recursive = false;
    for value in tokens.iter().skip(1) {
        if options && value == "--" {
            options = false;
        } else if options
            && (value == "--recursive"
                || (value.starts_with('-')
                    && !value.starts_with("--")
                    && value.chars().skip(1).any(|flag| matches!(flag, 'r' | 'R'))))
        {
            recursive = true;
        }
    }
    recursive
        && tokens
            .iter()
            .skip(1)
            .filter(|value| !value.starts_with('-'))
            .any(|value| broad_delete_target(value))
}

fn broad_delete_target(value: &str) -> bool {
    let without_glob = value.trim_end_matches('*');
    let without_glob = if without_glob == "/" {
        without_glob
    } else {
        without_glob.trim_end_matches('/')
    };
    if matches!(without_glob, "~" | "$HOME" | "${HOME}") {
        return true;
    }
    let normalized = lexical_absolute_path(without_glob);
    let home = std::env::var_os("HOME").and_then(|value| PathBuf::from(value).canonicalize().ok());
    normalized.as_deref() == Some(Path::new("/"))
        || home
            .as_deref()
            .is_some_and(|home| normalized.as_deref() == Some(home))
        || normalized.as_deref().is_some_and(|path| {
            matches!(
                path.to_str(),
                Some(
                    "/bin"
                        | "/boot"
                        | "/dev"
                        | "/etc"
                        | "/home"
                        | "/lib"
                        | "/lib64"
                        | "/opt"
                        | "/root"
                        | "/sbin"
                        | "/usr"
                        | "/var"
                )
            )
        })
}

fn lexical_absolute_path(value: &str) -> Option<PathBuf> {
    if !value.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut path = PathBuf::from("/");
    for part in parts {
        path.push(part);
    }
    Some(path)
}

fn is_raw_block_device(value: &str) -> bool {
    let value = value.trim_matches(|character| matches!(character, '\'' | '"'));
    [
        "/dev/sd",
        "/dev/hd",
        "/dev/vd",
        "/dev/xvd",
        "/dev/nvme",
        "/dev/mmcblk",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix))
}

fn invokes_elevation(command: &str) -> bool {
    command.split_whitespace().any(|token| {
        matches!(
            token.trim_matches(|value: char| !value.is_ascii_alphanumeric()),
            "sudo" | "doas" | "su"
        )
    })
}

/// Conservative allowlist for shell commands that only inspect state. Shell
/// commands outside this list still work, but `on_risk` asks before running
/// them because a shell string cannot be proven safe in the general case.
fn is_read_only_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|value| matches!(value, '`' | '\n' | '\r'))
        || trimmed.contains("$((")
        || trimmed.contains("$(")
        || trimmed.contains('#')
    {
        return false;
    }
    let segments = trimmed.split([';', '|', '&']);
    for segment in segments {
        // Discarding output streams cannot mutate anything; any other
        // redirection target (a file) keeps the command approval-gated.
        let tokens: Vec<_> = segment
            .split_whitespace()
            .filter(|token| {
                !matches!(
                    *token,
                    "2>/dev/null" | "2>&1" | "1>/dev/null" | ">/dev/null"
                )
            })
            .collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens
            .iter()
            .any(|token| token.contains('>') || token.contains('<'))
        {
            return false;
        }
        let Some(program) = trusted_program_name(tokens[0]) else {
            return false;
        };
        let args = &tokens[1..];
        // A bare version/help query against a trusted program cannot mutate.
        if args.len() == 1 && matches!(args[0], "--version" | "-V" | "version" | "--help" | "-h") {
            continue;
        }
        let allowed = matches!(
            program.as_str(),
            "date"
                | "pwd"
                | "whoami"
                | "id"
                | "groups"
                | "tty"
                | "uname"
                | "arch"
                | "nproc"
                | "hostname"
                | "hostnamectl"
                | "timedatectl"
                | "localectl"
                | "uptime"
                | "ls"
                | "dir"
                | "find"
                | "locate"
                | "which"
                | "type"
                | "file"
                | "stat"
                | "readlink"
                | "realpath"
                | "cat"
                | "head"
                | "tail"
                | "wc"
                | "grep"
                | "rg"
                | "cut"
                | "sort"
                | "uniq"
                | "tr"
                | "printf"
                | "echo"
                | "test"
                | "true"
                | "false"
                | "df"
                | "du"
                | "free"
                | "ps"
                | "printenv"
                | "lsblk"
                | "blkid"
                | "ss"
                | "netstat"
                | "lsof"
                | "lscpu"
                | "lsmem"
                | "lsusb"
                | "lspci"
                | "getent"
                | "sysctl"
                | "command"
                | "pip"
                | "pip3"
                | "pipx"
                | "mount"
                | "swapon"
                | "systemctl"
                | "journalctl"
                | "dmesg"
                | "ip"
                | "iptables"
                | "ip6tables"
                | "nft"
                | "dpkg-query"
                | "dpkg"
                | "apt-cache"
                | "apt"
                | "rpm"
                | "pacman"
                | "apk"
                | "md5sum"
                | "sha1sum"
                | "sha224sum"
                | "sha256sum"
                | "sha384sum"
                | "sha512sum"
                | "b2sum"
                | "cmp"
                | "diff"
                | "diff3"
                | "od"
                | "strings"
        );
        if !allowed || has_mutating_read_command_options(&program, args) {
            return false;
        }
    }
    true
}

fn trusted_program_name(token: &str) -> Option<String> {
    let path = Path::new(token);
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if token.contains('/') {
        return (path.is_absolute() && is_trusted_executable_path(path)).then_some(name);
    }
    if matches!(
        name.as_str(),
        "pwd" | "type" | "printf" | "echo" | "test" | "true" | "false" | "command"
    ) {
        return Some(name);
    }
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(token);
        if candidate.is_file() {
            return is_trusted_executable_path(&candidate).then_some(name);
        }
    }
    None
}

fn is_trusted_executable_path(path: &Path) -> bool {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    trusted_executable_directory(path.parent()) && trusted_executable_directory(canonical.parent())
}

fn trusted_executable_directory(directory: Option<&Path>) -> bool {
    matches!(
        directory.and_then(Path::to_str),
        Some(
            "/bin"
                | "/usr/bin"
                | "/sbin"
                | "/usr/sbin"
                | "/System/Cryptexes/App/usr/bin"
                | "/System/Cryptexes/App/usr/sbin"
        )
    )
}

fn has_mutating_read_command_options(program: &str, args: &[&str]) -> bool {
    match program {
        "command" => {
            !(args.len() >= 2
                && args[0] == "-v"
                && args[1..].iter().all(|value| !value.starts_with('-')))
        }
        "pip" | "pip3" | "pipx" => {
            !read_only_subcommand(args, &["list", "show", "check", "freeze"], false)
        }
        "date" => args.iter().any(|value| {
            matches!(*value, "-s" | "--set")
                || (value.starts_with("-s") && value.len() > 2)
                || value.starts_with("--set=")
                || (!value.starts_with('+')
                    && value.starts_with(|ch: char| ch.is_ascii_digit())
                    && value.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
        }),
        "hostname" => args.iter().any(|value| {
            !value.starts_with('-')
                || matches!(*value, "-b" | "--boot" | "-F" | "--file")
                || short_option_cluster_contains(value, 'b')
                || short_option_cluster_contains(value, 'F')
                || value.starts_with("--file=")
        }),
        "file" => args.iter().any(|value| {
            matches!(*value, "-C" | "--compile") || short_option_cluster_contains(value, 'C')
        }),
        "find" => args.iter().any(|value| {
            matches!(*value, "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir")
                || value.starts_with("-fprint")
                || *value == "-fls"
        }),
        "rg" => args.iter().any(|value| {
            matches!(*value, "--pre" | "--pre-glob")
                || value.starts_with("--pre=")
                || value.starts_with("--pre-glob=")
        }),
        "sort" => args.iter().any(|value| {
            matches!(*value, "-o" | "--output" | "--compress-program")
                || (value.starts_with("-o") && value.len() > 2)
                || value.starts_with("--output=")
                || value.starts_with("--compress-program=")
        }),
        "uniq" => uniq_has_output_path(args),
        "blkid" => args.iter().any(|value| {
            matches!(*value, "-w" | "--cache-file" | "-g" | "--garbage-collect")
                || (value.starts_with("-w") && value.len() > 2)
                || value.starts_with("--cache-file=")
        }),
        "ss" => args.iter().any(|value| {
            matches!(*value, "-K" | "--kill") || short_option_cluster_contains(value, 'K')
        }),
        "hostnamectl" => !read_only_subcommand(args, &["status"], true),
        "timedatectl" => !read_only_subcommand(
            args,
            &["status", "show", "timesync-status", "show-timesync"],
            true,
        ),
        "localectl" => !read_only_subcommand(
            args,
            &[
                "status",
                "list-locales",
                "list-keymaps",
                "list-x11-keymap-models",
                "list-x11-keymap-layouts",
                "list-x11-keymap-variants",
                "list-x11-keymap-options",
            ],
            true,
        ),
        "systemctl" => !read_only_subcommand(
            args,
            &[
                "status",
                "show",
                "cat",
                "help",
                "list-units",
                "list-unit-files",
                "list-dependencies",
                "is-active",
                "is-failed",
                "is-enabled",
                "get-default",
                "get-property",
            ],
            true,
        ),
        "journalctl" => journalctl_mutates(args),
        "dmesg" => args.iter().any(|value| {
            matches!(*value, "-c" | "-C" | "--clear" | "--read-clear")
                || short_option_cluster_contains(value, 'c')
                || short_option_cluster_contains(value, 'C')
        }),
        "ip" => !ip_query(args),
        "iptables" | "ip6tables" => !iptables_query(args),
        "nft" => !nft_query(args),
        "sysctl" => args.iter().any(|value| {
            matches!(*value, "-w" | "--write" | "-p" | "--load" | "--system")
                || value.starts_with("--load=")
                || value.contains('=')
        }),
        "mount" => args
            .iter()
            .any(|value| !matches!(*value, "-l" | "--show-labels" | "-v" | "--verbose")),
        "swapon" => args.iter().any(|value| {
            !matches!(
                *value,
                "-s" | "--summary" | "--show" | "--noheadings" | "--raw" | "--bytes"
            )
        }),
        "dpkg" => {
            let queries = args.iter().any(|value| {
                matches!(
                    *value,
                    "-l" | "--list"
                        | "-s"
                        | "--status"
                        | "-S"
                        | "--search"
                        | "--print-architecture"
                        | "--print-foreign-architectures"
                        | "--get-selections"
                )
            });
            let mutations = args.iter().any(|value| {
                matches!(
                    *value,
                    "-i" | "--install"
                        | "--unpack"
                        | "--configure"
                        | "-r"
                        | "--remove"
                        | "-P"
                        | "--purge"
                        | "--update-avail"
                        | "--merge-avail"
                        | "--clear-avail"
                        | "--set-selections"
                )
            });
            !queries || mutations
        }
        "apt-cache" => {
            apt_custom_configuration(args)
                || args
                    .iter()
                    .any(|value| matches!(*value, "-g" | "--generate"))
                || !read_only_subcommand(
                    args,
                    &[
                        "show", "search", "policy", "depends", "rdepends", "showpkg", "stats",
                        "dump", "dotty", "xvcg", "madison",
                    ],
                    false,
                )
        }
        "apt" => {
            apt_custom_configuration(args)
                || !read_only_subcommand(args, &["list", "show", "search"], false)
        }
        "rpm" => {
            !args
                .iter()
                .any(|value| *value == "--query" || value.starts_with("-q"))
                || args.iter().any(|value| {
                    matches!(
                        *value,
                        "--install" | "--upgrade" | "--erase" | "--rebuilddb" | "--setperms"
                    )
                })
        }
        "pacman" => {
            !args
                .iter()
                .any(|value| *value == "--query" || value.starts_with("-Q"))
                || args.iter().any(|value| {
                    value.starts_with("-S") || value.starts_with("-R") || value.starts_with("-U")
                })
        }
        "apk" => !read_only_subcommand(
            args,
            &["info", "search", "list", "policy", "stats", "version"],
            false,
        ),
        _ => false,
    }
}

fn read_only_subcommand(args: &[&str], allowed: &[&str], allow_no_subcommand: bool) -> bool {
    first_subcommand(args).is_some_and(|value| allowed.contains(&value))
        || (allow_no_subcommand && first_subcommand(args).is_none())
}

fn first_subcommand<'a>(args: &'a [&str]) -> Option<&'a str> {
    let mut skip_value = false;
    for value in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if matches!(
            *value,
            "-t" | "--type"
                | "--state"
                | "-p"
                | "--property"
                | "--job-mode"
                | "--kill-whom"
                | "--signal"
                | "--root"
                | "--image"
                | "--lines"
                | "--output"
                | "-o"
                | "--option"
                | "-c"
                | "--config-file"
                | "--target-release"
                | "--host-architecture"
                | "--host"
                | "-H"
                | "--machine"
                | "-M"
        ) {
            skip_value = true;
            continue;
        }
        if value.starts_with('-') {
            continue;
        }
        return Some(value);
    }
    None
}

fn journalctl_mutates(args: &[&str]) -> bool {
    args.iter().any(|value| {
        matches!(
            *value,
            "--rotate"
                | "--sync"
                | "--flush"
                | "--relinquish-var"
                | "--setup-keys"
                | "--update-catalog"
        ) || value.starts_with("--vacuum-")
            || value.starts_with("--rotate=")
            || value.starts_with("--sync=")
            || value.starts_with("--flush=")
            || value.starts_with("--relinquish-var=")
            || value.starts_with("--setup-keys=")
            || value.starts_with("--update-catalog=")
    })
}

fn ip_query(args: &[&str]) -> bool {
    const OBJECTS: &[&str] = &[
        "address",
        "addr",
        "a",
        "link",
        "l",
        "route",
        "r",
        "rule",
        "neighbour",
        "neighbor",
        "neigh",
    ];
    const MUTATIONS: &[&str] = &[
        "add", "delete", "del", "change", "replace", "set", "flush", "append", "prepend",
    ];
    !args.iter().any(|value| {
        matches!(*value, "-b" | "-batch" | "--batch" | "-force" | "--force")
            || value.starts_with("--batch=")
    }) && args.iter().any(|value| OBJECTS.contains(value))
        && !args.iter().any(|value| MUTATIONS.contains(value))
}

fn iptables_query(args: &[&str]) -> bool {
    args.iter()
        .any(|value| matches!(*value, "-L" | "--list" | "-S" | "--list-rules"))
        && !args
            .iter()
            .any(|value| matches!(*value, "-M" | "--modprobe") || value.starts_with("--modprobe="))
        && !firewall_mutation(&format!("iptables {}", args.join(" ")))
}

fn nft_query(args: &[&str]) -> bool {
    !args.iter().any(|value| {
        matches!(*value, "-f" | "--file" | "-i" | "--interactive") || value.starts_with("--file=")
    }) && read_only_subcommand(args, &["list"], false)
        && !args.iter().any(|value| {
            matches!(
                *value,
                "add" | "create" | "insert" | "delete" | "flush" | "replace" | "rename"
            )
        })
}

fn apt_custom_configuration(args: &[&str]) -> bool {
    args.iter().any(|value| {
        matches!(*value, "-o" | "--option" | "-c" | "--config-file")
            || value.starts_with("--option=")
            || value.starts_with("--config-file=")
    })
}

fn short_option_cluster_contains(value: &str, option: char) -> bool {
    value.starts_with('-')
        && !value.starts_with("--")
        && value.chars().skip(1).any(|character| character == option)
}

fn uniq_has_output_path(args: &[&str]) -> bool {
    let mut positional = 0;
    let mut index = 0;
    let mut options = true;
    while index < args.len() {
        let value = args[index];
        if options && value == "--" {
            options = false;
        } else if options && value.starts_with('-') && value != "-" {
            if matches!(
                value,
                "-f" | "-s" | "-w" | "--skip-fields" | "--skip-chars" | "--check-chars"
            ) {
                index += 1;
                if index >= args.len() {
                    return true;
                }
            }
        } else {
            positional += 1;
            if positional > 1 {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// Firewall commands are high-risk only when they mutate rules: read-only
/// queries such as `iptables -L` or `nft list ruleset` stay approvable via
/// --yes. Runs on the original command because iptables short flags are
/// case-sensitive (`-d` destination vs `-D` delete).
fn firewall_mutation(command: &str) -> bool {
    let lower = command.to_lowercase();
    let iptables = lower.contains("iptables") || lower.contains("ip6tables");
    let nft = lower.contains("nft ") || lower.contains("nft\t");
    if !iptables && !nft {
        return false;
    }
    // Loading a full ruleset always mutates.
    if lower.contains("iptables-restore") || lower.contains("iptables-apply") {
        return true;
    }
    if iptables {
        let mutates = command
            .split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&')
            .any(|token| {
                matches!(
                    token,
                    "-A" | "-I"
                        | "-D"
                        | "-F"
                        | "-X"
                        | "-N"
                        | "-E"
                        | "-P"
                        | "-Z"
                        | "--append"
                        | "--insert"
                        | "--delete"
                        | "--flush"
                        | "--new-chain"
                        | "--delete-chain"
                        | "--rename-chain"
                        | "--policy"
                        | "--zero"
                )
            });
        if mutates {
            return true;
        }
    }
    if nft {
        let normalized: String = lower.split_whitespace().collect::<Vec<_>>().join(" ");
        const NFT_MUTATING: &[&str] = &[
            "nft add",
            "nft create",
            "nft insert",
            "nft delete",
            "nft flush",
            "nft replace",
            "nft rename",
            "nft -f",
            "nft --file",
        ];
        if NFT_MUTATING.iter().any(|sub| normalized.contains(sub)) {
            return true;
        }
    }
    false
}

fn remove_secret_environment(process: &mut Command, config: &Config) {
    let mut names = Vec::new();
    for model in config.models.values() {
        if let Some(name) = &model.api_key_env {
            names.push(name.as_str());
        }
    }
    if let Some(name) = &config.embeddings.api_key_env {
        names.push(name.as_str());
    }
    if let Some(name) = &config.storage.redis.url_env {
        names.push(name.as_str());
    }
    for provider in [
        &config.search.exa,
        &config.search.brave,
        &config.search.native,
    ] {
        if let Some(name) = &provider.api_key_env {
            names.push(name.as_str());
        }
    }
    names.sort_unstable();
    names.dedup();
    for name in names {
        // api_key_env may hold an inline key instead of a variable name;
        // only real variable names can be scrubbed from the environment.
        if crate::config::is_env_var_name(name) {
            process.env_remove(name);
        }
    }
}

fn remove_shell_startup_environment(process: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let visible = name.to_string_lossy();
        if is_shell_injection_variable(&visible) {
            process.env_remove(name);
        }
    }
}

fn is_shell_injection_variable(name: &str) -> bool {
    matches!(
        name,
        "ENV" | "BASH_ENV" | "SHELLOPTS" | "BASHOPTS" | "IFS" | "CDPATH" | "GCONV_PATH"
    ) || name.starts_with("BASH_FUNC_")
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
}

fn elevation_program(configured: &str) -> Result<&str> {
    if configured == "disabled" {
        bail!("Privilege elevation is disabled by configuration")
    }
    if configured == "doas" {
        return Ok("doas");
    }
    if configured == "sudo" {
        return Ok("sudo");
    }
    if command_exists("doas") {
        Ok("doas")
    } else if command_exists("sudo") {
        Ok("sudo")
    } else {
        bail!(
            "No usable doas or sudo executable was found; on OpenWrt, administrative tasks are usually run directly as root"
        )
    }
}
fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(name).is_file())
    })
}
pub(crate) fn guard_delete(path: &Path, cwd: &Path) -> Result<()> {
    let canonical = path.canonicalize()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if canonical == Path::new("/")
        || home.as_deref() == Some(canonical.as_path())
        || canonical == cwd.canonicalize()?
    {
        bail!(
            "Refusing to delete a broad, dangerous directory: {}",
            canonical.display()
        )
    }
    Ok(())
}
pub(crate) fn reject_symlink_target(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "Refusing to write through a symbolic-link target: {}",
            path.display()
        );
    }
    Ok(())
}
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn resolve_existing(cwd: &Path, path: &str) -> Result<PathBuf> {
    resolve(cwd, path)
        .canonicalize()
        .with_context(|| format!("Path does not exist or is inaccessible: {path}"))
}

fn resolve_target(cwd: &Path, path: &str) -> Result<PathBuf> {
    let candidate = resolve(cwd, path);
    let file_name = candidate
        .file_name()
        .context("The target path must have a file name")?;
    let parent = candidate
        .parent()
        .context("The target path must have a parent directory")?;
    let canonical_parent = canonicalize_nearest_parent(parent)?;
    Ok(canonical_parent.join(file_name))
}

fn canonicalize_nearest_parent(path: &Path) -> Result<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();
    while !cursor.exists() {
        missing.push(
            cursor
                .file_name()
                .context("Unable to resolve target parent")?
                .to_os_string(),
        );
        cursor = cursor.parent().context("Unable to resolve target parent")?;
    }
    let mut resolved = cursor.canonicalize()?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn is_external_path(cwd: &Path, path: &Path) -> bool {
    let Ok(workspace) = cwd.canonicalize() else {
        return true;
    };
    let comparable = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    !comparable.starts_with(workspace)
}

pub(crate) fn open_read_no_follow(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .with_context(|| format!("Unable to open {} safely for reading", path.display()))
}

pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("The output path has no parent directory")?;
    let existing_permissions = fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| metadata.permissions());
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(permissions) = existing_permissions {
        temp.as_file().set_permissions(permissions)?;
    }
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)?;
    Ok(())
}

pub(crate) fn atomic_copy(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("The destination path has no parent directory")?;
    let mut source_file = open_read_no_follow(source)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = source_file.metadata()?.permissions().mode() & 0o777;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    io::copy(&mut source_file, &mut temp)?;
    temp.as_file().sync_all()?;
    temp.persist(destination).map_err(|error| error.error)?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

pub(crate) fn set_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "Refusing to use a non-directory or symbolic link as qin trash: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("Missing string argument: {key}"))
}
fn text(content: String) -> ToolResult {
    ToolResult {
        content,
        exit_code: None,
        completion_summary: None,
        presentation: None,
    }
}
fn text_result(content: String) -> Result<ToolResult> {
    Ok(text(content))
}
fn one_line(value: &str) -> String {
    value
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}
fn truncate(mut value: String, max: usize) -> String {
    if value.len() > max {
        let boundary = value
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= max)
            .last()
            .unwrap_or(0);
        value.truncate(boundary);
        append_truncation_marker(&mut value, max);
    }
    value
}
/// Short human-readable argument summary for the `→ tool` event line; the
/// full redacted JSON stays in the audit record.
fn display_args(name: &str, args: &Value) -> String {
    let key = match name {
        "shell" => "command",
        "web_search" | "search_knowledge" | "search_memory" => "query",
        "save_memory" => "content",
        _ => "path",
    };
    args[key]
        .as_str()
        .map(|value| one_line(&redact(value)))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| one_line(&truncate(safe_args(name, args), 200)))
}

/// Builds renderer-only metadata for JSON event consumers. It deliberately
/// describes the card and affected locations without copying file contents or
/// raw command arguments into the event stream.
fn tool_presentation(call_id: &str, name: &str, args: &Value, cwd: &Path) -> Value {
    let (card, kind) = match name {
        "shell" => ("terminal", "execute"),
        "apply_patch" | "write_file" => ("diff", "edit"),
        "remove_path" => ("generic", "delete"),
        "list_directory" | "read_file" | "stat_path" | "search_memory" | "web_search" => {
            ("generic", "read")
        }
        _ => ("generic", "edit"),
    };
    let mut data = json!({
        "tool_call_id": call_id,
        "card": card,
        "kind": kind,
        "risk": risk_for(name, args),
    });
    let mut locations = Vec::new();
    for key in ["path", "source", "destination"] {
        if let Some(path) = args.get(key).and_then(Value::as_str) {
            locations.push(json!({"path": presentation_path(path)}));
        }
    }
    if !locations.is_empty() {
        data["locations"] = Value::Array(locations);
    }
    if name == "apply_patch"
        && let (Some(path), Some(old_text), Some(new_text)) = (
            args.get("path").and_then(Value::as_str),
            args.get("old_text").and_then(Value::as_str),
            args.get("new_text").and_then(Value::as_str),
        )
    {
        data["diffs"] = json!([{
            "path": presentation_path(path),
            "old_bytes": old_text.len(),
            "new_bytes": new_text.len(),
            "content_redacted": true,
        }]);
    } else if name == "write_file"
        && let (Some(path), Some(content)) = (
            args.get("path").and_then(Value::as_str),
            args.get("content").and_then(Value::as_str),
        )
    {
        data["diffs"] = json!([{
            "path": presentation_path(path),
            "new_bytes": content.len(),
            "content_redacted": true,
        }]);
    }
    if name == "shell" {
        data["cwd"] = Value::String(presentation_path(&cwd.display().to_string()));
    }
    data
}

fn presentation_path(value: &str) -> String {
    truncate(redact(value), 1_024)
}

fn safe_args(name: &str, args: &Value) -> String {
    let mut view = args.clone();
    if matches!(name, "write_file" | "apply_patch" | "save_memory") {
        if let Some(object) = view.as_object_mut() {
            for key in ["content", "old_text", "new_text"] {
                if let Some(value) = object.get_mut(key) {
                    let len = value.as_str().map(str::len).unwrap_or(0);
                    *value = Value::String(format!("[CONTENT {len} bytes]"));
                }
            }
        }
    }
    redact(&view.to_string())
}
fn risk(name: &str) -> &'static str {
    match name {
        "remove_path" => "destructive",
        "shell" => "mutating",
        "write_file" | "move_path" | "copy_path" | "apply_patch" | "create_directory"
        | "save_memory" => "mutating",
        _ => "read_only",
    }
}

fn risk_for(name: &str, args: &Value) -> &'static str {
    if name != "shell" {
        return risk(name);
    }
    let command = args["command"].as_str().unwrap_or_default();
    if forbidden_reason(command).is_some() {
        "forbidden"
    } else if args["elevated"].as_bool().unwrap_or(false)
        || dangerous(command)
        || invokes_elevation(command)
    {
        "destructive"
    } else if is_read_only_command(command) {
        "read_only"
    } else {
        "mutating"
    }
}

fn spawn_chunk_reader<R>(
    label: &'static str,
    reader: R,
    tx: tokio::sync::mpsc::Sender<(&'static str, String)>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut buffer = vec![0_u8; 8_192];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => {
                    if !pending.is_empty() {
                        let text = String::from_utf8_lossy(&pending).into_owned();
                        let _ = tx.send((label, text)).await;
                    }
                    break;
                }
                Ok(count) => {
                    pending.extend_from_slice(&buffer[..count]);
                    let (text, consumed) = match std::str::from_utf8(&pending) {
                        Ok(text) => (text.to_string(), pending.len()),
                        Err(error) if error.error_len().is_none() => (
                            String::from_utf8_lossy(&pending[..error.valid_up_to()]).into_owned(),
                            error.valid_up_to(),
                        ),
                        Err(_) => (
                            String::from_utf8_lossy(&pending).into_owned(),
                            pending.len(),
                        ),
                    };
                    if consumed > 0 {
                        pending.drain(..consumed);
                    }
                    if !text.is_empty() && tx.send((label, text)).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx
                        .send((label, format!("[stream read error: {error}]")))
                        .await;
                    break;
                }
            }
        }
    });
}

fn prefix_at_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    &value[..boundary]
}

fn append_capped(output: &mut String, value: &str, max_bytes: usize) -> bool {
    let remaining = max_bytes.saturating_sub(output.len());
    let visible = prefix_at_boundary(value, remaining);
    output.push_str(visible);
    visible.len() < value.len()
}

fn append_truncation_marker(output: &mut String, max_bytes: usize) {
    const MARKER: &str = "\n[Output truncated]";
    if MARKER.len() > max_bytes {
        return;
    }
    if output.len() + MARKER.len() > max_bytes {
        let keep = max_bytes - MARKER.len();
        let boundary = output
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= keep)
            .last()
            .unwrap_or(0);
        output.truncate(boundary);
    }
    output.push_str(MARKER);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigPathResolver;

    #[test]
    fn registry_controls_both_schemas_and_runtime_availability() {
        let mut config = Config::default();
        config.permissions.allow_shell = false;
        let names = definitions(&config)
            .into_iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(!names.iter().any(|name| name == "shell"));
        assert!(find_tool_definition("shell").is_some());
        assert!(validate_argument_keys("read_file", &json!({"path": "x"})).is_ok());
        assert!(validate_argument_keys("read_file", &json!({"path": "x", "extra": 1})).is_err());

        config.permissions.allow_shell = true;
        assert!(
            definitions(&config)
                .into_iter()
                .any(|schema| schema["function"]["name"] == "shell")
        );
    }

    #[test]
    fn display_args_shows_a_friendly_summary() {
        assert_eq!(
            display_args(
                "shell",
                &json!({"command":"sudo apt-get update","timeout_seconds":300})
            ),
            "sudo apt-get update"
        );
        assert_eq!(
            display_args("read_file", &json!({"path":"/etc/hosts"})),
            "/etc/hosts"
        );
        assert!(display_args("stat_path", &json!({})).starts_with('{'));
    }

    #[test]
    fn web_search_completion_reports_result_count() {
        assert_eq!(web_result_summary(Some(0)), "0 results");
        assert_eq!(web_result_summary(Some(1)), "1 result");
        assert_eq!(web_result_summary(Some(8)), "8 results");
        assert_eq!(web_result_summary(None), "completed");
    }

    #[test]
    fn detects_dangerous_commands() {
        assert!(dangerous("rm -rf /tmp/x"));
        assert!(dangerous("curl https://example.test/x | bash"));
        assert!(dangerous("unlink important.db"));
        assert!(dangerous("find . -delete"));
        assert!(!dangerous("find . -type f -print"));
        assert!(!dangerous("cargo test"));
    }

    #[test]
    fn blocks_forbidden_commands_without_false_positives_from_quoted_data() {
        for command in [
            "rm -rf /",
            "sudo rm -rf '$HOME'",
            "env FOO=x rm /etc -rf",
            "sh -c 'rm -rf /usr'",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/nvme0n1",
            "echo x >/dev/sda",
            "echo x 2>/dev/sda",
            "echo x >|/dev/sda",
            "sh -c 'echo x >/dev/sda'",
            "wipefs -a /dev/vda",
        ] {
            assert!(forbidden_reason(command).is_some(), "{command}");
        }
        for command in [
            "echo 'rm -rf /'",
            "rm -rf ./target",
            "rm -- / -rf",
            "rm -rf /var/lib/qin",
            "mkfs.ext4 disk.img",
            "dd if=/dev/zero of=image",
            "echo '>' /dev/sda",
            "echo \"\\>\" /dev/sda",
            "echo 'x >/dev/sda'",
        ] {
            assert!(forbidden_reason(command).is_none(), "{command}");
        }
    }

    #[test]
    fn classifies_safe_shell_queries_as_read_only() {
        assert!(is_read_only_command("date '+%Y-%m-%d %H:%M:%S %Z (%A)'"));
        assert!(is_read_only_command("pwd && uname -a"));
        assert!(is_read_only_command("find . -type f -print | head -20"));
        assert!(is_read_only_command("/usr/bin/date +%s"));
        assert!(is_read_only_command(
            "/usr/bin/systemctl --type service list-units"
        ));
        assert!(is_read_only_command("/usr/bin/systemctl status sshd"));
        assert!(is_read_only_command("/usr/bin/journalctl -u sshd -n 20"));
        assert!(is_read_only_command("/usr/bin/dmesg --level err"));
        assert!(is_read_only_command("/usr/bin/ip -j address show"));
        assert!(is_read_only_command("/usr/bin/iptables -L -n"));
        assert!(is_read_only_command("/usr/bin/nft list ruleset"));
        assert!(is_read_only_command("/usr/bin/dpkg -l openssh-server"));
        assert!(is_read_only_command("/usr/bin/apt-cache policy qin"));
        assert!(is_read_only_command("/usr/sbin/sysctl kernel.hostname"));
        assert!(is_read_only_command("/usr/bin/mount"));
        assert!(is_read_only_command("date 2>/dev/null"));
        assert!(is_read_only_command(
            "which python3 python 2>/dev/null; command -v pip3 pip uv conda 2>/dev/null"
        ));
        assert!(!is_read_only_command("date > now.txt"));
        assert!(!is_read_only_command("date 2> errors.log"));
        assert!(!is_read_only_command("date 2>/dev/null > now.txt"));
        assert!(!is_read_only_command("command -v python3; touch created"));
        assert!(!is_read_only_command("command python3"));
        // Bare-name PATH resolution is environment-dependent, so the
        // pip/command option gates are asserted directly.
        assert!(!has_mutating_read_command_options(
            "command",
            &["-v", "python3"]
        ));
        assert!(has_mutating_read_command_options("command", &["python3"]));
        assert!(!has_mutating_read_command_options("pip3", &["list"]));
        assert!(!has_mutating_read_command_options(
            "pip3",
            &["show", "requests"]
        ));
        assert!(has_mutating_read_command_options(
            "pip3",
            &["install", "requests"]
        ));
        assert!(!is_read_only_command("sudo date"));
        assert!(!is_read_only_command("printf x; touch created"));
        assert!(!is_read_only_command("date\ntouch created"));
        assert!(!is_read_only_command("date # harmless\ntouch created"));
        assert!(!is_read_only_command("./date"));
        assert!(!is_read_only_command("/tmp/date"));
        assert!(!is_read_only_command("date --set=tomorrow"));
        assert!(!is_read_only_command("date -stomorrow"));
        assert!(!is_read_only_command("hostname new-name"));
        assert!(!is_read_only_command("hostname --boot"));
        assert!(!is_read_only_command("find . -fprint output.txt"));
        assert!(!is_read_only_command("rg --pre 'touch created' pattern"));
        assert!(!is_read_only_command("sort -o output.txt input.txt"));
        assert!(!is_read_only_command("sort -ooutput.txt input.txt"));
        assert!(!is_read_only_command("uniq input.txt output.txt"));
        assert!(!is_read_only_command("ss --kill dst 192.0.2.1"));
        assert!(!is_read_only_command("ss -Knt dst 192.0.2.1"));
        assert!(!is_read_only_command("/usr/bin/systemctl restart sshd"));
        assert!(!is_read_only_command("/usr/bin/systemctl enable status"));
        assert!(!is_read_only_command(
            "/usr/bin/journalctl --vacuum-time=1d"
        ));
        assert!(!is_read_only_command("/usr/bin/dmesg -C"));
        assert!(!is_read_only_command("/usr/bin/ip link set eth0 down"));
        assert!(!is_read_only_command("/usr/bin/ip -batch route"));
        assert!(!is_read_only_command("/usr/bin/iptables -D INPUT 1"));
        assert!(!is_read_only_command(
            "/usr/bin/iptables -L --modprobe=/tmp/run-me"
        ));
        assert!(!is_read_only_command("/usr/bin/nft flush ruleset"));
        assert!(!is_read_only_command("/usr/bin/nft -f list"));
        assert!(!is_read_only_command(
            "/usr/bin/dpkg --install package.deb -l"
        ));
        assert!(!is_read_only_command("/usr/bin/apt install qin"));
        assert!(!is_read_only_command(
            "/usr/bin/apt-cache --generate policy qin"
        ));
        assert!(!is_read_only_command(
            "/usr/bin/apt -o APT::Update::Post-Invoke=touch list"
        ));
        assert!(!is_read_only_command(
            "/usr/sbin/sysctl -w kernel.hostname=changed"
        ));
        assert!(!is_read_only_command("/usr/bin/mount /dev/sda1 /mnt"));
        assert!(!is_read_only_command("git status"));
        assert!(!is_read_only_command("/tmp/date"));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_classifier_rejects_symlinks_from_untrusted_directories() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("date");
        symlink("/usr/bin/date", &fake).unwrap();
        assert!(trusted_program_name(fake.to_str().unwrap()).is_none());
    }

    #[test]
    fn identifies_hidden_shell_and_loader_injection_variables() {
        for name in [
            "ENV",
            "BASH_ENV",
            "BASH_FUNC_date%%",
            "LD_PRELOAD",
            "LD_DEBUG_OUTPUT",
            "DYLD_INSERT_LIBRARIES",
            "GCONV_PATH",
        ] {
            assert!(is_shell_injection_variable(name), "{name}");
        }
        for name in ["PATH", "LANG", "TERM", "HOME"] {
            assert!(!is_shell_injection_variable(name), "{name}");
        }
    }

    #[test]
    fn parses_one_time_and_task_wide_command_approvals() {
        assert_eq!(parse_approval_answer("y\n", true), ApprovalAnswer::Once);
        assert_eq!(parse_approval_answer("ALL\n", true), ApprovalAnswer::All);
        assert_eq!(
            parse_approval_answer("\u{5168}\u{90e8}\n", true),
            ApprovalAnswer::All
        );
        assert_eq!(parse_approval_answer("n\n", true), ApprovalAnswer::Rejected);
        assert_eq!(
            approval_outcome(ApprovalAnswer::Once),
            ApprovalOutcome::AllowedOnce
        );
        assert_eq!(
            approval_outcome(ApprovalAnswer::All),
            ApprovalOutcome::AllowedForTask
        );
    }

    #[test]
    fn read_only_shell_is_a_read_only_audit() {
        assert_eq!(risk_for("shell", &json!({"command": "date"})), "read_only");
        assert_eq!(
            risk_for("shell", &json!({"command": "touch file"})),
            "mutating"
        );
        assert_eq!(
            risk_for("shell", &json!({"command": "date", "elevated": true})),
            "destructive"
        );
    }

    #[test]
    fn on_risk_only_auto_approves_non_overwriting_workspace_writes() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let internal = resolve_target(dir.path(), "new.txt").unwrap();
        let external = dir
            .path()
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("outside.txt");
        assert!(auto_approves_path_mutation(
            &config,
            dir.path(),
            &[internal.as_path()],
            true,
        ));
        assert!(!auto_approves_path_mutation(
            &config,
            dir.path(),
            &[internal.as_path()],
            false,
        ));
        assert!(!auto_approves_path_mutation(
            &config,
            dir.path(),
            &[external.as_path()],
            true,
        ));
    }

    #[test]
    fn firewall_readonly_queries_are_not_dangerous() {
        // Read-only listings stay approvable via --yes.
        assert!(!dangerous(
            "sudo -n iptables -L -n 2>/dev/null | head -30 || iptables -L -n | head -30"
        ));
        assert!(!dangerous("sudo nft list ruleset | head -40"));
        assert!(!dangerous("iptables -t nat -S"));
        // Mutations always require manual confirmation.
        assert!(dangerous("iptables -F"));
        assert!(dangerous("iptables -A INPUT -d 1.2.3.4 -j DROP"));
        assert!(dangerous("ip6tables --flush"));
        assert!(dangerous("iptables-restore < /tmp/rules"));
        assert!(dangerous("nft add rule inet filter input drop"));
        assert!(dangerous("nft flush ruleset"));
        assert!(dangerous("iptables -L; iptables -D INPUT 1"));
    }

    #[test]
    fn redacts_tokens() {
        assert!(!redact("curl -H 'Bearer abc123'").contains("abc123"));
    }

    #[test]
    fn path_mutation_prompts_include_confirmation_suffix() {
        assert_eq!(
            path_mutation_prompt("Modify file /var/www/finance/.gitignore"),
            "Modify file /var/www/finance/.gitignore? [y/N] "
        );
    }

    #[test]
    fn tool_presentation_exposes_safe_diff_metadata() {
        let presentation = tool_presentation(
            "call-1",
            "apply_patch",
            &json!({
                "path": "requirements.txt",
                "old_text": "secret-old",
                "new_text": "secret-new"
            }),
            Path::new("/tmp/project"),
        );
        assert_eq!(presentation["card"], "diff");
        assert_eq!(presentation["kind"], "edit");
        assert_eq!(presentation["tool_call_id"], "call-1");
        assert_eq!(presentation["diffs"][0]["old_bytes"], 10);
        assert_eq!(presentation["diffs"][0]["new_bytes"], 10);
        assert_eq!(presentation["diffs"][0]["content_redacted"], true);
        let encoded = presentation.to_string();
        assert!(!encoded.contains("secret-old"));
        assert!(!encoded.contains("secret-new"));
    }

    #[test]
    fn any_tty_backed_child_gets_foreground_terminal_control() {
        assert!(child_needs_foreground_terminal(true));
        assert!(!child_needs_foreground_terminal(false));
    }

    #[cfg(unix)]
    #[test]
    fn a_foreground_child_killed_by_ctrl_c_counts_as_user_cancellation() {
        use std::os::unix::process::ExitStatusExt as _;
        let sigint = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("kill -INT $$")
            .status()
            .unwrap();
        assert_eq!(sigint.signal(), Some(libc::SIGINT));
        assert!(sigint_death_is_user_cancel(true, sigint));
        assert!(!sigint_death_is_user_cancel(false, sigint));

        // A normal exit, even with the 130 Ctrl-C convention, is a failure.
        let exit_130 = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 130")
            .status()
            .unwrap();
        assert!(!sigint_death_is_user_cancel(true, exit_130));
    }

    #[test]
    fn edit_tool_descriptions_require_exact_unique_replacements() {
        let apply_patch = tool_registry()
            .into_iter()
            .find(|definition| definition.name == "apply_patch")
            .unwrap();
        assert!(apply_patch.description.contains("exactly once"));
        assert!(
            apply_patch
                .description
                .contains("preserve unrelated content")
        );
        assert!(apply_patch.description.contains("verify the result"));
    }

    #[test]
    fn shell_description_forbids_wrappers_that_detach_the_terminal() {
        let shell = tool_registry()
            .into_iter()
            .find(|definition| definition.name == "shell")
            .unwrap();
        // Wrapping in timeout/setsid/nohup moves the command out of the
        // terminal foreground group, so interactive prompts stop working.
        assert!(shell.description.contains("timeout/setsid/nohup"));
        assert!(shell.description.contains("timeout_seconds"));
    }

    #[test]
    fn interactive_shell_guard_rejects_terminal_detaching_wrappers() {
        for command in [
            "timeout 20 git push origin main",
            "/usr/bin/setsid git push origin main",
            "nohup git push origin main",
            "env GIT_TERMINAL_PROMPT=1 timeout 20 git push origin main",
            "sudo -n timeout 20 git push origin main",
            "FOO=bar /usr/bin/nohup git push origin main",
            "sh -c 'timeout 20 git push origin main'",
        ] {
            let error = validate_interactive_shell_command(command, true).unwrap_err();
            assert!(
                error.to_string().contains("timeout_seconds"),
                "{command}: {error}"
            );
        }

        assert!(validate_interactive_shell_command("git push origin main", true).is_ok());
        assert!(
            validate_interactive_shell_command("timeout 20 git push origin main", false).is_ok()
        );
        assert!(validate_interactive_shell_command("echo 'timeout 20'", true).is_ok());
    }

    #[test]
    fn caps_output_and_normalizes_targets() {
        let mut value = String::new();
        assert!(append_capped(&mut value, "abcdef", 4));
        append_truncation_marker(&mut value, 20);
        assert!(value.len() <= 20);
        assert!(value.contains("truncated"));

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        let target = resolve_target(dir.path(), "real/../real/new.txt").unwrap();
        assert_eq!(
            target,
            dir.path().canonicalize().unwrap().join("real/new.txt")
        );
        let nested = resolve_target(dir.path(), "missing/parents/new.txt").unwrap();
        assert_eq!(
            nested,
            dir.path()
                .canonicalize()
                .unwrap()
                .join("missing/parents/new.txt")
        );
        assert!(validate_argument_keys("read_file", &json!({"path":"x","extra":1})).is_err());
    }

    #[tokio::test]
    async fn executes_shell_and_captures_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.enabled = true;
        config.storage.database = "tools.db".into();
        config.ui.command_heartbeat_seconds = 60;
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        let session = store.new_session(dir.path(), Some("tools")).unwrap();
        let events = EventSink::new(true, false, false);
        let mut approve_all_commands = false;
        let mut context = ToolContext {
            config: &config,
            events: &events,
            store: &mut store,
            session_id: &session,
            turn_id: "test-turn",
            tool_call_id: "test-call",
            tool_name: "shell",
            cwd: dir.path(),
            assume_yes: true,
            approve_all_commands: &mut approve_all_commands,
            dry_run: false,
        };
        let result = execute(
            "call-test",
            "shell",
            r#"{"command":"printf 'qin-shell你好'"}"#,
            &mut context,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.content.contains("qin-shell"));
        assert!(result.content.contains("你好"));
        assert!(result.content.contains("exit_code=0"));
    }

    #[tokio::test]
    async fn on_risk_runs_read_only_shell_without_interactive_approval() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.enabled = true;
        config.storage.database = "readonly-shell.db".into();
        config.ui.command_heartbeat_seconds = 60;
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        let session = store
            .new_session(dir.path(), Some("readonly-shell"))
            .unwrap();
        let events = EventSink::new(true, false, false);
        let mut approve_all_commands = false;
        let mut context = ToolContext {
            config: &config,
            events: &events,
            store: &mut store,
            session_id: &session,
            turn_id: "test-turn",
            tool_call_id: "test-call",
            tool_name: "shell",
            cwd: dir.path(),
            assume_yes: false,
            approve_all_commands: &mut approve_all_commands,
            dry_run: false,
        };
        let result = execute(
            "call-readonly-shell",
            "shell",
            r#"{"command":"date '+%Y-%m-%d %H:%M:%S %Z (%A)'"}"#,
            &mut context,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.content.contains("exit_code=0"));
    }

    #[tokio::test]
    async fn task_wide_approval_runs_later_unknown_shell_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.enabled = true;
        config.storage.database = "approve-all-shell.db".into();
        config.ui.command_heartbeat_seconds = 60;
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        let session = store.new_session(dir.path(), Some("approve-all")).unwrap();
        let events = EventSink::new(true, false, false);
        let mut approve_all_commands = true;
        let mut context = ToolContext {
            config: &config,
            events: &events,
            store: &mut store,
            session_id: &session,
            turn_id: "test-turn",
            tool_call_id: "test-call",
            tool_name: "shell",
            cwd: dir.path(),
            assume_yes: false,
            approve_all_commands: &mut approve_all_commands,
            dry_run: false,
        };
        execute(
            "call-approved-for-task",
            "shell",
            r#"{"command":"touch approved-for-task"}"#,
            &mut context,
        )
        .await
        .unwrap();
        assert!(dir.path().join("approved-for-task").is_file());
    }

    #[tokio::test]
    async fn rejects_model_calls_to_disabled_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.enabled = true;
        config.storage.database = "disabled-tools.db".into();
        config.permissions.allow_shell = false;
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        let session = store.new_session(dir.path(), Some("tools")).unwrap();
        let events = EventSink::new(true, false, false);
        let mut approve_all_commands = false;
        let mut context = ToolContext {
            config: &config,
            events: &events,
            store: &mut store,
            session_id: &session,
            turn_id: "test-turn",
            tool_call_id: "test-call",
            tool_name: "shell",
            cwd: dir.path(),
            assume_yes: true,
            approve_all_commands: &mut approve_all_commands,
            dry_run: false,
        };
        let error = execute(
            "call-disabled",
            "shell",
            r#"{"command":"touch should-not-exist"}"#,
            &mut context,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("disabled"));
        assert!(!dir.path().join("should-not-exist").exists());
    }
}
