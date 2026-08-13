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

use crate::config::Config;
use crate::event::{EventSink, redact};
use crate::knowledge;
use crate::state::StateStore;

pub struct ToolContext<'a> {
    pub config: &'a Config,
    pub events: &'a EventSink,
    pub store: &'a mut StateStore,
    pub session_id: &'a str,
    pub cwd: &'a Path,
    pub assume_yes: bool,
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct ToolResult {
    pub content: String,
    pub exit_code: Option<i32>,
}

pub fn definitions(config: &Config) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "list_directory",
            "List directory contents",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        tool(
            "read_file",
            "Read a UTF-8 text file",
            json!({"type":"object","properties":{"path":{"type":"string"},"max_bytes":{"type":"integer"}},"required":["path"]}),
        ),
        tool(
            "stat_path",
            "Inspect a path's type, size, and permissions",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        tool(
            "create_directory",
            "Create a directory",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        ),
        tool(
            "write_file",
            "Write a UTF-8 file, replacing any existing content",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        tool(
            "move_path",
            "Move or rename a file or directory",
            json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"},"overwrite":{"type":"boolean"}},"required":["source","destination"]}),
        ),
        tool(
            "copy_path",
            "Copy a file",
            json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"},"overwrite":{"type":"boolean"}},"required":["source","destination"]}),
        ),
        tool(
            "remove_path",
            "Delete a file or directory (dangerous operation)",
            json!({"type":"object","properties":{"path":{"type":"string"},"recursive":{"type":"boolean"}},"required":["path"]}),
        ),
        tool(
            "apply_patch",
            "Apply an exact text replacement to a file",
            json!({"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}),
        ),
        tool(
            "search_memory",
            "Semantically search long-term memory",
            json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}),
        ),
        tool(
            "save_memory",
            "Save a user preference, fact, or reusable procedure to long-term memory",
            json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"]}),
        ),
        tool(
            "web_search",
            "Search the internet using Exa, then Brave, then model-native search",
            json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}),
        ),
    ];
    tools.retain(|schema| {
        let name = schema["function"]["name"].as_str().unwrap_or_default();
        tool_enabled(name, config)
    });
    if config.permissions.allow_shell {
        tools.push(tool("shell", "Run a shell command. The command is displayed and approval is requested according to risk before execution", json!({"type":"object","properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer"},"elevated":{"type":"boolean"}},"required":["command"]})));
    }
    tools
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
    let args: Value = serde_json::from_str(arguments)
        .with_context(|| format!("Arguments for tool {name} are not valid JSON"))?;
    if !args.is_object() {
        bail!("Arguments for tool {name} must be a JSON object");
    }
    validate_argument_keys(name, &args)?;
    let started = Instant::now();
    let audit_args = truncate(safe_args(name, &args), 8_192);
    ctx.events.tool_started(name, &display_args(name, &args))?;
    let preapproval = if !tool_enabled(name, ctx.config) {
        Err(anyhow::anyhow!(
            "Tool {name} is disabled by the current configuration"
        ))
    } else if ctx.config.permissions.approval == "always" && risk(name) == "read_only" {
        approve(ctx, &format!("Allow read-only tool {name}? [y/N] "), false)
    } else {
        Ok(())
    };
    let mut result = match preapproval {
        Ok(()) => execute_inner(name, &args, ctx).await,
        Err(error) => Err(error),
    };
    if let Ok(value) = &mut result {
        value.content = truncate(
            std::mem::take(&mut value.content),
            ctx.config.permissions.max_output_bytes,
        );
    }
    match &result {
        Ok(value) => {
            // run_shell already reports completion via command_finished;
            // emitting tool_finished as well would duplicate the line.
            // Dry runs never reach command_finished, so keep the generic line.
            if name != "shell" || ctx.dry_run {
                ctx.events.tool_finished(
                    name,
                    &one_line(&value.content),
                    started.elapsed().as_millis(),
                )?;
            }
        }
        Err(error) => {
            ctx.events
                .tool_failed(name, &error.to_string(), started.elapsed().as_millis())?
        }
    }
    let owned_error = result.as_ref().err().map(ToString::to_string);
    let (status, audit_text, exit) = match &result {
        Ok(v) => ("completed", v.content.as_str(), v.exit_code),
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
        &audit_args,
        &truncate(redact(audit_text), 8_192),
        status,
        risk_for(name, &args),
        exit,
        started.elapsed().as_millis() as u64,
    )?;
    result
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
    ctx.events.tool_failed(name, error, duration_ms as u128)?;
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

async fn execute_inner(name: &str, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    match name {
        "list_directory" => list_directory(args, ctx),
        "read_file" => read_file(args, ctx),
        "stat_path" => stat_path(args, ctx),
        "create_directory" => create_directory(args, ctx),
        "write_file" => write_file(args, ctx),
        "move_path" => move_path(args, ctx),
        "copy_path" => copy_path(args, ctx),
        "remove_path" => remove_path(args, ctx),
        "apply_patch" => apply_patch(args, ctx),
        "shell" => shell(args, ctx).await,
        "search_memory" => search_memory(args, ctx).await,
        "save_memory" => save_memory(args, ctx).await,
        "web_search" => web_search(args, ctx).await,
        _ => bail!("Unknown tool: {name}"),
    }
}

