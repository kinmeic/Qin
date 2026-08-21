use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "qin",
    version,
    about = "A command-line AI agent that exits after each task",
    long_about = None
)]
pub struct Cli {
    /// Use a specific configuration file
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Emit events as JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Hide routine progress messages
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Stream live command stdout/stderr lines (hidden by default)
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Approve ordinary writes and commands; extremely high-risk actions still require confirmation
    #[arg(long, global = true)]
    pub yes: bool,

    /// Plan and read only; do not perform writes or run commands
    #[arg(long, global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a qin configuration file and display its path
    Init {
        /// Create a system-wide configuration
        #[arg(long, conflicts_with = "config")]
        system: bool,

        /// Back up and replace an existing configuration
        #[arg(long)]
        force: bool,

        /// Open the file in an editor after creation
        #[arg(long)]
        edit: bool,
    },

    /// Read a UTF-8 text file and use its contents as the prompt
    Fromfile {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },

    /// Replay a JSONL fixture through the real tool and persistence pipeline
    Replay {
        #[arg(value_name = "PATH")]
        fixture: PathBuf,
    },

    /// Create and switch to a new session, optionally with an initial prompt
    New { prompt: Vec<String> },

    /// List recent sessions
    Sessions,

    /// Switch the active session
    Use { session_id: String },

    /// Show the active or specified session
    Show { session_id: Option<String> },

    /// Permanently delete a session and all of its stored history
    Delete { session_id: String },

    /// List recent file checkpoints recorded before tool mutations
    Checkpoints,

    /// Restore the files captured by a checkpoint (defaults to the latest)
    Undo { checkpoint_id: Option<String> },

    /// Manage long-term memory
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },

    /// Manage the document knowledge base
    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommand,
    },

    /// Commit pending audits, checkpoint WAL, and flush SQLite cached pages
    Sync,

    /// Check configuration, database, and platform capabilities
    Doctor,

    /// Check the latest GitHub release and replace the running qin executable when newer
    Update {
        /// Restore the executable backup saved by the previous update
        #[arg(long)]
        rollback: bool,

        /// Prevent recursive privilege delegation in the internal updater subprocess
        #[arg(long, hide = true)]
        internal_delegated: bool,
    },

    /// View or validate configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    #[command(hide = true)]
    Run {
        // No trailing_var_arg/allow_hyphen_values: global flags such as --yes
        // placed after the prompt must still parse as flags. Prompts that
        // start with a hyphen can be passed after `--`.
        #[arg(required = true)]
        prompt: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Display the active configuration path
    Path,
    /// Validate configuration and secret references
    Check,
    /// Interactively create or update configuration with safe defaults
    Wizard {
        /// Replace an existing configuration without asking for confirmation
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    List,
    Add {
        text: String,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum KnowledgeCommand {
    List,
    Add {
        path: PathBuf,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    Remove {
        id: String,
    },
    Reindex,
}

impl Cli {
    pub fn parse_normalized() -> Self {
        Self::parse_from(normalize_args(std::env::args_os().collect()))
    }
}

fn normalize_args(mut args: Vec<OsString>) -> Vec<OsString> {
    if args.len() <= 1 {
        return args;
    }

    let known = [
        "init",
        "fromfile",
        "replay",
        "new",
        "sessions",
        "use",
        "show",
        "delete",
        "checkpoints",
        "undo",
        "memory",
        "knowledge",
        "sync",
        "doctor",
        "update",
        "config",
        "run",
        "help",
    ];
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_string_lossy();
        if value == "--config" {
            index += 2;
            continue;
        }
        if value == "--" {
            args.remove(index);
            if index < args.len() {
                args.insert(index, OsString::from("run"));
            }
            return args;
        }
        if value.starts_with("--config=")
            || value == "--json"
            || value == "--quiet"
            || value == "-q"
            || value == "--verbose"
            || value == "-v"
            || value == "--yes"
            || value == "--dry-run"
        {
            index += 1;
            continue;
        }
        if value == "--help" || value == "-h" || value == "--version" || value == "-V" {
            return args;
        }
        if known.contains(&value.as_ref()) || value.starts_with('-') {
            return args;
        }
        args.insert(index, OsString::from("run"));
        return args;
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn inserts_run_before_bare_prompt() {
        assert_eq!(
            normalize_args(strings(&["qin", "inspect this directory"])),
            strings(&["qin", "run", "inspect this directory"])
        );
    }

    #[test]
    fn keeps_known_subcommand() {
        assert_eq!(
            normalize_args(strings(&["qin", "fromfile", "prompt.md"])),
            strings(&["qin", "fromfile", "prompt.md"])
        );
    }

    #[test]
    fn recognizes_update_subcommand() {
        let cli = Cli::try_parse_from(strings(&["qin", "update"])).expect("should parse");
        assert!(matches!(
            cli.command,
            Command::Update {
                rollback: false,
                internal_delegated: false
            }
        ));
    }

    #[test]
    fn skips_global_config_option() {
        assert_eq!(
            normalize_args(strings(&["qin", "--config", "x.toml", "hello"])),
            strings(&["qin", "--config", "x.toml", "run", "hello"])
        );
    }

    #[test]
    fn supports_hyphen_prefixed_prompts_after_separator() {
        assert_eq!(
            normalize_args(strings(&["qin", "--", "- inspect this directory"])),
            strings(&["qin", "run", "- inspect this directory"])
        );
    }

    #[test]
    fn global_flags_after_prompt_are_not_swallowed() {
        let args = normalize_args(strings(&["qin", "list services", "--yes"]));
        let cli = Cli::try_parse_from(args).expect("should parse");
        assert!(cli.yes);
        match cli.command {
            Command::Run { prompt } => assert_eq!(prompt, vec!["list services".to_string()]),
            _ => panic!("expected the run command"),
        }
    }

    #[test]
    fn new_prompt_stops_at_flags() {
        let cli = Cli::try_parse_from(strings(&["qin", "new", "hello world", "--quiet"]))
            .expect("should parse");
        assert!(cli.quiet);
        match cli.command {
            Command::New { prompt } => assert_eq!(prompt, vec!["hello world".to_string()]),
            _ => panic!("expected the new command"),
        }
    }
}
