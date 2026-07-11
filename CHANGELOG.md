# Changelog

All notable changes are documented here. This project follows Semantic Versioning and the Keep a Changelog format.

## [Unreleased]

## [0.1.2] - 2026-07-11

### Fixed

- Use Grok Build's singular `[model.<alias>]` custom-model schema and migrate exact bridge-owned legacy `[models.<alias>]` blocks transactionally.

## [0.1.1] - 2026-07-11

### Fixed

- Suppress expected `launchctl print` diagnostics when checking whether the macOS user service is already loaded.

## [0.1.0] - 2026-07-11

### Added

- Local streaming Responses API bridge for Codex Sol and Terra.
- Native `run`, `setup`, `doctor`, lifecycle, status, and uninstall commands.
- Strictly additive Grok model configuration with backups and ownership tracking.
- User services for launchd and systemd.
- Live Codex subscription quota reporting through `/status`.
- Bounded graceful shutdown and stream compatibility normalization.

### Security

- Loopback-only binding.
- Owner-only authentication and installation state checks.
- Transactional configuration and ownership-safe uninstall behavior.
