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
