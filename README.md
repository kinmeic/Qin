# qin

> Use natural language to operate Linux, macOS, and OpenWrt from the command line—without memorizing commands or flags.

`qin` is a Rust-powered command-line AI agent that turns plain-language requests into safe, visible actions on your computer. Ask it to inspect files, reorganize directories, edit text, run commands, search the web, or use project knowledge; `qin` plans the work, shows each tool call and command, asks for approval when needed, and returns control to your shell when the task is complete.

```console
$ qin "Move every file from ./incoming into the current directory"
$ qin "Find the five largest files here and explain what they are"
$ qin fromfile ./task.md
```

## Why qin?

Shells are powerful, but their commands and flags are easy to forget. `qin` lets you describe the outcome you want while keeping the execution local, observable, and subject to explicit safety rules.

- **Natural-language operations** — work with files and commands without recalling exact syntax.
- **Cross-platform CLI** — designed for Linux, macOS, and resource-constrained OpenWrt systems.
- **Visible execution** — tool calls, commands, progress, stdout, stderr, exit codes, and timeouts are shown as they happen.
- **Approval-aware safety** — writes and commands are risk-checked; highly destructive operations always require confirmation.
- **Persistent sessions** — each invocation exits normally, while conversation history remains available for the next invocation.
- **Knowledge and memory** — document ingestion, embeddings, and cosine vector search provide project-specific context.
- **Search fallback** — Exa is tried first, then Brave, followed by model-native search when supported.
- **OpenAI-compatible models** — configure your own base URL, model, API key, context window, and compression thresholds.
- **OpenWrt-friendly persistence** — batched writes and a persistent journal reduce frequent SQLite writes on flash storage.

## Quick start

### Build from source

Rust 1.85 or later is required.

```bash
cargo build --release
./target/release/qin init
```

`qin init` creates a configuration file in the platform-appropriate location and prints its absolute path. Edit that file, then provide the API key through the environment variable referenced by `api_key_env`.

```bash
export QIN_API_KEY="your-api-key"
./target/release/qin config check
./target/release/qin "Summarize this project"
```

To install the release binary and supporting files:

```bash
./scripts/install.sh
```

OpenWrt devices vary by CPU architecture, libc, and ABI. Cross-compile for the exact device target first, then use the files under `packaging/openwrt` to build an `opkg` package.

## Configuration

Configuration and persistent state use platform-appropriate directories by default:

- Linux user configuration: `${XDG_CONFIG_HOME:-~/.config}/qin/config.toml`
- Linux user data: `${XDG_DATA_HOME:-~/.local/share}/qin/`
- Linux/OpenWrt system configuration: `/etc/qin/config.toml`
- Linux system data: `/var/lib/qin/`
- OpenWrt system data: `/etc/qin/qin.db` by default, so it survives systems where `/var` is tmpfs; set `storage.data_dir` to durable external storage when available
- macOS user configuration and data: platform application-support directories for `qin`

Run `qin config path` and `qin doctor` to see the exact active paths. Use `qin init --system` when you intentionally want a system-wide configuration. An explicitly selected configuration stores its database beside that configuration unless `storage.data_dir` overrides the location.

A generated configuration includes documented defaults. The essential model settings look like this:

```toml
version = 1
default_model = "primary"

[models.primary]
base_url = "https://api.openai.com/v1"
model = "gpt-4.1-mini"
api_key_env = "QIN_API_KEY"
context_window = 128000
max_output_tokens = 4096

[context]
compact_trigger_ratio = 0.72
compact_target_ratio = 0.45
```

Keep API keys in environment variables whenever possible instead of storing secrets directly in the configuration file.

## Usage

### Run a task

```bash
qin "Create an archive directory and move all .log files older than 30 days into it"
```

Each ordinary invocation continues the active session, performs the requested work, persists the conversation, and exits.

### Start a new session

```bash
qin new
qin new "Review this repository for unfinished work"
```

Manage saved sessions with:

```bash
qin sessions
qin show
qin use <SESSION_ID>
qin delete <SESSION_ID>
```

Session IDs may be given in full or by the unique short prefix displayed by `qin sessions`. Deletion permanently removes the session history and tool audit records, asks for confirmation, and accepts `--yes` for non-interactive use. If the active session is deleted, `qin` creates and switches to a brand-new empty session.

### Read a prompt from a file

```bash
qin fromfile ./task.md
```

The file must be a non-empty UTF-8 text file and must fit within the configured input-size limit. Its contents become the user prompt for the current task.

### Use administrator privileges

Run `sudo qin ...` when the entire agent genuinely needs administrator access. For individual elevated commands, `qin` can use the configured `sudo` or `doas` flow and will display the command before requesting approval.

### Knowledge and long-term memory

```bash
qin knowledge add ./docs
qin knowledge search "How is configuration resolved?"
qin memory list
```

Documents are chunked, embedded, and stored with canonical vectors. Search combines semantic similarity with text signals. Long-term memory is intended for stable preferences, project facts, important decisions, and reusable procedures—not transient command output.

### Diagnostics and maintenance

```bash
qin config path
qin config check
qin doctor
qin sync
```

`qin sync` commits pending audit records, checkpoints WAL databases, and asks SQLite to flush cached pages.

## How a task runs

For every model request, `qin` supplies a bounded snapshot of the execution environment, including the current time, time zone, operating system, architecture, current directory, privilege state, and active policy. Relevant session history, memories, and knowledge results are added within the configured context budget.

The model can request local tools for directory listing, file inspection, reading, writing, moving, copying, deletion, exact text replacement, shell execution, memory, and web search. Tool output is treated as untrusted data, size-limited, and never promoted to system instructions.

## Safety model

`qin` reduces accidental damage through path validation, risk classification, approvals, command redaction, timeouts, and conservative defaults. Use `--dry-run` to allow planning and read-only inspection without performing writes or commands. `--yes` can approve ordinary mutations, but it does not bypass confirmation for extremely high-risk actions.

These controls are guardrails, not a complete operating-system sandbox. Review displayed commands, use normal user privileges by default, protect your configuration and database, and keep backups of important data.

See [SECURITY.md](SECURITY.md) for vulnerability reporting and deployment guidance. The latest correctness, security, and performance review is summarized in [AUDIT.md](AUDIT.md).

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The detailed architecture, permission model, session database, vector search design, and low-write OpenWrt strategy are documented in [QIN_DESIGN.md](QIN_DESIGN.md).

## License

Licensed under the Apache License 2.0.
