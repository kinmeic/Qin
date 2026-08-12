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
                "普通权限"
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
                "普通权限"
            };
            self.stderr(
                "command_preview",
                &format!(
                    "→ 准备执行命令 [{level}]  cwd={}  timeout={}s\n  $ {}",
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
            self.stderr("command_heartbeat", &format!("… 命令仍在运行  {seconds}s"))?;
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
                "{} 命令执行{}  exit={}  {:.2}s",
                if ok { "✓" } else { "✗" },
                if ok { "成功" } else { "失败" },
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
                    "✓ 已加载提示词文件  path={}  bytes={}  sha256={}",
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
                    "next_action": "编辑模型与 Embedding 配置，然后运行 qin config check"
                }))?
            );
            return Ok(());
        }

        if outcome.created {
            println!("✓ qin 配置文件已创建");
        } else {
            println!("✓ qin 配置文件已存在，未作修改");
        }
        println!("  范围：{}", outcome.scope.label());
        println!("  路径：{}", outcome.config_path.display());
        if cfg!(target_os = "macos") {
            println!("  编辑：open -e {}", shell_quote(&outcome.config_path));
        } else {
            println!(
                "  编辑：${{EDITOR:-vi}} {}",
                shell_quote(&outcome.config_path)
            );
        }
        if let Some(backup) = outcome.backup_path.as_ref() {
            println!("  备份：{}", backup.display());
        }
        println!("  下一步：编辑模型与 Embedding 配置，然后运行 qin config check");
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
            println!("范围：{}", resolver.scope().label());
            println!("配置：{}", resolver.config_path().display());
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
