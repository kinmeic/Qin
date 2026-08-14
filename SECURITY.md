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

## Release supply chain

Tagged releases are built on GitHub Actions with SHA-pinned actions and `cargo --locked` builds, and are published with three independent integrity layers:

- **Signatures**: `SHA256SUMS` is signed with minisign (`SHA256SUMS.minisig`). The signing secret key and its password live only in CI secrets; the public key is embedded in the qin binary, and `qin update` refuses any release whose signature is missing or invalid before it even checks hashes.
- **Provenance**: every release asset carries a GitHub build-provenance attestation (SLSA), verifiable with `gh attestation verify <asset> --repo kinmeic/Qin`.
- **SBOM**: a CycloneDX SBOM (`qin-v<version>-sbom.cdx.json`) is attached to each release for dependency auditing.

Manual downloads should verify `SHA256SUMS.minisig` with the published public key, then check the archive hash. `qin update` additionally saves the previous executable as `qin.previous` and can restore it with `qin update --rollback`.
