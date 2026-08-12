use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
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
    if config.permissions.allow_shell {
        tools.push(tool("shell", "Run a shell command. The command is displayed and approval is requested according to risk before execution", json!({"type":"object","properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer"},"elevated":{"type":"boolean"}},"required":["command"]})));
    }
    tools
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
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
    let started = Instant::now();
    let audit_args = safe_args(name, &args);
    ctx.events.tool_started(name, &audit_args)?;
    let result = execute_inner(name, &args, ctx).await;
    match &result {
        Ok(value) => ctx.events.tool_finished(
            name,
            &one_line(&value.content),
            started.elapsed().as_millis(),
        )?,
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
        &redact(audit_text),
        status,
        risk(name),
        exit,
        started.elapsed().as_millis() as u64,
    )?;
    result
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

fn list_directory(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve(ctx.cwd, string(args, "path")?);
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
    let path = resolve(ctx.cwd, string(args, "path")?);
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
    text_result(fs::read_to_string(path).context("The file is not valid UTF-8 text")?)
}

fn stat_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve(ctx.cwd, string(args, "path")?);
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
    let path = resolve(ctx.cwd, string(args, "path")?);
    approve_mutation(ctx, &format!("Create directory {}", path.display()))?;
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
    let path = resolve(ctx.cwd, string(args, "path")?);
    reject_symlink_target(&path)?;
    let content = string(args, "content")?;
    approve_mutation(
        ctx,
        &format!("Write {} ({} bytes)", path.display(), content.len()),
    )?;
    if !ctx.dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
    }
    text_result(if ctx.dry_run {
        "Dry run: file not written".into()
    } else {
        format!("Wrote {} bytes", content.len())
    })
}

fn move_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let src = resolve(ctx.cwd, string(args, "source")?);
    let dst = resolve(ctx.cwd, string(args, "destination")?);
    reject_symlink_target(&dst)?;
    if dst.exists() && !args["overwrite"].as_bool().unwrap_or(false) {
        bail!("Destination already exists: {}", dst.display())
    }
    approve_mutation(ctx, &format!("Move {} -> {}", src.display(), dst.display()))?;
    if !ctx.dry_run {
        if dst.exists() {
            remove_existing(&dst)?;
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
    let src = resolve(ctx.cwd, string(args, "source")?);
    let dst = resolve(ctx.cwd, string(args, "destination")?);
    reject_symlink_target(&dst)?;
    if !src.is_file() {
        bail!("copy_path currently supports regular files only")
    }
    if dst.exists() && !args["overwrite"].as_bool().unwrap_or(false) {
        bail!("Destination already exists: {}", dst.display())
    }
    approve_mutation(ctx, &format!("Copy {} -> {}", src.display(), dst.display()))?;
    if !ctx.dry_run {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst)?;
    }
    text_result(if ctx.dry_run {
        "Dry run: file not copied".into()
    } else {
        "Copy completed".into()
    })
}

