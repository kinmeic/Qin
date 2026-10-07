# Changelog

All notable changes to `qin` are documented here.

## 0.6.5

- Matched the TUI message input to the compact prompt style: `#313131` background, `#7D8185` placeholder, and `#C8D1D9` input text; added top/bottom padding and a leading `> ` prompt with text beginning in column three.
- Started with one editable content row and grew the input for newlines and wrapping, retaining scrolling for long messages.
- Replaced the session ID beside qin with the running package version, and changed MEM to available / total memory instead of utilization percentage.

## 0.6.4

- Reduced the input footer to two rows: approval mode, model name without a label, compact context and total turn usage, and status share one line; shortcuts occupy the last row. Long model names and statuses are shortened to fit.
- Removed conversation and message borders and gave the input a light-gray background with dark text.
- Added CPU utilization, memory utilization/capacity, and working-directory filesystem disk usage/capacity to the title bar, sampled off the UI thread on Linux/OpenWrt and macOS. Missing readings display `--`.
- Added regression coverage for footer layout, long model names, title metrics, native sampling, CPU counter resets, Linux guest accounting, and legacy memory statistics.

## 0.6.3

- Fixed TUI terminal handoff so interactive applications receive terminal stdout and stderr with their control sequences intact. Their output is displayed directly; ordinary commands remain captured in the conversation. Git global options such as `-C` and `-c` no longer hide commands that need a terminal.
- Rejected incomplete non-streaming model responses before any tool execution, matching streaming response validation.
- Fixed secret redaction when a value starts with `[REDACTED]` but appends a secret; repeated redaction remains stable.
- Applied cancellation and the agent deadline while waiting for TUI approval, rejected late answers, and dismissed expired approval dialogs when the turn ends.
- Bounded the TUI event queue and event drain to keep streaming output from exhausting memory or starving keyboard input.
- Preserved captured stdout/stderr across arbitrary read chunks, bounded sparse-table alignment expansion, and avoided duplicate truncated streamed answers. Restored history also aligns tables.
- Added ten regression tests and a local-model pseudo-terminal smoke test covering terminal stdio, ANSI output, cancellation, approval deadlines, result pairing, and shutdown; runs on Linux and macOS in CI.
- Documented the follow-up audit in `docs/code-audit-2026-10-07.md`.

## 0.6.2

- Kept streamed shell output in a single TUI block so line and table alignment survives arbitrary read chunk boundaries; expanded tab stops when rendering output.
- Aligned Markdown pipe tables in completed TUI answers by terminal display width, including wide CJK characters, while preserving fenced code blocks.

## 0.6.1

- Kept ordinary shell commands in the TUI, displaying their streamed output in the conversation instead of handing the terminal back to the shell. The TUI gives stdin to `/dev/null` for these commands so they cannot steal prompt input.
- Retained terminal handoff for sudo, SSH, interactive shells and editors, Git operations that may prompt, package installs needing confirmation, and other recognized interactive commands. Added the shell tool's `interactive=true` option for commands that require terminal input but cannot be detected automatically.

## 0.6.0

- Added `qin tui`, a persistent terminal chat interface with a branded header, restored active-session messages, streaming replies, tool activity, approval dialogs, and terminal handoff for interactive shell commands.
- Added multiline input with Ctrl+J and Shift+Enter where supported, bracketed paste, prompt history, conversation scrolling, cancellation, and `/new`, `/help`, and `/exit` commands.
- Added approval mode, model name, context-window progress, and per-turn input/output token usage below the message input. Estimated usage is marked with `~`.
- Added Shift+Tab approval switching through `always`, `on risk`, `auto`, and `YOLO`. Changes apply to the next turn and do not modify the configuration file. YOLO skips all tool approval confirmations, including high-risk actions; disabled tools and forbidden operations remain blocked, and sudo may still require a terminal password.
- Fixed unintended high-risk approval bypasses through task-wide `All`, `--yes`, and `approval = "never"`; explicit TUI YOLO selection is the separate opt-in override.
- Hardened stream completion validation and secret redaction, shell timeout and cancellation handling, tool audit persistence, and TUI shutdown, history, rendering, and buffer limits.
- Fixed session storage locking and recovery, checkpoint integrity and permission restoration, file-operation undo coverage, special-file reads, knowledge ingestion limits, and numeric validation.
- Hardened update redirects, archive extraction, rollback reads, and release packaging arguments.
- Updated TUI dependencies to Ratatui 0.30.2 and Crossterm 0.29.0, removed vulnerable or unmaintained transitive dependencies, and raised the minimum Rust version to 1.88.
- Documented the full code audit and its verification results in `docs/code-audit-2026-10-06.md`.

