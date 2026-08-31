# DEcode documentation

[Русская версия](README.ru.md) · [Project README](../README.md)

Use this page as the documentation index. The root README gives the shortest path from clone to first task; these guides contain the operational and contributor details.

## User guides

| Guide | What it covers |
|---|---|
| [Installation](INSTALLATION.md) | Windows, Linux, and macOS prerequisites, source builds, Cargo installation, updates, and build verification |
| [Configuration](CONFIGURATION.md) | Configuration precedence, providers, credentials, endpoints, context limits, permissions, integrations, and logs |
| [Usage](USAGE.md) | First task, composer, attachments, approvals, sessions, context, modes, Pixel, terminal tabs, and recovery |
| [Troubleshooting](TROUBLESHOOTING.md) | Startup, provider, context, attachment, session, terminal, and reporting diagnostics |
| [Keymap](KEYMAP.md) | Complete keyboard, mouse, dialog, composer, and terminal controls |

## Technical reference

| Guide | Audience |
|---|---|
| [Feature matrix](FEATURES.md) | Users comparing implemented behavior with explicit boundaries |
| [Security model](SECURITY.md) | Users configuring trust boundaries and contributors changing sensitive code |
| [Architecture](ARCHITECTURE.md) | Contributors changing orchestration, persistence, providers, tools, or UI state |
| [Configuration template](../config.example.toml) | Operators who need the complete annotated TOML reference |
| [Examples](../examples/README.md) | Copyable configuration, plugin, and deterministic UI examples |
| [Contributing](../CONTRIBUTING.md) | Issue authors and contributors preparing a pull request |
| [Support policy](../SUPPORT.md) | Users preparing a reproducible, sanitized support request |
| [Security policy](../SECURITY.md) | Private vulnerability reporting scope and process |
| [Changelog](../CHANGELOG.md) | User-visible changes by release |
| [Code of Conduct](../CODE_OF_CONDUCT.md) | Participation standards, private reporting, and enforcement |

## Recommended reading paths

New user:

1. [Installation](INSTALLATION.md)
2. [Configuration](CONFIGURATION.md)
3. [Usage](USAGE.md)
4. [Security model](SECURITY.md)

Contributor:

1. [Contributing](../CONTRIBUTING.md)
2. [Architecture](ARCHITECTURE.md)
3. [Security model](SECURITY.md)
4. [Feature matrix](FEATURES.md)

When documentation and a local executable disagree, `decode --help` and the checked-out source are authoritative for that exact revision. Provider limits and model capabilities still come from the provider's current documentation and account configuration.
