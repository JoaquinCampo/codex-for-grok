# Contributing

## Development

```sh
cargo fmt --all --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

Tests must not require real Codex credentials or mutate the developer's Grok configuration. Use temporary directories and injected paths for setup, uninstall, and service-manager tests.

## Pull requests

Keep changes focused, add regression tests, preserve the Sol/Terra-only contract, and document user-visible behavior. Never commit authentication files, captured prompts, tokens, runtime logs, or generated ownership manifests.

## Releases

Releases are created from signed `vX.Y.Z` tags after CI passes. The tag must match `Cargo.toml`. Release artifacts include checksums and attestations. Homebrew updates are proposed to `joaquincampo/homebrew-tap` after publication.