## 0.5.0

- Removed the hard model-request iteration stop from live runs. qin now warns after 24 requests and continues; the default hard tool-call ceiling is 512 and the wall-time deadline is one hour. Existing `agent.max_iterations` settings remain accepted as the soft warning threshold.
- Added model/user diagnostics for repeated identical tool-call batches and for runs approaching the tool-call ceiling.
- Declared read-only parallel eligibility in the tool registry while retaining runtime approval, argument, and workspace-path checks before execution.
- Corrected the repository license text to match the Apache-2.0 metadata and existing documentation.
- Fixed OpenWrt apk release metadata to use the version from `Cargo.toml` instead of the stale package version.
- Updated locked `rustls` to 0.23.45 to resolve RUSTSEC-2026-0285 and replaced a yanked `chacha20` lock entry.

## 0.4.7

- Fixed path mutation approval prompts so file and directory changes consistently show the `[y/N]` confirmation suffix.

## 0.4.6

- Rejected `timeout`, `setsid`, and `nohup` wrappers for TTY-backed shell commands so interactive credentials stay in qin's foreground process group; use the shell tool's `timeout_seconds` instead.

## 0.4.5

- Fixed multi-step interactive shell prompts by transferring foreground terminal control to every TTY-backed child, not only elevated commands.
- Preserved command lifecycle events when an interactive child is terminated by the terminal's Ctrl-C signal, and handled commands that exit during terminal handoff.

## 0.4.4

- Added deterministic JSONL replay through the real model-independent tool, persistence, approval, and rendering paths, with durable request snapshots and input fingerprints.
- Added guarded concurrency for independent local read-only tool calls while keeping writes, approvals, external paths, and shell commands serialized.
- Added typed event handling and session invariant validation, plus safer interactive shell prompt and heartbeat behavior.

## 0.4.3

- Fixed approval prompts for non-TTY and JSON event consumers so the complete `[y/N]` prompt is emitted as a line instead of being hidden after the tool filename.
- Added closed approval outcomes with durable `approval/asked` and `approval/decided` pairs linked to the tool call, including distinct one-time and task-wide grants; missing or unavailable approval fails closed.
- Added policy-aware system instructions, stricter `apply_patch` and shell tool guidance, and safe JSON presentation metadata for tool cards, locations, and redacted diff sizes.

## 0.4.2

- Added a registry-backed tool execution pipeline with explicit preparation, authorization, dispatch, normalization, and observation stages.
- Persisted turn, user-message, assistant-message, tool-call, and tool-result events incrementally, including crash recovery that records interrupted tool outcomes without replaying potentially completed side effects.
- Added redacted event metadata for tool arguments and recovery coverage for both SQLite and lightweight non-SQLite session stores.

## 0.4.1

- Indented the "All subsequent shell commands are approved" notice so it aligns with the surrounding tool invocation events.
- Improved read-only command recognition: harmless stream-discarding redirections (`2>/dev/null`, `2>&1`, `>/dev/null`) no longer force approval, single-argument `--version`/`-V`/`version`/`--help`/`-h` queries against trusted programs (such as `python3 --version`) run without a prompt, and `command -v` plus read-only `pip`/`pip3`/`pipx` subcommands (`list`, `show`, `check`, `freeze`) are now recognized. Redirects to files, arbitrary interpreter invocations, and package mutations still require approval.

## 0.4.0

