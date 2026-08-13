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

### Prebuilt releases

Tagged releases publish checksummed archives for Linux (`x86_64`, `arm64`), macOS (`x86_64`, Apple Silicon `arm64`), and OpenWrt (`x86_64`, `aarch64_cortex-a53`). OpenWrt releases include native OpenWrt 25.12.5 `apk` v3 packages and legacy `.ipk` packages.

OpenWrt 25.12.5:

```sh
apk add --allow-untrusted ./qin-VERSION-r1_openwrt-25.12.5_aarch64_cortex-a53.apk
# or, on x86_64:
apk add --allow-untrusted ./qin-VERSION-r1_openwrt-25.12.5_x86_64.apk
```

OpenWrt 24.10 or earlier:

```sh
opkg install qin_VERSION-1_aarch64_cortex-a53.ipk
# or
opkg install qin_VERSION-1_x86_64.ipk
```

The `apk` files are produced by the official OpenWrt 25.12.5 SDKs for `mvebu/cortexa53` and `x86/64`; the embedded package architectures are `aarch64_cortex-a53` and `x86_64`. After installing, copy or rename `/etc/qin/config.toml.example` to `/etc/qin/config.toml`, edit it, and run `qin config check`.

## Configuration

Configuration and persistent state use platform-appropriate directories by default:

- Linux user configuration: `${XDG_CONFIG_HOME:-~/.config}/qin/config.toml`
- Linux user data: `${XDG_DATA_HOME:-~/.local/share}/qin/`
- Linux/OpenWrt system configuration: `/etc/qin/config.toml`
- Linux system data: `/var/lib/qin/`
- OpenWrt system data: `/etc/qin/qin.db` by default, so it survives systems where `/var` is tmpfs; set `storage.data_dir` to durable external storage when available
- macOS user configuration and data: platform application-support directories for `qin`

Run `qin config path` and `qin doctor` to see the exact active paths. Use `qin init --system` when you intentionally want a system-wide configuration. An explicitly selected configuration stores its database beside that configuration unless `storage.data_dir` overrides the location.

### Optional Redis session storage

When SQLite is disabled, qin normally keeps the current session in a tmpfs JSON file. If a Redis server is available, qin can store that lightweight session directly in Redis instead:

```toml
[storage]
enabled = false

[storage.redis]
enabled = true
url = "redis://127.0.0.1:6379/0"
key_prefix = "qin"
connect_timeout_ms = 1000
```

For a password or a remote Redis server, keep the URL out of the config file:

```toml
[storage.redis]
enabled = true
url_env = "QIN_REDIS_URL"
key_prefix = "qin"
```

```bash
export QIN_REDIS_URL='redis://:password@127.0.0.1:6379/0'
```

qin verifies the URL and sends `PING` during startup. Both `redis://` and certificate-verified `rediss://` connections are supported. When Redis is reachable, no `qin-session.json` is written. If Redis is unavailable, qin falls back to the tmpfs JSON session store and prints the reason in a warning. Redis session storage is intentionally only used when `storage.enabled = false`; enabling SQLite and Redis together is rejected to avoid ambiguous storage semantics. Long-term memory and embeddings still require SQLite storage to be enabled.

The session contains conversation content, so use a private Redis instance with authentication/ACLs and TLS or a protected local network as appropriate. When Redis becomes available, qin compares any outage-time JSON session with the Redis session, migrates the newer state, and removes the obsolete JSON file after Redis accepts it. Invalid or unsupported session data in Redis is reported as an error instead of silently replacing it with a fallback.

Redis locking is local to the machine running qin. If several machines use the same Redis database, configure a unique `key_prefix` for each qin installation; do not use one session key as a concurrent multi-host store.

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
supports_native_search = false

[context]
compact_trigger_ratio = 0.9
```

Keep API keys in environment variables whenever possible instead of storing secrets directly in the configuration file.

Search backends are attempted in the configured order. Disabled providers are skipped, so the following example tries Exa first and Brave second:

```toml
[search]
order = ["exa", "brave", "native"]
max_results = 8
timeout_seconds = 15

[search.exa]
enabled = true
api_key_env = "EXA_API_KEY"

[search.brave]
enabled = true
api_key_env = "BRAVE_API_KEY"

[search.native]
enabled = false
model = "primary"
```

Model-native search is the final fallback. Enable it only when the selected OpenAI-compatible provider supports the Responses API web-search tool, and also set `supports_native_search = true` on that model. Exa and Brave keys should be exported as `EXA_API_KEY` and `BRAVE_API_KEY` respectively.

## Usage

### Help and version

```bash
qin --help
qin <COMMAND> --help
qin --version
```

`qin --help` lists every supported top-level command and global option. Subcommand help shows command-specific arguments.

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
qin config wizard
qin doctor
qin sync
qin update
```

`qin sync` commits pending audit records, checkpoints WAL databases, and asks SQLite to flush cached pages.
`qin update` checks the latest GitHub release for the current platform, verifies its SHA-256 checksum, and atomically replaces the running executable when a newer version is available. Use `sudo qin update` when qin is installed in a directory that requires administrator permissions; `qin update --dry-run` only checks and reports what would change.
`qin config wizard` walks through the model connection, optional Redis/SQLite storage, and safety settings. Existing files are backed up before replacement; use `qin config wizard --force` or the global `--yes` flag to skip the final replacement confirmation. `--dry-run` performs the full review and validation without writing the file. API keys and Redis URLs can be supplied through environment variables, and the wizard never asks you to paste an API key into the config file; an existing inline API key is preserved unless you explicitly replace it with an environment-variable reference.

For forward compatibility, unknown fields and configuration sections produce a warning and are ignored instead of preventing qin from starting. Known fields with invalid types or values remain errors. An ignored setting does not take effect; use `qin config check` and review its warnings after moving a configuration file between qin versions.

## How a task runs

For every model request, `qin` supplies a bounded snapshot of the execution environment, including the current time, time zone, operating system, architecture, current directory, privilege state, and active policy. Relevant session history, memories, and knowledge results are added within the configured context budget.

The model can request local tools for directory listing, file inspection, reading, writing, moving, copying, deletion, exact text replacement, shell execution, memory, and web search. Tool output is treated as untrusted data, size-limited, and never promoted to system instructions.

## Safety model

`qin` reduces accidental damage through path validation, risk classification, approvals, command redaction, timeouts, and conservative defaults. With `permissions.approval = "on_risk"`, read-only tools and recognized read-only shell commands such as `date`, `pwd`, and `find ... -print` run without an approval prompt. Writes, unknown shell commands, external-path access, destructive commands, and privilege elevation remain subject to approval. Use `--dry-run` to allow planning and read-only inspection without performing writes or commands. `--yes` can approve ordinary mutations, but it does not bypass confirmation for extremely high-risk actions.

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