fn remove_path(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let path = resolve(ctx.cwd, string(args, "path")?);
    guard_delete(&path, ctx.cwd)?;
    let use_trash = ctx.config.permissions.trash_instead_of_delete;
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
        !use_trash,
    )?;
    if !ctx.dry_run {
        if use_trash {
            let trash = ctx
                .store
                .path()
                .parent()
                .context("The database has no parent directory")?
                .join("trash");
            fs::create_dir_all(&trash)?;
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("item");
            fs::rename(
                &path,
                trash.join(format!("{}-{name}", uuid::Uuid::new_v4())),
            )?;
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
    let path = resolve(ctx.cwd, string(args, "path")?);
    reject_symlink_target(&path)?;
    let old = string(args, "old_text")?;
    let new = string(args, "new_text")?;
    let content = fs::read_to_string(&path)?;
    let count = content.matches(old).count();
    if count != 1 {
        bail!("old_text must match exactly once; found {count} matches")
    }
    approve_mutation(ctx, &format!("Modify file {}", path.display()))?;
    if !ctx.dry_run {
        fs::write(path, content.replacen(old, new, 1))?;
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
        .min(3600);
    ctx.events
        .command_preview(ctx.cwd, command, elevated, timeout)?;
    approve(
        ctx,
        &format!("Allow command `{}`? [y/N] ", redact(command)),
        elevated || dangerous(command),
    )?;
    ctx.events
        .command_started(ctx.cwd, command, elevated, timeout)?;
    if ctx.dry_run {
        return text_result("Dry run: command not executed".into());
    }
    let started = Instant::now();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut process = if elevated && unsafe { libc::geteuid() } != 0 {
        let mut p = Command::new(elevation_program(&ctx.config.permissions.elevation)?);
        p.arg(&shell).arg("-c").arg(command);
        p
    } else {
        let mut p = Command::new(&shell);
        p.arg("-c").arg(command);
        p
    };
    let mut child = process
        .current_dir(ctx.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("Unable to capture stdout")?;
    let stderr = child.stderr.take().context("Unable to capture stderr")?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_line_reader("stdout", stdout, tx.clone());
    spawn_line_reader("stderr", stderr, tx.clone());
    drop(tx);
    let mut output = String::new();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(
        ctx.config.ui.command_heartbeat_seconds.max(1),
    ));
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some((label, line)) => {
                    if output.len() < ctx.config.permissions.max_output_bytes {
                        output.push_str(label);
                        output.push_str(": ");
                        output.push_str(&line);
                        output.push('\n');
                    }
                    if ctx.config.ui.stream_command_output {
                        ctx.events.command_output(label, &line)?;
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
    ctx.events
        .command_finished(status.code(), started.elapsed().as_millis())?;
    Ok(ToolResult {
        content: truncate(output, ctx.config.permissions.max_output_bytes),
        exit_code: status.code(),
    })
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
    approve_mutation(ctx, "Save an item to long-term memory")?;
    if ctx.dry_run {
        return text_result("Dry run: memory not saved".into());
    }
    let added = knowledge::add_memory(ctx.store, ctx.config, string(args, "content")?).await?;
    text_result(if added {
        "Memory saved".into()
    } else {
        "An identical memory already exists".into()
    })
}

async fn web_search(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolResult> {
    let query = string(args, "query")?;
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
    bail!("No search backend succeeded: {}", errors.join("; "))
}

async fn search_exa(config: &Config, query: &str, limit: usize) -> Result<String> {
    let key = config.search.exa.secret("Exa")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.search.timeout_seconds,
        ))
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
    Ok(serde_json::to_string_pretty(
        &response.json::<Value>().await?["results"],
    )?)
}
async fn search_brave(config: &Config, query: &str, limit: usize) -> Result<String> {
    let key = config.search.brave.secret("Brave")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.search.timeout_seconds,
        ))
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
    Ok(serde_json::to_string_pretty(
        &response.json::<Value>().await?["web"]["results"],
    )?)
}

async fn search_native(config: &Config, query: &str) -> Result<String> {
    let model = config.primary_model()?;
    if !model.supports_native_search {
        bail!("The primary model does not declare supports_native_search=true");
    }
    let endpoint = format!("{}/responses", model.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.search.timeout_seconds.max(30),
        ))
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
    let value = response.json::<Value>().await?;
    if let Some(text) = value["output_text"].as_str() {
        return Ok(text.to_string());
    }
    Ok(serde_json::to_string_pretty(&value["output"])?)
}

fn approve_mutation(ctx: &ToolContext<'_>, message: &str) -> Result<()> {
    if !ctx.config.permissions.workspace_write {
        bail!("The configuration does not allow workspace writes")
    }
    approve(ctx, message, false)
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
    ctx.events.approval(message)?;
    eprint!("{message}");
    io::stderr().flush()?;
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
    [
        "rm -rf",
        "mkfs",
        "dd if=",
        "shutdown",
        "reboot",
        "iptables",
        "nft ",
        "> /etc/",
        "curl | sh",
        "curl|sh",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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
fn remove_existing(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?
    } else {
        fs::remove_file(path)?
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
        value.push_str("\n[Output truncated]");
    }
    value
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

fn spawn_line_reader<R>(
    label: &'static str,
    reader: R,
    tx: tokio::sync::mpsc::UnboundedSender<(&'static str, String)>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send((label, line));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigPathResolver;
    #[test]
    fn detects_dangerous_commands() {
        assert!(dangerous("rm -rf /tmp/x"));
        assert!(!dangerous("cargo test"));
    }
    #[test]
    fn redacts_tokens() {
        assert!(!redact("curl -H 'Bearer abc123'").contains("abc123"));
    }

    #[tokio::test]
    async fn executes_shell_and_captures_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.database = "tools.db".into();
        config.ui.command_heartbeat_seconds = 60;
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        let session = store.new_session(dir.path(), Some("tools")).unwrap();
        let events = EventSink::new(true, false);
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
            r#"{"command":"printf qin-shell"}"#,
            &mut context,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.content.contains("qin-shell"));
    }
}