- Added release signing: CI signs `SHA256SUMS` with minisign and publishes `SHA256SUMS.minisig`; `qin update` verifies the signature against the embedded release public key before checking hashes and refuses unsigned or tampered releases outright.
- Added GitHub build-provenance attestations (SLSA) for every release asset and a CycloneDX SBOM (`qin-v<version>-sbom.cdx.json`) attached to each release.
- Added update rollback: `qin update` saves the previous executable as `qin.previous` and `qin update --rollback` restores it atomically, with the same trusted sudo/doas delegation for protected installations.
- Added file checkpoints and `qin undo`: typed file tools snapshot affected files before mutating them, `qin checkpoints` lists recent checkpoints, and `qin undo [ID]` restores overwritten content, removes created files, renames moved paths back, and recovers deleted files from snapshots or the trash directory after confirmation. Configurable via `[checkpoints]` (`enabled`, `max_file_bytes`, `keep`); requires the SQLite storage backend and does not track shell commands.
- Added AGENTS.md support: a hand-created, non-empty `AGENTS.md` beside the active configuration file is injected into the system prompt as project instructions (symlink-free, size-capped by `input.agents_md_max_bytes`, never auto-created); `qin doctor` reports whether it was loaded.

## 0.3.0

- Fixed `qin update` for protected system installations by detecting unwritable executable directories before downloading and safely delegating only the update command to a trusted `sudo` or `doas` executable.

## 0.2.9

- Fixed `sudo qin` to reuse the invoking user's configuration and SQLite data directories instead of unexpectedly switching to `/etc/qin` and `/var/lib/qin`.
- Preserved the invoking user's ownership when root creates or updates that user's configuration and database files.
- Standardized tool and shell durations with two-decimal seconds, switching to minutes after one minute and hours after one hour.
- Replaced the stray JSON `[` in web-search completion events with a result count and placed elapsed time before the count.
- Refined `approval = "on_risk"` with command-specific read-only rules for system, network, service, log, and package diagnostics, while rejecting option combinations that can write or execute helper programs.
- Allowed new files, directories, and non-overwriting copies inside the current workspace without a prompt; overwrites, moves, external paths, unknown shell commands, elevation, and destructive actions still require approval.
- Added an unconditional safety floor for recursive deletion of broad system/home paths, raw-device formatting or overwrites, fork bombs, and kill-all commands.
- Prevented read-only auto-approval from trusting executables resolved from user-writable `PATH` directories, and removed shell startup, exported-function, and dynamic-loader injection variables from child environments.
- Added `[y/N/All]` command approval; `All` approves subsequent shell commands only for the current task, while Forbidden operations remain blocked.
- Fixed interactive sudo prompts by handing the child process group foreground terminal control, giving prompts a newline-delimited terminal area, pausing heartbeats, inheriting stdin explicitly, and restoring qin's foreground group and terminal echo/mode after completion, cancellation, or timeout.
- Delayed the first ordinary command heartbeat until the configured interval instead of displaying `Command still running  0s` immediately.

## 0.2.8

- Added `qin update` with platform-aware GitHub release discovery, SHA-256 verification, bounded downloads, and atomic executable replacement.
- Added optional Redis-backed lightweight session storage with TLS support, outage fallback, recovery reconciliation, and integrity checks.
- Added an interactive configuration wizard with safe secret handling, backups, and dry-run support.
- Added Linux distribution, distribution version, kernel, and macOS version details to model runtime context when available.
- Improved `approval = "on_risk"` so recognized read-only shell queries run without approval while unsafe or ambiguous commands still require it.
- Changed unknown configuration fields and sections to emit warnings and be ignored for forward compatibility; invalid known settings remain errors.
- Hardened session files, Redis state handling, updater archives, runtime prompt escaping, and read-only command classification following a security audit.

## 0.2.0

- Added automated, checksummed GitHub release builds for Linux, macOS, and OpenWrt, including legacy `.ipk` and OpenWrt 25.12.5 SDK-built apk v3 packages for `aarch64_cortex-a53` and `x86_64`.
- Added `qin delete <SESSION_ID>` with confirmation, short-ID support, cascading history cleanup, and automatic creation of a new session when deleting the active one.
- Added durable context-compaction boundaries while preserving full session history.
- Hardened file, configuration, database, lock, shell, terminal, and HTTP trust boundaries.
- Added bounded model, search, command, tool-audit, and embedding response handling.
- Added API-key environment isolation for shell child processes.
- Added batched knowledge ingestion, pre-embedding deduplication, and streaming flat-vector search.
- Added strict configuration validation while retaining compatibility with fields generated by the 0.1 configuration template.
- Added dependency vulnerability scanning to CI and documented the security model and audit results.

## 0.1.0

- Initial Rust CLI agent with sessions, local tools, OpenAI-compatible models, knowledge search, and OpenWrt-oriented persistence.
