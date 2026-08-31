# Troubleshooting DEcode

[Русская версия](TROUBLESHOOTING.ru.md) · [Documentation](README.md) · [Installation](INSTALLATION.md) · [Configuration](CONFIGURATION.md)

Start with the exact error text, the exact executable being run, and the effective configuration path. Do not post API keys, authorization headers, private prompts, repository content, usernames, or sensitive filesystem paths in a public issue.

## The window opens and closes immediately

DEcode is a terminal application. A double-clicked console closes as soon as the process exits, hiding configuration errors. Start it from an existing terminal:

```bat
cd /d D:\path\to\DEcode
target\release\decode.exe --workspace "D:\path\to\project"
```

```powershell
Set-Location D:\path\to\DEcode
& .\target\release\decode.exe --workspace "D:\path\to\project"
```

Keep the terminal open and read the final error. Administrator rights are not normally required and can select a different user profile, keyring, config, and session directory.

## `--workspace` requires a value

`--workspace` is not a switch. Pass an existing directory:

```text
decode --workspace /absolute/path/to/project
```

Quote a path that contains spaces. A file path is not accepted.

## The EXE and `cargo run` show different configuration or sessions

First confirm which binary is running:

```bat
where decode
```

```powershell
Get-Command decode -All
```

```bash
type -a decode
```

Then launch both forms with the same explicit paths:

```text
decode --config /absolute/config.toml --workspace /absolute/project
```

Check `agent.session_dir` in that config. Different binaries, operating-system users, `--config` values, or session directories legitimately produce different settings and session lists. Rebuild or reinstall after changing source:

```bash
cargo build --locked --release
cargo install --locked --path . --force
```

Do not run one copy elevated and another as a normal user when comparing keyring or per-user data.

## `invalid api.responses_url: relative URL without a base`

The configured endpoint is empty or relative. Use an absolute HTTPS URL with a host, or remove the custom field and use the provider default. For Azure, configure exactly one of:

- a complete `responses_url` containing the intended Responses route; or
- `azure_base_url` plus the correct deployment.

Do not set both. Check the effective file selected by `--config`; editing a different `config.toml` will not affect the process.

## HTTP 404 `Resource not found`

The server was reached, but the route or deployment was not found. Verify:

- provider selection;
- resource hostname and region;
- deployment or model name, including case;
- Azure base URL versus full Responses URL;
- account access to that API route;
- proxy rewriting.

A valid API key does not prove that a deployment or route exists. Test with the smallest plain-text request after correcting the endpoint.

## `This model is not supported by Responses API`

The chosen model and endpoint do not support the Responses protocol. Select a Responses-capable model/deployment or configure the provider adapter that implements the model's actual protocol. Increasing the context limit, retrying, or changing an attachment does not fix a protocol mismatch.

## Authentication fails

