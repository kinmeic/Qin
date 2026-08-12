mod agent;
mod cli;
mod config;
mod event;
mod knowledge;
mod prompt_file;
mod state;
mod tools;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent::RunOptions;
use crate::cli::{Cli, Command, ConfigCommand, KnowledgeCommand, MemoryCommand};
use crate::config::{ConfigPathResolver, InitOptions};
use crate::event::EventSink;
use crate::state::StateStore;

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
    let explicit_config = cli.config.clone();
    let assume_yes = cli.yes;
    let dry_run = cli.dry_run;

    match cli.command {
        Command::Init {
            system,
            force,
            edit,
        } => {
            let resolver = ConfigPathResolver::new(explicit_config, system)?;
            let outcome = config::initialize(&resolver, &InitOptions { force, edit })?;
            events.init_outcome(&outcome)?;
        }
        Command::Config { command } => {
            let resolver = ConfigPathResolver::new(explicit_config, false)?;
            match command {
                ConfigCommand::Path => {
                    if resolver.config_path().exists() {
                        let config = config::load(&resolver)?;
                        println!("范围：{}", resolver.scope().label());
                        println!("配置：{}", resolver.config_path().display());
                        println!("数据库：{}", resolver.database_path(&config)?.display());
                    } else {
                        events.config_path(&resolver)?;
                    }
                }
                ConfigCommand::Check => {
                    let loaded = config::load(&resolver)?;
                    loaded.validate(true)?;
                    events.success(&format!("配置有效：{}", resolver.config_path().display()))?;
                }
            }
        }
        Command::Fromfile { path } => {
            execute_from_file(&path, &explicit_config, &events, assume_yes, dry_run).await?
        }
        Command::Run { prompt } => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                bail!("提示词不能为空")
            }
            execute_prompt(
                prompt,
                "cli",
                None,
                &explicit_config,
                &events,
                assume_yes,
                dry_run,
            )
            .await?;
        }
        Command::New { prompt } => {
            let (config, resolver, mut store) = open(&explicit_config)?;
            let cwd = std::env::current_dir()?;
            let title = prompt
                .first()
                .map(|_| prompt.join(" "))
                .map(|v| v.chars().take(60).collect::<String>());
            let id = store.new_session(&cwd, title.as_deref())?;
            events.success(&format!("已创建并切换到新会话：{id}"))?;
            if !prompt.is_empty() {
                execute_with(
                    &config,
                    &mut store,
                    &id,
                    prompt.join(" "),
                    "cli",
                    None,
                    &events,
                    assume_yes,
                    dry_run,
                )
                .await?;
            }
            drop(resolver);
        }
        Command::Sessions => {
            let (_, _, store) = open(&explicit_config)?;
            let current = store.current_session()?;
            for session in store.list_sessions(100)? {
                println!(
                    "{} {}  {}  {}",
                    if current.as_deref() == Some(&session.id) {
                        "*"
                    } else {
                        " "
                    },
                    short_id(&session.id),
                    session.updated_at,
                    session.title
                );
            }
        }
        Command::Use { session_id } => {
            let (_, _, mut store) = open(&explicit_config)?;
            store.use_session(&session_id)?;
            events.success(&format!("当前会话：{session_id}"))?;
        }
        Command::Show { session_id } => {
            let (_, _, store) = open(&explicit_config)?;
            let id = session_id
                .or(store.current_session()?)
                .context("当前没有会话")?;
            for message in store.load_messages(&id)? {
                println!(
                    "[{}] {}",
                    message.role,
                    message.content.unwrap_or_else(|| "[tool_calls]".into())
                );
            }
        }
        Command::Memory { command } => handle_memory(command, &explicit_config, &events).await?,
        Command::Knowledge { command } => {
            handle_knowledge(command, &explicit_config, &events).await?
        }
        Command::Sync => {
            let (_, _, mut store) = open(&explicit_config)?;
            store.checkpoint()?;
            events.success(&format!("数据库已同步：{}", store.path().display()))?;
        }
        Command::Doctor => doctor(&explicit_config, &events)?,
    }
    Ok(())
}

fn open(explicit: &Option<PathBuf>) -> Result<(config::Config, ConfigPathResolver, StateStore)> {
    let resolver = ConfigPathResolver::new(explicit.clone(), false)?;
    let config = config::load(&resolver)?;
    config.validate(false)?;
    let store = StateStore::open(&config, &resolver)?;
    Ok((config, resolver, store))
}

