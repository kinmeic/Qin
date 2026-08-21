mod agent;
mod agents_md;
mod approval;
mod checkpoint;
mod cli;
mod config;
mod event;
mod knowledge;
mod markdown;
mod prompt_file;
mod state;
mod tools;
mod update;
mod wizard;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent::RunOptions;
use crate::cli::{Cli, Command, ConfigCommand, KnowledgeCommand, MemoryCommand};
use crate::config::{ConfigPathResolver, InitOptions};
use crate::event::EventSink;
use crate::state::StateStore;
use crate::update::UpdateOutcome;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {}", terminal(&event::redact(&format!("{error:#}"))));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse_normalized();
    let events = EventSink::new(cli.quiet, cli.json, cli.verbose);
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
                        let config = load_config(&resolver, &events)?;
                        println!("Scope: {}", terminal(resolver.scope().label()));
                        println!(
                            "Configuration: {}",
                            terminal(&resolver.config_path().display().to_string())
                        );
                        if config.persistence_enabled() {
                            println!(
                                "Database: {}",
                                terminal(&resolver.database_path(&config)?.display().to_string())
                            );
                        } else if config.storage.redis.enabled {
                            println!(
                                "Session store: Redis (key prefix: {})",
                                terminal(&config.storage.redis.key_prefix)
                            );
                        } else {
                            println!(
                                "Session store: {} (tmpfs, cleared on reboot)",
                                terminal(
                                    &crate::state::memory_state_path(&config)?
                                        .display()
                                        .to_string()
                                )
                            );
                        }
                    } else {
                        events.config_path(&resolver)?;
                    }
                }
                ConfigCommand::Check => {
                    let loaded = load_config(&resolver, &events)?;
                    loaded.validate(true)?;
                    events.success(&format!(
                        "Configuration is valid: {}",
                        resolver.config_path().display()
                    ))?;
                }
                ConfigCommand::Wizard { force } => {
                    wizard::run(&resolver, assume_yes || force, dry_run)?;
                }
            }
        }
        Command::Fromfile { path } => {
            execute_from_file(&path, &explicit_config, &events, assume_yes, dry_run).await?
        }
        Command::Replay { fixture } => {
            execute_replay_command(&fixture, &explicit_config, &events, assume_yes, dry_run).await?
        }
        Command::Run { prompt } => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                bail!("The prompt cannot be empty")
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
            let (config, resolver, mut store) = open(&explicit_config, &events)?;
            let cwd = std::env::current_dir()?;
            let title = prompt
                .first()
                .map(|_| prompt.join(" "))
                .map(|v| v.chars().take(60).collect::<String>());
            let id = store.new_session(&cwd, title.as_deref())?;
            events.success(&format!("Created and switched to a new session: {id}"))?;
            if !prompt.is_empty() {
                let agents_md = load_agents_md(&resolver, &config, &events);
                execute_with(
                    &config,
                    &mut store,
                    &id,
                    prompt.join(" "),
                    "cli",
                    None,
                    agents_md.as_deref(),
                    &events,
                    assume_yes,
                    dry_run,
                )
                .await?;
            }
            drop(resolver);
        }
        Command::Sessions => {
            let (_, _, store) = open(&explicit_config, &events)?;
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
                    terminal(&session.title)
                );
            }
        }
        Command::Use { session_id } => {
            let (_, _, mut store) = open(&explicit_config, &events)?;
            let id = store.use_session(&session_id)?;
            events.success(&format!("Active session: {id}"))?;
        }
        Command::Show { session_id } => {
            let (_, _, store) = open(&explicit_config, &events)?;
            let id_or_prefix = session_id
                .or(store.current_session()?)
                .context("There is no active session")?;
            let id = store.resolve_session_id(&id_or_prefix)?;
            for message in store.load_messages(&id)? {
                println!(
                    "[{}] {}",
                    terminal(&message.role),
                    terminal(&message.content.unwrap_or_else(|| "[tool_calls]".into()))
                );
            }
        }
        Command::Delete { session_id } => {
            let (_, _, mut store) = open(&explicit_config, &events)?;
            let id = store.resolve_session_id(&session_id)?;
            if dry_run {
                events.success(&format!("Dry run: session would be deleted: {id}"))?;
            } else {
                confirm_session_delete(&events, &id, assume_yes)?;
                let cwd = std::env::current_dir()?;
                let (_, new_current) = store.delete_session(&id, &cwd)?;
                let suffix = new_current.map_or_else(String::new, |active| {
                    format!("; created and switched to a new session: {active}")
                });
                events.success(&format!("Deleted session: {id}{suffix}"))?;
            }
        }
        Command::Memory { command } => handle_memory(command, &explicit_config, &events).await?,
        Command::Checkpoints => {
            let (config, _, store) = open(&explicit_config, &events)?;
            if !store.checkpoints_supported() {
                events.success(
                    "Checkpoints require the SQLite storage backend (storage.enabled = true)",
                )?;
            } else if !config.checkpoints.enabled {
                events.success("Checkpoints are disabled (checkpoints.enabled = false)")?;
            } else {
                let checkpoints = store.list_checkpoints(20)?;
                if checkpoints.is_empty() {
                    events.success("No checkpoints recorded yet")?;
                }
                for info in checkpoints {
                    let paths = info
                        .paths
                        .iter()
                        .map(|path| terminal(path))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "{}{} {}  {}  {}",
                        if info.restored { "(restored) " } else { "" },
                        short_id(&info.id),
                        info.created_at,
                        terminal(&info.tool),
                        paths
                    );
                }
            }
        }
        Command::Undo { checkpoint_id } => {
            let (_, _, store) = open(&explicit_config, &events)?;
            if !store.checkpoints_supported() {
                bail!("Checkpoints require the SQLite storage backend (storage.enabled = true)");
            }
            let id = match checkpoint_id {
                Some(value) => store.resolve_checkpoint_id(&value)?,
                None => store
                    .latest_checkpoint_id()?
                    .context("There are no checkpoints to undo")?,
            };
            if store.checkpoint_restored(&id)? {
                bail!("Checkpoint {} has already been restored", short_id(&id));
            }
            let steps = checkpoint::plan_undo(&store, &id)?;
            println!("Checkpoint {} will:", short_id(&id));
            for step in &steps {
                println!("  - {}", terminal(&step.description));
            }
            if dry_run {
                events.success("Dry run: no files were restored")?;
            } else {
                confirm_undo(&events, assume_yes)?;
                for outcome in checkpoint::execute_undo(&store, &id)? {
                    println!("{}", terminal(&outcome));
                }
                events.success(&format!("Restored checkpoint {}", short_id(&id)))?;
            }
        }
        Command::Knowledge { command } => {
            handle_knowledge(command, &explicit_config, &events).await?
        }
        Command::Sync => {
            let (config, _, mut store) = open(&explicit_config, &events)?;
            if !config.persistence_enabled() {
                events.success(match store.backend_label() {
                    "redis" => "Session state is stored in Redis and written immediately; nothing to synchronize",
                    _ => "Session state lives in a tmpfs file and is written immediately; nothing to synchronize",
                })?;
            } else {
                store.checkpoint()?;
                events.success(&format!(
                    "Database synchronized: {}",
                    store.path().display()
                ))?;
            }
        }
        Command::Doctor => doctor(&explicit_config, &events)?,
        Command::Update {
            internal_delegated,
            rollback,
        } => {
            if rollback {
                let outcome = update::rollback(dry_run, internal_delegated).await?;
                let message = match outcome {
                    update::RollbackOutcome::RolledBack { executable } => format!(
                        "qin was rolled back to the previous version; executable: {} (backup kept at {}; delete it when satisfied)",
                        executable.display(),
                        update::backup_path(&executable).display()
                    ),
                    update::RollbackOutcome::DryRun { executable } => format!(
                        "Dry run: qin would restore the previous executable over {}",
                        executable.display()
                    ),
                    update::RollbackOutcome::Delegated => return Ok(()),
                };
                events.success(&message)?;
                return Ok(());
            }
            let outcome = update::run(dry_run, internal_delegated).await?;
            let message = match outcome {
                UpdateOutcome::UpToDate {
                    current,
                    executable,
                } => format!(
                    "qin is already up to date (v{current}); executable: {}",
                    executable.display()
                ),
                UpdateOutcome::DryRun {
                    current,
                    latest,
                    executable,
                } => format!(
                    "Dry run: qin would update from v{current} to v{latest}; executable: {}",
                    executable.display()
                ),
                UpdateOutcome::Updated {
                    current,
                    latest,
                    executable,
                } => format!(
                    "qin updated from v{current} to v{latest}; executable: {}",
                    executable.display()
                ),
                UpdateOutcome::Delegated => return Ok(()),
            };
            events.success(&message)?;
        }
    }
    Ok(())
}

