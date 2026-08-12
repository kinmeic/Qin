# Code audit

This repository was audited for correctness, security, and performance in August 2026. The audit covered configuration loading, model and search HTTP clients, the agent loop, context compression, local tools, privilege elevation, SQLite persistence, session locking, knowledge ingestion, embedding search, terminal output, installation, packaging, and CI.

## Fixed findings

### Correctness

- Persisted context compaction now records the exact message sequence covered by a summary. Full history remains available for `qin show`, while subsequent model requests load only active history after the compaction boundary.
- Agent turns, tool audits, and compaction metadata are committed together. Partial turns are retained when a model request, tool call, timeout, or cancellation fails.
- Streaming responses stop at `[DONE]`, validate tool calls, and enforce output limits across text and tool-call arguments.
- Total wall time now covers model calls and tool execution, and cancellation no longer allows the agent to continue after an interrupted command.
- Models can explicitly disable tool schemas with `supports_tools = false`.
- Invalid or unsupported configuration values are rejected instead of being silently ignored.

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
- The configured model provider receives prompts, selected history, runtime context, tool output, and recalled knowledge needed for a task.

For higher-assurance environments, combine `qin` with a restricted OS account, a container or VM, read-only mounts, network egress controls, and encrypted storage.