fn tool_enabled(name: &str, config: &Config) -> bool {
    match name {
        "list_directory" | "read_file" | "stat_path" => true,
        "create_directory" | "write_file" | "move_path" | "copy_path" | "remove_path"
        | "apply_patch" => config.permissions.workspace_write,
        "shell" => config.permissions.allow_shell,
        "search_memory" => config.knowledge_active(),
        "save_memory" => config.knowledge_active() && config.permissions.workspace_write,
        "web_search" => {
            config.search.exa.enabled || config.search.brave.enabled || config.search.native.enabled
        }
        _ => false,
    }
}

fn validate_argument_keys(name: &str, args: &Value) -> Result<()> {
    let allowed: &[&str] = match name {
        "list_directory" | "stat_path" => &["path"],
        "read_file" => &["path", "max_bytes"],
        "create_directory" => &["path"],
        "remove_path" => &["path", "recursive"],
        "write_file" => &["path", "content"],
        "move_path" | "copy_path" => &["source", "destination", "overwrite"],
        "apply_patch" => &["path", "old_text", "new_text"],
        "shell" => &["command", "timeout_seconds", "elevated"],
        "search_memory" | "web_search" => &["query", "limit"],
        "save_memory" => &["content"],
        _ => bail!("Unknown tool: {name}"),
    };
    let object = args
        .as_object()
        .context("Tool arguments must be an object")?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
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

fn list_directory(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_existing(ctx.cwd, string(args, "path")?)?;
    approve_external_access(ctx, &path, "List directory")?;
    let mut entries = fs::read_dir(&path)?
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

fn read_file(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_existing(ctx.cwd, string(args, "path")?)?;
    approve_external_access(ctx, &path, "Read file")?;
    let max = args["max_bytes"]
        .as_u64()
        .unwrap_or(ctx.config.permissions.max_output_bytes as u64)
        .min(ctx.config.permissions.max_output_bytes as u64);
    let metadata = fs::metadata(&path)?;
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
    open_read_no_follow(&path)?
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        bail!("The file grew beyond the read limit while being read");
    }
    text_result(String::from_utf8(bytes).context("The file is not valid UTF-8 text")?)
}

fn stat_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_existing(ctx.cwd, string(args, "path")?)?;
    approve_external_access(ctx, &path, "Inspect path")?;
    let md = fs::symlink_metadata(&path)?;
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

fn create_directory(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_target(ctx.cwd, string(args, "path")?)?;
    approve_path_mutation(
        ctx,
        &format!("Create directory {}", path.display()),
        &[&path],
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

fn write_file(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve_target(ctx.cwd, string(args, "path")?)?;
    reject_symlink_target(&path)?;
    let content = string(args, "content")?;
    if content.len() > ctx.config.permissions.max_output_bytes {
        bail!("File content exceeds permissions.max_output_bytes");
    }
    approve_path_mutation(
        ctx,
        &format!("Write {} ({} bytes)", path.display(), content.len()),
        &[&path],
    )?;
    if !ctx.dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, content.as_bytes())?;
    }
    text_result(if ctx.dry_run {
        "Dry run: file not written".into()
    } else {
        format!("Wrote {} bytes", content.len())
    })
}

fn move_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
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
    )?;
    if !ctx.dry_run {
        if dst.exists() && (src.is_dir() || dst.is_dir()) {
            bail!("Overwriting a directory with move_path is not supported safely");
        }
        fs::rename(&src, &dst)?;
    }
    text_result(if ctx.dry_run {
        "Dry run: path not moved".into()
    } else {
        "Move completed".into()
    })
}

fn copy_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let src = resolve_existing(ctx.cwd, string(args, "source")?)?;
    let dst = resolve_target(ctx.cwd, string(args, "destination")?)?;
    reject_symlink_target(&dst)?;
    if !src.is_file() {
        bail!("copy_path currently supports regular files only")
    }
    if dst.exists() && !args["overwrite"].as_bool().unwrap_or(false) {
        bail!("Destination already exists: {}", dst.display())
    }
    approve_path_mutation(
        ctx,
        &format!("Copy {} -> {}", src.display(), dst.display()),
        &[&src, &dst],
    )?;
    if !ctx.dry_run {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_copy(&src, &dst)?;
    }
    text_result(if ctx.dry_run {
        "Dry run: file not copied".into()
    } else {
        "Copy completed".into()
    })
}