fn open(
    explicit: &Option<PathBuf>,
    events: &EventSink,
) -> Result<(config::Config, ConfigPathResolver, StateStore)> {
    let resolver = ConfigPathResolver::new(explicit.clone(), false)?;
    let config = load_config(&resolver, events)?;
    config.validate(false)?;
    let store = StateStore::open(&config, &resolver)?;
    if let Some(notice) = store.notice() {
        events.warning(notice)?;
    }
    Ok((config, resolver, store))
}

fn load_config(resolver: &ConfigPathResolver, events: &EventSink) -> Result<config::Config> {
    let loaded = config::load_with_warnings(resolver)?;
    events.configure(&loaded.config.ui);
    if loaded.unknown_field_count > 0 {
        let fields = loaded.unknown_fields.join(", ");
        let omitted = loaded
            .unknown_field_count
            .saturating_sub(loaded.unknown_fields.len());
        let suffix = if omitted == 0 {
            String::new()
        } else {
            format!(", and {omitted} more")
        };
        events.warning(&format!(
            "Ignoring unknown configuration field(s) in {}: {fields}{suffix}",
            resolver.config_path().display()
        ))?;
    }
    Ok(loaded.config)
}

async fn execute_from_file(
    path: &Path,
    explicit: &Option<PathBuf>,
    events: &EventSink,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    let (config, resolver, mut store) = open(explicit, events)?;
    events.tool_started("read_prompt_file", &format!("path={}", path.display()))?;
    let loaded = prompt_file::load(path, &config.input)
        .with_context(|| format!("Unable to load prompt file {}", path.display()))?;
    events.prompt_file_loaded(&loaded)?;
    let id = store.ensure_current_session(&std::env::current_dir()?)?;
    let agents_md = load_agents_md(&resolver, &config, events);
    execute_with(
        &config,
        &mut store,
        &id,
        loaded.content,
        "file",
        Some(loaded.canonical_path),
        agents_md.as_deref(),
        events,
        yes,
        dry_run,
    )
    .await
}