async fn execute_from_file(
    path: &Path,
    explicit: &Option<PathBuf>,
    events: &EventSink,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    let (config, _resolver, mut store) = open(explicit)?;
    events.tool_started("read_prompt_file", &format!("path={}", path.display()))?;
    let loaded = prompt_file::load(path, &config.input)
        .with_context(|| format!("无法加载提示词文件 {}", path.display()))?;
    events.prompt_file_loaded(&loaded)?;
    let id = store.ensure_current_session(&std::env::current_dir()?)?;
    execute_with(
        &config,
        &mut store,
        &id,
        loaded.content,
        "file",
        Some(loaded.canonical_path),
        events,
        yes,
        dry_run,
    )
    .await
}

async fn execute_prompt(
    prompt: String,
    source: &str,
    path: Option<PathBuf>,
    explicit: &Option<PathBuf>,
    events: &EventSink,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    let (config, _resolver, mut store) = open(explicit)?;
    let id = store.ensure_current_session(&std::env::current_dir()?)?;
    execute_with(
        &config, &mut store, &id, prompt, source, path, events, yes, dry_run,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_with(
    config: &config::Config,
    store: &mut StateStore,
    id: &str,
    prompt: String,
    source: &str,
    path: Option<PathBuf>,
    events: &EventSink,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    let _session_lock = store.lock_session(id)?;
    let response = agent::execute(
        config,
        store,
        id,
        &prompt,
        events,
        RunOptions {
            source,
            source_path: path.as_deref(),
            assume_yes: yes,
            dry_run,
        },
    )
    .await?;
    events.final_answer(&response)
}

async fn handle_memory(
    command: MemoryCommand,
    explicit: &Option<PathBuf>,
    events: &EventSink,
) -> Result<()> {
    let (config, _resolver, mut store) = open(explicit)?;
    match command {
        MemoryCommand::List => {
            for row in store.list_knowledge(Some("memory"))? {
                println!("{}\t{}", short_id(&row.id), row.content)
            }
        }
        MemoryCommand::Add { text } => {
            let added = knowledge::add_memory(&mut store, &config, &text).await?;
            events.success(if added {
                "记忆已保存"
            } else {
                "相同记忆已存在"
            })?
        }
        MemoryCommand::Search { query, limit } => {
            for hit in knowledge::search(&store, &config, &query, Some("memory"), limit).await? {
                println!("{:.3}\t{}\t{}", hit.score, short_id(&hit.id), hit.content)
            }
        }
        MemoryCommand::Delete { id } => {
            if !store.delete_knowledge(&id)? {
                bail!("记忆不存在：{id}")
            }
            events.success("记忆已删除")?
        }
    }
    Ok(())
}

async fn handle_knowledge(
    command: KnowledgeCommand,
    explicit: &Option<PathBuf>,
    events: &EventSink,
) -> Result<()> {
    let (config, _resolver, mut store) = open(explicit)?;
    match command {
        KnowledgeCommand::List => {
            for row in store.list_knowledge(None)? {
                println!("{}\t{}\t{}", short_id(&row.id), row.kind, row.title)
            }
        }
        KnowledgeCommand::Add { path } => {
            let count = knowledge::add_path(&mut store, &config, &path).await?;
            events.success(&format!("知识库新增 {count} 个文档"))?
        }
        KnowledgeCommand::Search { query, limit } => {
            for hit in knowledge::search(&store, &config, &query, None, limit).await? {
                println!(
                    "{:.3}\t{}\t{}\n{}",
                    hit.score,
                    short_id(&hit.id),
                    hit.title,
                    hit.content
                )
            }
        }
        KnowledgeCommand::Remove { id } => {
            if !store.delete_knowledge(&id)? {
                bail!("知识条目不存在：{id}")
            }
            events.success("知识条目已删除")?
        }
        KnowledgeCommand::Reindex => {
            events.success("flat 向量索引以 canonical BLOB 为准，无需重建")?
        }
    }
    Ok(())
}

fn doctor(explicit: &Option<PathBuf>, events: &EventSink) -> Result<()> {
    let (config, resolver, store) = open(explicit)?;
    println!("配置：{}", resolver.config_path().display());
    println!("数据库：{}", store.path().display());
    println!("模型：{}", config.primary_model()?.model);
    println!(
        "Embedding：{} ({} dimensions, {})",
        config.embeddings.model, config.embeddings.dimensions, config.embeddings.vector_encoding
    );
    println!("平台：{}/{}", std::env::consts::OS, std::env::consts::ARCH);
    println!(
        "Shell：{}",
        if config.permissions.allow_shell {
            "enabled"
        } else {
            "disabled"
        }
    );
    events.success("基础诊断通过")
}
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
