# DEcode examples

[Русская версия](README.ru.md) · [Project documentation](../docs/README.md)

These examples are safe starting points, not active configuration. DEcode does not load them directly from this directory.

## Layout

```text
examples/
├── configuration/
│   ├── agent-profile.toml   scoped research/writer profile
│   ├── command.toml         custom slash command
│   ├── hook.toml            trusted user lifecycle hook
│   ├── instructions.md      explicit user instruction file
│   └── privacy.ignore       additive machine-wide privacy rules
├── plugin/
│   ├── plugin.json          minimal plugin manifest
│   ├── commands/            plugin-provided slash commands
│   └── skills/              plugin-provided skills
└── ui_gallery.rs            deterministic offline TUI renderer
```

## Configuration examples

Read the comments in each file before copying it. Placement determines trust:

- project commands and profiles live under `<workspace>/.decode/` and cannot expand user authority;
- user commands, profiles, executable hooks, and privacy rules live under the platform configuration directory;
- the main instruction file is referenced by an explicit absolute path in trusted `config.toml`;
- executable hooks must be regular non-symlink files reviewed by the user.

The complete configuration reference is [config.example.toml](../config.example.toml). See [Configuration](../docs/CONFIGURATION.md) for precedence, credentials, and platform storage rules.

## Plugin example

`plugin/` demonstrates a manifest with one skill and one slash command. It does not contain native executable code. A distributable package places `plugin.json` at the ZIP root and lists every contributed path in `components`.

Before installing or publishing a plugin:

1. use a stable reverse-domain ID;
2. keep every component inside the package root;
3. validate names, versions, and relative paths;
4. create the ZIP without build output or secrets;
5. calculate and publish the exact SHA-256 digest;
6. test install, enable, disable, update, and removal in a disposable profile.

A verified digest proves package identity, not trustworthiness. Review every contribution before enabling it.

## UI gallery

The gallery renders the real UI into a deterministic `TestBackend`; it needs no terminal session or provider credentials.

```bash
cargo run --locked --example ui_gallery -- 120 40 chat en dark
cargo run --locked --example ui_gallery -- 120 40 mcp-add ru dark
cargo run --locked --example ui_gallery -- 100 32 lsp-add en light
```

Arguments are `width height screen locale theme`. Supported screens are `chat`, `mcp`, `mcp-add`, `lsp`, and `lsp-add`; unknown screen values render the chat state. CI exercises its maintained matrix across locales, themes, screens, and terminal sizes.
