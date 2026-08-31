# Contributing to DEcode

[Русская версия](CONTRIBUTING.ru.md) · [Documentation](docs/README.md) · [Security policy](SECURITY.md) · [Code of Conduct](CODE_OF_CONDUCT.md)

DEcode is a security-sensitive coding agent. Contributions are welcome, but correctness, recoverability, narrow authority, and a usable terminal interface take priority over feature count.

## Choose the right channel

- Reproducible defect: use the bug report form after searching existing issues.
- Focused user-facing improvement: use the feature request form.
- Exploitable security boundary failure: follow [SECURITY.md](SECURITY.md), never a public issue with details.
- Usage or setup problem: check [Troubleshooting](docs/TROUBLESHOOTING.md) first.

Remove credentials, bearer tokens, account identifiers, private prompts, repository content, usernames, and sensitive paths from all public text, screenshots, and logs.

## Development setup

Install current stable Rust through rustup with `rustfmt` and `clippy`, plus Git and the native build tools for your platform. See the complete [installation guide](docs/INSTALLATION.md).

```bash
git clone https://github.com/denysoid/DEcode.git
cd DEcode
cargo check --locked --all-targets
cargo test --locked --all-targets
```

Normal tests do not require real provider credentials. Never place a real secret in this repository, fixture output, snapshot, or issue.

## Before changing code

1. Read the [architecture](docs/ARCHITECTURE.md) for cross-module work.
2. Read the [security model](docs/SECURITY.md) when touching files, commands, providers, persistence, permissions, integrations, or display text.
3. Confirm existing behavior with a focused test or reproduction.
4. Keep unrelated working-tree changes intact.
5. Prefer the smallest coherent change that fixes the underlying invariant.

Open an issue before a large redesign so scope and compatibility can be discussed before implementation.

## Branches and commits

Create a short-lived branch from current `main`. Use a descriptive name such as `fix/session-resume-usage` or `docs/provider-setup`.

Write commit subjects in the imperative mood and keep one concern per commit. The preferred form is:

```text
type(scope): concise change
```

Use `feat`, `fix`, `docs`, `test`, `refactor`, `build`, or `chore` for `type`; omit the scope when it adds no information. Examples: `fix(session): preserve usage across resume` and `docs: explain Azure deployment names`.

Each commit should compile and leave the relevant tests passing. Do not mix generated build output, formatting unrelated to the change, or personal configuration into a functional commit. DEcode's private checkpoint refs are runtime recovery data, not project-history commits, and must never be copied into a pull request.

## Code standards

- Keep `#![forbid(unsafe_code)]` intact.
- Use typed commands, outcomes, errors, and state transitions instead of stringly typed control flow.
- Bound external input by size, count, time, retries, and cancellation.
- Treat model, repository, process, provider, MCP/LSP, and plugin data as untrusted.
- Resolve model file access through the workspace capability sandbox.
- Use exact patches or atomic writes for mutations.
- Preserve durable causal boundaries across pause, cancellation, failure, and restart.
- Avoid panics in runtime paths; convert recoverable failures into actionable errors.
- Do not add shell-prefix auto-approval, plaintext secrets, unbounded reads, or raw terminal escape rendering.
- Keep comments short and reserve them for non-obvious invariants or workarounds.

Run `cargo fmt`; do not hand-format around the standard Rust formatter.

## User interface changes

Every enabled visible action must have:

- a real mouse hit region;
- a keyboard/focus path;
- localized text in all supported UI locales;
- usable behavior at narrow and wide terminal sizes;
- sanitized rendering for untrusted content.

A clipped, hidden, or disabled control must not dispatch. Test scrolling with off-screen selections and verify that focus remains visible.

Render relevant deterministic scenes:

```bash
cargo run --locked --example ui_gallery -- 54 24 chat en dark
cargo run --locked --example ui_gallery -- 120 40 chat ru dark
cargo run --locked --example ui_gallery -- 160 50 mcp-add en light
```

For a new reusable scene or layout branch, extend `examples/ui_gallery.rs` and CI coverage rather than relying only on a screenshot.

If the visible README demo changes, regenerate it on Windows from the real gallery renderer:

```powershell
.\scripts\render-readme-demo.ps1
```

Review the resulting `assets/demo.gif` before committing it. Do not replace it with a mockup that can drift away from the shipped interface.

## Tests

Add a regression test for every reproducible bug. Place narrow unit tests beside the module under `src/`; use `tests/` for behavior crossing public/module boundaries. Existing tests under `src/` are standard Rust unit tests and are not compiled into release binaries.

Cover relevant failure paths, not only success:

- malformed, empty, oversized, duplicate, or stale input;
- timeout and cancellation;
- torn persistence and restart recovery;
- path traversal, symlink/reparse escape, and privacy denial;
- incomplete streamed output;
- conflicting manual Git changes;
- narrow terminal, scrolling, keyboard, and mouse behavior.

Tests must be deterministic, offline by default, and free of personal paths or live credentials.

## Documentation and compatibility

Update documentation whenever a change affects configuration, CLI flags, key bindings, provider behavior, persistence, security assumptions, or user-visible workflow.

- Keep English and Russian documents structurally equivalent.
- Update every supported UI translation for user-facing strings.
- Update `config.example.toml` for configuration fields.
- Update [CHANGELOG.md](CHANGELOG.md) for user-visible behavior.
- Preserve session/config compatibility or document and test a migration.

Do not claim support for a provider capability that is neither implemented nor verified. State explicit boundaries instead.

## Required checks

Run the same gates as CI:

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

For dependency or packaging changes, also run:

```bash
cargo package --locked --allow-dirty --no-verify
```

The pull request must not contain generated build output, release binaries, local configuration, sessions, logs, credentials, backup dumps, or editor/OS metadata.

## Pull requests

Open the pull request against the `main` branch of [denysoid/DEcode](https://github.com/denysoid/DEcode). A draft pull request is appropriate when you want early design or platform feedback; mark it ready only after the required gates pass and the description is complete.

A pull request should contain:

- the concrete problem and expected behavior;
- the smallest coherent implementation;
- security, persistence, compatibility, and platform impact;
- regression tests for success and meaningful failure paths;
- exact verification commands and results;
- screenshots or gallery output for visible UI changes;
- English/Russian documentation and UI translation updates when applicable.

Keep commits reviewable and avoid mixing refactors with unrelated behavior changes. Maintainers may ask for a large change to be split. A passing CI run is required but does not replace review of trust boundaries and failure behavior.