async fn execute_replay_command(
    fixture: &Path,
    explicit: &Option<PathBuf>,
    events: &EventSink,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    agent::load_replay_fixture(fixture)?;
    let (config, _resolver, mut store) = open(explicit, events)?;
    // The lightweight backends hold a single session: starting the replay
    // session replaces whatever conversation is currently active.
    if store.backend_label() != "sqlite"
        && let Some(current) = store.current_session()?
        && !store.load_messages(&current)?.is_empty()
    {
        events.warning(
            "Replay starts a fresh session; with the lightweight session backend this replaces the current session",
        )?;
    }
    let cwd = std::env::current_dir()?;
    let session = store.new_session(&cwd, Some("Replay"))?;
    let report =
        agent::execute_replay(&config, &mut store, &session, fixture, events, yes, dry_run).await?;
    events.final_answer(&report.answer)
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
    let (config, resolver, mut store) = open(explicit, events)?;
    let id = store.ensure_current_session(&std::env::current_dir()?)?;
    let agents_md = load_agents_md(&resolver, &config, events);
    execute_with(
        &config,
        &mut store,
        &id,
        prompt,
        source,
        path,
        agents_md.as_deref(),
        events,
        yes,
        dry_run,
    )
    .await
}

fn load_agents_md(
    resolver: &ConfigPathResolver,
    config: &config::Config,
    events: &EventSink,
) -> Option<String> {
    match agents_md::load(resolver.config_path(), config.input.agents_md_max_bytes) {
        Ok(Some(loaded)) => Some(loaded.content),
        Ok(None) => None,
        Err(error) => {
            let _ = events.warning(&format!(
                "Ignoring AGENTS.md: {}",
                event::redact(&format!("{error:#}"))
            ));
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_with(
    config: &config::Config,
    store: &mut StateStore,
    id: &str,
    prompt: String,
    source: &str,
    path: Option<PathBuf>,
    agents_md: Option<&str>,
    events: &EventSink,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    let _session_lock = store.lock_session(id)?;
    let recovered = store.recover_session(id)?;
    if recovered > 0 {
        events.warning(&format!(
            "Recovered {recovered} unfinished event(s) from the previous qin process; external tool state was not retried automatically"
        ))?;
    }
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
            agents_md,
        },
    )
    .await;
    store.validate_session(id)?;
    let response = response?;
    events.final_answer(&response)
}

async fn handle_memory(
    command: MemoryCommand,
    explicit: &Option<PathBuf>,
    events: &EventSink,
) -> Result<()> {
    let (config, _resolver, mut store) = open(explicit, events)?;
    match command {
        MemoryCommand::List => {
            for row in store.list_knowledge(Some("memory"))? {
                println!("{}\t{}", short_id(&row.id), terminal(&row.content))
            }
        }
        MemoryCommand::Add { text } => {
            let added = knowledge::add_memory(&mut store, &config, &text).await?;
            events.success(if added {
                "Memory saved"
            } else {
                "An identical memory already exists"
            })?
        }
        MemoryCommand::Search { query, limit } => {
            for hit in knowledge::search(&store, &config, &query, Some("memory"), limit).await? {
                println!(
                    "{:.3}\t{}\t{}",
                    hit.score,
                    short_id(&hit.id),
                    terminal(&hit.content)
                )
            }
        }
        MemoryCommand::Delete { id } => {
            if !store.delete_knowledge(&id)? {
                bail!("Memory does not exist: {id}")
            }
            events.success("Memory deleted")?
        }
    }
    Ok(())
}

async fn handle_knowledge(
    command: KnowledgeCommand,
    explicit: &Option<PathBuf>,
    events: &EventSink,
) -> Result<()> {
    let (config, _resolver, mut store) = open(explicit, events)?;
    match command {
        KnowledgeCommand::List => {
            for row in store.list_knowledge(None)? {
                println!(
                    "{}\t{}\t{}",
                    short_id(&row.id),
                    terminal(&row.kind),
                    terminal(&row.title)
                )
            }
        }
        KnowledgeCommand::Add { path } => {
            let count = knowledge::add_path(&mut store, &config, &path).await?;
            events.success(&format!("Added {count} document(s) to the knowledge base"))?
        }
        KnowledgeCommand::Search { query, limit } => {
            for hit in knowledge::search(&store, &config, &query, None, limit).await? {
                println!(
                    "{:.3}\t{}\t{}\n{}",
                    hit.score,
                    short_id(&hit.id),
                    terminal(&hit.title),
                    terminal(&hit.content)
                )
            }
        }
        KnowledgeCommand::Remove { id } => {
            if !store.delete_knowledge(&id)? {
                bail!("Knowledge entry does not exist: {id}")
            }
            events.success("Knowledge entry deleted")?
        }
        KnowledgeCommand::Reindex => events.success(
            "The flat vector index uses canonical BLOB data and does not need rebuilding",
        )?,
    }
    Ok(())
}

fn doctor(explicit: &Option<PathBuf>, events: &EventSink) -> Result<()> {
    let (config, resolver, store) = open(explicit, events)?;
    println!(
        "Configuration: {}",
        terminal(&resolver.config_path().display().to_string())
    );
    println!(
        "Database: {}",
        if config.persistence_enabled() {
            terminal(&store.path().display().to_string())
        } else if store.backend_label() == "redis" {
            terminal("Redis session backend")
        } else {
            terminal(&format!("{} (tmpfs session file)", store.path().display()))
        }
    );
    println!("Session backend: {}", terminal(store.backend_label()));
    println!("Model: {}", terminal(&config.primary_model()?.model));
    match agents_md::load(resolver.config_path(), config.input.agents_md_max_bytes) {
        Ok(Some(loaded)) => println!(
            "Project instructions: {} ({} bytes)",
            terminal(&loaded.path.display().to_string()),
            loaded.byte_len
        ),
        Ok(None) => println!(
            "Project instructions: none (create {} beside the configuration file to enable)",
            agents_md::AGENTS_MD_FILE_NAME
        ),
        Err(error) => println!(
            "Project instructions: {}",
            terminal(&format!(
                "unusable ({})",
                event::redact(&format!("{error:#}"))
            ))
        ),
    }
    if config.embeddings_active() {
        println!(
            "Embedding: {} ({} dimensions, {})",
            terminal(&config.embeddings.model),
            config.embeddings.dimensions,
            terminal(&config.embeddings.vector_encoding)
        );
    } else {
        println!("Embedding: disabled");
    }
    println!(
        "Platform: {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "Shell: {}",
        if config.permissions.allow_shell {
            "enabled"
        } else {
            "disabled"
        }
    );
    events.success("Basic diagnostics passed")
}
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn terminal(value: &str) -> String {
    event::sanitize_terminal(value)
}

fn confirm_undo(events: &EventSink, assume_yes: bool) -> Result<()> {
    if assume_yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("Restoring a checkpoint requires an interactive confirmation or --yes");
    }
    events.approval_prompt("Restore these files now? [y/N] ")?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("The checkpoint restore was declined by the user")
    }
}

fn confirm_session_delete(events: &EventSink, id: &str, assume_yes: bool) -> Result<()> {
    if assume_yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("Session deletion requires an interactive confirmation or --yes");
    }
    let message = format!("Permanently delete session {id} and all of its history? [y/N] ");
    events.approval_prompt(&message)?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("Session deletion was declined by the user")
    }
}
