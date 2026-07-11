# codex-for-grok

An unofficial local bridge that lets Grok Build use a user's existing ChatGPT/Codex subscription. It exposes a loopback-only OpenAI Responses API, preserves Grok's native streaming/tool behavior, and reports live Codex subscription quota.

> This project is not affiliated with or endorsed by xAI or OpenAI. Grok, ChatGPT, Codex, and related names are trademarks of their respective owners. The bridge depends on undocumented upstream interfaces that may change.

## Supported models

- **Codex Sol** (`gpt-5.6-sol`)
- **Codex Terra** (`gpt-5.6-terra`)

Luna is intentionally unsupported because the Codex subscription HTTP endpoint rejects it. See [COMPATIBILITY.md](COMPATIBILITY.md).

## Install

```sh
brew install joaquincampo/tap/codex-for-grok
codex-for-grok setup --dry-run
codex-for-grok setup
codex-for-grok doctor
```

Before setup, install a current Codex CLI and authenticate:

```sh
codex login
```

`setup` is strictly additive. It preserves Grok, Composer, and every existing custom model. It appends only missing `codex-sol` and `codex-terra` entries. Conflicting aliases cause a safe refusal; they are never overwritten. A byte-for-byte backup and ownership manifest support safe recovery and uninstall.

## Commands

```text
codex-for-grok run
codex-for-grok setup [--dry-run] [--config PATH]
codex-for-grok doctor
codex-for-grok start
codex-for-grok stop
codex-for-grok restart
codex-for-grok status
codex-for-grok uninstall [--dry-run]
```

- `run` starts the bridge in the foreground.
- `setup` appends the two Codex model entries; it does not install or start a service.
- `start` installs and starts the per-user service after setup.
- `doctor` checks authentication, owned configuration, service definition (if present), and bridge identity.
- Lifecycle commands manage launchd on macOS and systemd user services on Linux.
- `uninstall` removes only unchanged artifacts recorded as owned; user-edited or pre-existing content is preserved.

The service listens on `127.0.0.1:18474` by default.

## Health and quota

```sh
curl http://127.0.0.1:18474/healthz
curl http://127.0.0.1:18474/readyz
curl http://127.0.0.1:18474/status
curl http://127.0.0.1:18474/v1/models
```

`/status` separates observed response token usage from the real Codex subscription quota obtained through Codex app-server. In Grok Build, `/codex-usage` can present that quota; Grok's built-in `/usage` remains tied to the xAI account.

## Runtime configuration

| Variable | Default | Purpose |
|---|---:|---|
| `CODEX_FOR_GROK_HOST` | `127.0.0.1` | Loopback bind address; non-loopback is rejected |
| `CODEX_FOR_GROK_PORT` | `18474` | Local port |
| `CODEX_AUTH_PATH` | `~/.codex/auth.json` | Codex authentication file |
| `CODEX_FOR_GROK_MAX_BODY_BYTES` | `4194304` | Maximum request body |
| `CODEX_FOR_GROK_MAX_STREAMS` | `16` | Concurrent stream limit |
| `CODEX_FOR_GROK_UPSTREAM_IDLE_TIMEOUT_SECS` | `180` | Stream idle timeout |
| `CODEX_FOR_GROK_DRAIN_TIMEOUT_SECS` | `30` | Graceful shutdown deadline |
| `RUST_LOG` | `info` | Foreground service log filter |

## Build from source

```sh
cargo build --release --locked
./target/release/codex-for-grok --help
```

The first release targets macOS ARM64/x86_64 and Linux GNU ARM64/x86_64. Windows and musl are not currently supported.

## Privacy and security

The bridge has no telemetry. It does not upload analytics or configuration. Credentials remain local and are sent only to the Codex upstream as required for authenticated requests. Do not share auth files or unreviewed logs.

See [SECURITY.md](SECURITY.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT © 2026 Joaquin Campo.