Run `decode --help` and verify the selected provider. Then check only the corresponding environment variable from the [credential table](CONFIGURATION.md#credentials). Common causes are:

- the key belongs to another provider, resource, region, or deployment;
- the shell was opened before the environment variable was set;
- `--env-file` was omitted or points inside the workspace;
- a stale keyring entry is selected;
- system time or an AWS profile/region is wrong;
- a corporate proxy removes authorization headers.

Never paste the key into logs or an issue. A `401` or `403` is not retried automatically because repetition cannot repair credentials or policy.

## Context suddenly decreases or a request is rejected

Distinguish three values:

- cumulative session tokens, which only grow;
- tokens in the most recent provider request;
- the selected per-request context budget.

Compaction intentionally reduces the second value, not the cumulative ledger. DEcode records a compaction marker in chat and preserves the initial task anchor plus the newest complete causal group. If that required group alone exceeds the selected budget, the request is rejected instead of dropping tool results or recent instructions.

Fixes:

1. verify the model's real documented context window;
2. choose a budget no larger than that window;
3. remove unnecessary large attachments from the new turn;
4. finish the current causal operation before starting an unrelated task;
5. fork or start a new session for a separate objective.

Setting `max_context_budget` to a larger invented number does not increase provider capacity. If UI totals exceed the budget, check whether the UI is showing cumulative usage rather than the current request size.

## A session returns to an unexpected context budget

Each existing session stores its own selected budget. The last explicit change also becomes the default for new sessions. Verify that you opened the intended session and the same session directory. If the value still changes after reopening the same session, capture the steps and sanitized session metadata and report a bug.

## An attachment appears as a path instead of a file

Before sending, a successful attachment appears as a chip with its filename, kind, and size. Use `@` or paste/select the file again if ordinary path text remains in the composer.

For clipboard images:

- make sure the clipboard currently contains bitmap image data, not only a filename or browser URL;
- ensure the terminal does not intercept `Ctrl+V` before DEcode receives it;
- on Wayland install `wl-clipboard`; on X11 install `xclip`;
- use the `@` browser as a deterministic fallback.

The `@` browser can reach workspace, home, Desktop, Downloads, parent directories, and filesystem roots/drives. `Space` selects multiple files. External and temporary files are snapshotted into session storage before the request.

## Drag and drop does nothing

Terminal emulators implement drag and drop differently. DEcode accepts the absolute path text emitted by a compatible terminal; it cannot force a host terminal to generate a drop event. Try:

1. drop into the composer and confirm that attachment chips appear;
2. paste the absolute path;
3. use `@` to select the file directly.

Quote paths with spaces if the terminal emits plain text. Multiple quoted absolute paths can be attached in one paste.

## A file is rejected or the model cannot read it

Check the filename, detected kind, size, total turn size, and provider capability. DEcode supports storage and routing for image, document, text, audio, and video, but not every provider/model accepts every kind. The limits are 16 files, 50 MiB per file, and 50 MiB total per turn by default.

An encrypted, corrupt, unsupported, or misleadingly renamed file can still be rejected. DEcode does not claim semantic extraction quality for a format the provider cannot process.

## Pause, resume, or cancellation behaves unexpectedly

`F6` pauses/resumes an agent turn; `F8` cancels a paused turn. Resume creates a new provider request from the last durable boundary. Duration, token, and cost totals belong to the logical turn and include completed work before the pause.

An external command may finish between cancellation request and process termination. When DEcode cannot prove its effect, it records an unknown/recovery state rather than assuming success or repeating it. Use Git diff and process inspection before retrying a destructive action.

## Commands fail although they work in another shell

The model command runner is non-interactive, has null stdin, and may use a different configured shell or `PATH`. Check:

- executable availability with `where.exe` on Windows or `command -v` on Unix;
- shell-specific quoting and separators;
- working directory;
- environment inherited by the DEcode process;
- command timeout and output limit;
- approval or sandbox denial shown in the tool card.

On Windows, `python` may be absent while `py -3` exists. Do not assume PowerShell syntax will run unchanged through CMD, or vice versa.

## Mouse, colors, or layout are wrong

Use a current terminal with UTF-8 and color support, then resize it above the smallest practical layout. Check whether the terminal or multiplexer captures mouse reporting. Compare behavior without SSH/tmux and with a common `TERM` such as `xterm-256color`.

Every action has a keyboard path. If a visible enabled control cannot be clicked but works through focus and `Enter`, record the terminal name, version, dimensions, operating system, locale, theme, and exact screen.

## Build fails

Update the stable toolchain and verify the pinned graph:

```bash
rustup update stable
cargo clean
cargo check --locked --all-targets
```

On Windows, a linker error normally means the MSVC C++ build tools or Windows SDK is missing. On Linux, install the distribution's C build toolchain. Network errors while fetching crates are separate from compiler failures; do not delete `Cargo.lock` to hide them.

## Collect safe diagnostics

Record:

```text
decode --help
rustc --version
cargo --version
git rev-parse --short HEAD
```

Also include operating system, terminal/version, terminal dimensions, selected provider/model without secrets, exact reproduction steps, expected behavior, and sanitized error text. Configure file tracing with `[logging]` or `--log-level`/`--log-dir`; DEcode intentionally keeps TUI diagnostics off stdout.

Search existing issues, then use the repository's bug form. Report exploitable security problems privately according to the [security policy](../SECURITY.md).