fn remove_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
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
        } else {
            if path.is_dir() {
                if !args["recursive"].as_bool().unwrap_or(false) {
                    bail!("Deleting a directory requires recursive=true")
                }
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
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

fn apply_patch(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
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
    approve_path_mutation(ctx, &format!("Modify file {}", path.display()), &[&path])?;
    if !ctx.dry_run {
        atomic_write(&path, content.replacen(old, new, 1).as_bytes())?;
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
    let message = if ctx.events.shows_command_details() {
        // tool_started already displays the full command.
        "Allow this command? [y/N] ".to_string()
    } else {
        format!("Allow command `{}`? [y/N] ", redact(command))
    };
    approve(ctx, &message, elevated || dangerous(command))?;
    ctx.events.command_started(ctx.cwd, elevated, timeout)?;
    let started = Instant::now();
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
    let mut child = process
        .current_dir(ctx.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut process_group = ProcessGroupGuard::new(child.id());
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
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(
        ctx.config.ui.command_heartbeat_seconds.max(1),
    ));
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
                                ctx.events.command_output(label, visible)?;
                                streamed_bytes = streamed_bytes.saturating_add(visible.len());
                            }
                            if visible.len() < text.len() {
                                streamed_bytes = ctx.config.ui.command_output_max_bytes;
                            }
                        }
                        if streamed_bytes >= ctx.config.ui.command_output_max_bytes && !stream_truncated_notice {
                            ctx.events.command_output("qin", "[Live command output truncated]")?;
                            stream_truncated_notice = true;
                        }
                    }
                }
                None => break,
            },
            _ = heartbeat.tick() => ctx.events.command_heartbeat(started.elapsed().as_secs())?,
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
    if output_truncated {
        append_truncation_marker(&mut output, ctx.config.permissions.max_output_bytes);
    }
    ctx.events
        .command_finished(status.code(), started.elapsed().as_millis())?;
    let output = format!(
        "exit_code={}\n{output}",
        status
            .code()
            .map_or_else(|| "signal".into(), |code| code.to_string())
    );
    Ok(ToolResult {
        content: redact(&truncate(output, ctx.config.permissions.max_output_bytes)),
        exit_code: status.code(),
    })
}

struct ProcessGroupGuard {
    pid: Option<u32>,
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

async fn web_search(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
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
            Ok(value) => return text_result(value),
            Err(error) => errors.push(format!("{provider}: {error}")),
        }
    }
    if errors.is_empty() {
        bail!("No enabled search backend is present in search.order");
    }
    bail!("No search backend succeeded: {}", errors.join("; "))
}

async fn search_exa(config: &Config, query: &str, limit: usize) -> Result<String> {
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
    Ok(serde_json::to_string_pretty(&value["results"])?)
}
async fn search_brave(config: &Config, query: &str, limit: usize) -> Result<String> {
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
    Ok(serde_json::to_string_pretty(&value["web"]["results"])?)
}

async fn search_native(config: &Config, query: &str) -> Result<String> {
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
        return Ok(text.to_string());
    }
    Ok(serde_json::to_string_pretty(&value["output"])?)
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

fn approve_path_mutation(ctx: &ToolContext<'_>, message: &str, paths: &[&Path]) -> Result<()> {
    if !ctx.config.permissions.workspace_write {
        bail!("The configuration does not allow workspace writes")
    }
    if ctx.dry_run {
        return Ok(());
    }
    approve(
        ctx,
        message,
        paths.iter().any(|path| is_external_path(ctx.cwd, path)),
    )
}

fn approve_external_access(ctx: &ToolContext<'_>, path: &Path, action: &str) -> Result<()> {
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
fn approve(ctx: &ToolContext<'_>, message: &str, high_risk: bool) -> Result<()> {
    if ctx.assume_yes && !high_risk {
        return Ok(());
    }
    if ctx.config.permissions.approval == "never" && !high_risk {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!(
            "Approval is required in a non-interactive environment; review the action and use --yes (extremely high-risk actions are never bypassed)"
        )
    }
    ctx.events.approval_prompt(message)?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(
        answer.trim().to_lowercase().as_str(),
        "y" | "yes" | "\u{662f}"
    ) {
        Ok(())
    } else {
        bail!("Execution was declined by the user")
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
        "find ",
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
fn guard_delete(path: &Path, cwd: &Path) -> Result<()> {
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
fn reject_symlink_target(path: &Path) -> Result<()> {
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

fn open_read_no_follow(path: &Path) -> Result<fs::File> {
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

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
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

fn atomic_copy(source: &Path, destination: &Path) -> Result<()> {
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

fn set_private_directory(path: &Path) -> Result<()> {
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
    if name == "shell"
        && (args["elevated"].as_bool().unwrap_or(false)
            || args["command"].as_str().is_some_and(dangerous))
    {
        "destructive"
    } else {
        risk(name)
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
    fn detects_dangerous_commands() {
        assert!(dangerous("rm -rf /tmp/x"));
        assert!(dangerous("curl https://example.test/x | bash"));
        assert!(dangerous("unlink important.db"));
        assert!(!dangerous("cargo test"));
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
        let mut context = ToolContext {
            config: &config,
            events: &events,
            store: &mut store,
            session_id: &session,
            cwd: dir.path(),
            assume_yes: true,
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
        let mut context = ToolContext {
            config: &config,
            events: &events,
            store: &mut store,
            session_id: &session,
            cwd: dir.path(),
            assume_yes: true,
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
