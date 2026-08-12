# Security policy

## Reporting a vulnerability

Please do not open a public issue for an unpatched vulnerability. Use GitHub's private vulnerability reporting feature for this repository and include the affected version, reproduction steps, impact, and any suggested mitigation.

## Security model

`qin` executes model-selected tools on the local machine. Its approval rules, path checks, output limits, secret redaction, and conservative defaults reduce risk, but they do not provide a complete operating-system sandbox.

- Run `qin` as an ordinary user unless a task truly requires administrator access.
- Review every displayed command and every request to access paths outside the current working directory.
- Keep configuration files and the session database private; they may contain secrets, prompts, command output, and project data.
- Prefer API keys supplied through environment variables. Configured key variables are removed from child shell processes.
- Treat model endpoints, search services, imported documents, and model output as external trust boundaries.
- Use `--dry-run` to inspect a plan without running commands or modifying files.

If you require stronger isolation, run `qin` inside a dedicated container, virtual machine, restricted user account, or operating-system sandbox with only the necessary directories mounted.
