mod agent;
mod cli;
mod config;
mod event;
mod prompt_file;

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::cli::{Cli, Command, ConfigCommand};
use crate::config::{ConfigPathResolver, InitOptions};
use crate::event::EventSink;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("错误：{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse_normalized();
    let events = EventSink::new(cli.quiet, cli.json);

    match cli.command {
        Command::Init {
            system,
            force,
            edit,
        } => {
            let resolver = ConfigPathResolver::new(cli.config, system)?;
            let outcome = config::initialize(&resolver, &InitOptions { force, edit })?;
            events.init_outcome(&outcome)?;
        }
        Command::Config { command } => {
            let resolver = ConfigPathResolver::new(cli.config, false)?;
            match command {
                ConfigCommand::Path => events.config_path(&resolver)?,
                ConfigCommand::Check => {
                    let loaded = config::load(&resolver)?;
                    loaded.validate(true)?;
                    events.success(&format!("配置有效：{}", resolver.config_path().display()))?;
                }
            }
        }
        Command::Fromfile { path } => {
            execute_from_file(&path, &cli.config, &events).await?;
        }
        Command::Run { prompt } => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                bail!("提示词不能为空");
            }
            execute_prompt(prompt, "cli", None, &cli.config, &events).await?;
        }
    }

    Ok(())
}

async fn execute_from_file(
    path: &Path,
    explicit_config: &Option<std::path::PathBuf>,
    events: &EventSink,
) -> Result<()> {
    let resolver = ConfigPathResolver::new(explicit_config.clone(), false)?;
    let loaded_config = config::load(&resolver)?;
    loaded_config.validate(false)?;

    events.tool_started("read_prompt_file", &format!("path={}", path.display()))?;
    let loaded = prompt_file::load(path, &loaded_config.input)
        .with_context(|| format!("无法加载提示词文件 {}", path.display()))?;
    events.prompt_file_loaded(&loaded)?;

    execute_prompt_with_config(
        &loaded_config,
        loaded.content,
        "file",
        Some(loaded.canonical_path),
        events,
    )
    .await
}

async fn execute_prompt(
    prompt: String,
    source: &str,
    source_path: Option<std::path::PathBuf>,
    explicit_config: &Option<std::path::PathBuf>,
    events: &EventSink,
) -> Result<()> {
    let resolver = ConfigPathResolver::new(explicit_config.clone(), false)?;
    let loaded_config = config::load(&resolver)?;
    loaded_config.validate(false)?;

    execute_prompt_with_config(&loaded_config, prompt, source, source_path, events).await
}

async fn execute_prompt_with_config(
    loaded_config: &config::Config,
    prompt: String,
    source: &str,
    source_path: Option<std::path::PathBuf>,
    events: &EventSink,
) -> Result<()> {
    events.phase("正在调用模型…")?;
    let response = agent::execute(loaded_config, &prompt, source, source_path.as_deref()).await?;
    events.final_answer(&response)?;
    Ok(())
}
