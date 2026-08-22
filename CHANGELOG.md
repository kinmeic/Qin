# Changelog

All notable changes to `qin` are documented here.

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
