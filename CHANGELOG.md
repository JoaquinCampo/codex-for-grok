# Changelog

All notable changes are documented here. This project follows Semantic Versioning and the Keep a Changelog format.

## [Unreleased]

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
