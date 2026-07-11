# Security Policy

## Supported versions

Security fixes are provided for the latest released version.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability involving credentials, authentication, installer ownership, or local privilege boundaries. Report it privately through GitHub Security Advisories in `joaquincampo/grok-codex-bridge`.

Include the affected version, operating system, reproduction steps, impact, and any suggested mitigation. Do not include Codex tokens, refresh tokens, prompts, or private logs.

## Security model

The bridge binds only to loopback, reads the user's existing Codex authentication file, and does not collect telemetry. It never intentionally prints token values. Any process running as the same local user may be able to reach the loopback service or read user-owned files; this project is not a sandbox against a compromised local account.

`setup` uses ownership metadata and additive configuration changes. It refuses conflicting model or service definitions. `uninstall` removes only unchanged artifacts owned by the installation manifest.

This project uses private, undocumented upstream interfaces that may change without notice. Review release notes before upgrading and keep Codex CLI current.
