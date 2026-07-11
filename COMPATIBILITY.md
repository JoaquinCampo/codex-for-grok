# Compatibility

| Component | Initial support |
|---|---|
| macOS | Apple Silicon and Intel; user service via launchd |
| Linux | x86_64 and ARM64 GNU; user service via systemd |
| Windows | Not supported |
| Grok Build | Custom Responses API models |
| Codex CLI | 0.144.0 or newer recommended |
| Models | `gpt-5.6-sol`, `gpt-5.6-terra` |
| Luna | Not supported; the subscription HTTP endpoint rejects it |

The bridge requires `codex login` and an owner-only Codex authentication file. The upstream subscription API is not public or versioned, so compatibility can break independently of this project.

Linux musl builds are not claimed until separately validated. Service commands require a working per-user launchd or systemd session. Foreground `run` mode remains available for diagnostics.
