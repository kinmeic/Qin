# Code audit

This repository was audited for correctness, security, and performance in August 2026. The audit covered configuration loading and the interactive wizard, model and search HTTP clients, the agent loop, context compression, local tools and approval classification, privilege elevation, SQLite/Redis/tmpfs persistence, session locking, runtime host context, self-update, knowledge ingestion, embedding search, terminal output, installation, packaging, dependencies, and CI.

## Fixed findings

### Correctness

- Persisted context compaction now records the exact message sequence covered by a summary. Full history remains available for `qin show`, while subsequent model requests load only active history after the compaction boundary.
- Agent turns, tool audits, and compaction metadata are committed together. Partial turns are retained when a model request, tool call, timeout, or cancellation fails.
- Streaming responses stop at `[DONE]`, validate tool calls, and enforce output limits across text and tool-call arguments.
- Total wall time now covers model calls and tool execution, and cancellation no longer allows the agent to continue after an interrupted command.
- Models can explicitly disable tool schemas with `supports_tools = false`.
- Invalid known configuration values are rejected; unknown fields are reported as warnings and ignored for forward compatibility.
- Redis recovery now reconciles the remote session with a newer outage-time JSON session, including same-second message sequence changes, and removes the obsolete JSON file after migration.
- The configuration wizard honors `--dry-run`, preserves existing inline/legacy API keys unless explicitly replaced, and reports failed backup restoration instead of hiding it.
- Runtime distribution, platform, and kernel fields are optional and platform-specific; unavailable values are omitted rather than fabricated.

### Security and privacy

- Configuration and database paths reject symbolic links; private file permissions are enforced and checked.
- File paths are normalized through existing parents. Access outside the current working directory requires an explicit confirmation that `--yes` cannot bypass.
- File writes use no-follow file descriptors on Unix, and unsafe directory replacement during moves is rejected.
- Shell child processes no longer inherit configured model, embedding, or search API-key environment variables.
- HTTP redirects are disabled for authenticated model, embedding, and search requests to prevent credentials from crossing redirect trust boundaries.
- Model, embedding, search, command, and audit outputs have bounded memory and persistence sizes.
- Command stdout and stderr use a bounded channel, terminal control characters are filtered, and live command output honors its configured byte limit.
- Session lock names are hashed and private, and an advisory operating-system lock avoids stale-file ownership races without persistent flash writes.
- Destructive command detection was expanded, and path operations outside the workspace are always treated as high risk.
- `on_risk` shell auto-approval rejects multiline/comment ambiguity, untrusted executable paths, and side-effecting options such as `date --set`, `find -fprint`, `rg --pre`, `sort -o`, and `ss --kill`.
- Common service, journal, kernel, network, firewall, and package queries use command-specific read-only rules; batch files, generated caches, helper executables, custom package-manager configuration, and mutating subcommands remain approval-gated.
- Read-only shell auto-approval resolves bare executables through `PATH` and trusts only system executable directories; shell startup, exported-function, and dynamic-loader injection variables are removed before execution.
- Non-overwriting file/directory creation inside the workspace is treated as reversible, while overwrite, move, and external-path operations remain approval-gated.
- Catastrophic recursive deletion, raw-device destruction, fork bombs, and kill-all commands are classified as forbidden and refused rather than offered for approval.
- Task-scoped `All` approval is explicit, limited to subsequent shell commands in the current run, and cannot bypass Forbidden rules or structured-tool external-path checks.
- Interactive privilege prompts no longer share a transient heartbeat line; the child process group temporarily receives foreground terminal control, heartbeats pause while sudo owns the terminal, stdin is inherited explicitly, and guards restore qin's foreground group and terminal echo after normal exit, cancellation, or timeout.
- tmpfs session state rejects symlinks, non-private ownership/modes, malformed data, unsupported versions, and values larger than 128 MiB; writes use private same-directory temporary files and atomic replacement.
- Redis supports certificate-verified TLS, uses bounded connect/read/write operations, rejects malformed or wrongly typed state instead of silently overwriting it, and removes secret Redis URL environment variables from shell children.
- Runtime-context values and remote updater error summaries are escaped/sanitized before display or model delivery.
- Self-update accepts only bounded GitHub release assets for the current platform, verifies SHA-256, rejects unsafe archive paths, and atomically replaces the executable.
- The RustSec advisory database reports no known vulnerability in the locked dependency graph at the time of this audit.

### Performance and OpenWrt

- Embedding requests honor `embeddings.batch_size`.
- Duplicate knowledge content is checked before embedding, and document batches are persisted in one SQLite transaction.
- Vector search no longer loads the full source document once per chunk.
- Command output is streamed through bounded chunks instead of an unbounded line queue.
- SQLite journal mode is changed only when necessary. OpenWrt continues to default to PERSIST, f16 vectors, delayed automatic memory extraction, and batched writes.
- OpenWrt system state defaults to `/etc/qin/qin.db` instead of frequently volatile `/var`; external durable storage remains configurable.

## Residual risks

- Shell command classification is heuristic. An approved shell can perform anything available to the current operating-system account.
- File-system path checks reduce common symlink attacks but cannot provide the guarantees of a kernel sandbox against a hostile process that races path changes.
- Flat cosine search streams rows with bounded result memory, but remains CPU- and I/O-heavy for very large knowledge bases.
- Session data and knowledge are protected by file permissions, not encrypted at rest.
- Redis session locking is local to one host. Deployments that share one Redis database across machines must use a unique `storage.redis.key_prefix` per qin installation and must not concurrently write the same key.
- The configured model provider receives prompts, selected history, runtime context, tool output, and recalled knowledge needed for a task.

For higher-assurance environments, combine `qin` with a restricted OS account, a container or VM, read-only mounts, network egress controls, and encrypted storage.
