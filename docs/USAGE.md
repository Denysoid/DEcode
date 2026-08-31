# Using DEcode

[Русская версия](USAGE.ru.md) · [Documentation](README.md) · [Configuration](CONFIGURATION.md) · [Keymap](KEYMAP.md)

DEcode is a terminal coding agent. You choose a workspace, describe an outcome, review requested authority, and keep the resulting code changes under normal Git control.

## Start a workspace

Launch the installed executable with an explicit project directory:

```powershell
decode.exe --workspace "D:\projects\my-app"
```

```bash
decode --workspace /home/user/projects/my-app
```

To use a reproducible configuration, add `--config`:

```text
decode --config /absolute/path/to/config.toml --workspace /absolute/path/to/project
```

The workspace must already exist. Run `decode --help` for every option accepted by the current build.

## Complete a first task

1. Open a clean or intentionally dirty Git working tree.
2. State the desired result, relevant constraints, and how success should be verified.
3. Attach examples or source material when needed.
4. Review command and patch approvals; approve only the exact action you understand.
5. Let the turn finish, then inspect the final answer and Git diff.
6. Run the project's own tests before committing.

A useful request names the outcome rather than prescribing every implementation detail. Include exact errors, expected behavior, affected platform, and a minimal reproduction when reporting a bug.

## Composer

- `Enter` sends the current message.
- `Alt+Enter` inserts a newline.
- `/` in an empty composer opens built-in and custom commands.
- `@` opens the file browser even after text has already been entered.
- `Ctrl+V` pastes ordinary text or captures an image directly from the native clipboard.
- A very large text paste becomes a `.txt` attachment instead of being silently truncated.

The composer may contain text and multiple attachments in the same turn. Attached files appear as removable chips before submission.

## Attach files

The `@` browser can navigate the workspace, home directory, Desktop, Downloads, parent directories, and filesystem roots or Windows drives. Type to filter, use arrows to select a row, `Enter` to enter a directory, and `Space` to select multiple files. Activate **Attach** after making the selection.

Other supported input paths:

- paste one or more absolute file paths;
- paste an image with `Ctrl+V`;
- drag files from the desktop file manager into a terminal that converts drops to file paths.

DEcode accepts image, document, text, audio, and video files, but the selected provider and model decide which modalities can actually be sent. Unsupported modalities fail before the request. Files are copied into the session's content-addressed store, bounded by size/count limits, and verified by digest before transmission. The original temporary or Downloads path is not used as the model-visible attachment.

Default per-turn limits are 16 files, 50 MiB per file, and 50 MiB total. File type is detected and validated; an extension alone does not grant execution or bypass provider capability checks.

## Read a turn

The chat distinguishes user messages, model reasoning/status, tool calls, tool results, failures, and the final answer. Tool cards can be selected with the mouse or, while the composer is empty, with `Tab` and `Shift+Tab`. Open a card to inspect full details or mention its call/result in a follow-up.

The final answer includes accumulated duration, token usage, and estimated cost for the logical turn. A pause, retry, or recoverable failure does not intentionally reset those logical-turn totals.

Use `End` to return to the newest output after scrolling upward. Auto-follow remains suspended while you inspect older history.

## Review approvals and patches

Command approval is bound to the exact action shown. Read the command, working directory, reason, and requested scope. A session grant applies only to its exact revisioned rule and is not a general shell bypass.

Patch review works per hunk. Accept or reject each visible change, scroll through long content, and confirm only after the final row has been reviewed. Manual changes made after a checkpoint are conflict-checked during rewind.

Use `agent.shell.confirmation_mode = "always"` until you have deliberately configured narrower trusted rules.

## Pause, resume, interrupt, and cancel

- `F6` pauses an active agent turn and resumes a paused turn.
- `F8` cancels a paused turn instead of resuming it.
- `Ctrl+C` or `Esc` interrupts active work according to the current screen.
- `Ctrl+R` resets the current run through the urgent control path.

Pause is durable at a confirmed causal boundary. Resume starts a new provider request from recorded state; it cannot continue a closed SSE/TCP stream at the exact next token. An incomplete streamed tool call is never treated as executed. When an external command's effect is unknown, DEcode reports that uncertainty instead of replaying it blindly.

## Sessions

Use `Ctrl+N` to create a session and `Ctrl+O` to open the session manager. Sessions can be resumed, forked, renamed, pinned, archived, and searched. Each session keeps its own history, attachment references, context budget, usage ledger, checkpoints, and recovery metadata.

Session journals are append-only JSONL records with checksums. A process crash can discard an invalid torn tail, but it does not rewrite earlier valid history. Starting the same binary with a different user, configuration, or session directory can show a different session list; use the same `--config` and `agent.session_dir` when continuity matters.

## Model, reasoning, and context

Open the runtime picker with `Ctrl+M`. It controls the model or deployment, reasoning effort, and active context budget.

- The selected budget belongs to the current session.
- Changing it also becomes the default for newly created sessions.
- Returning to an older session restores that session's saved value.
- `max_context_budget` is only a UI/runtime ceiling; it cannot enlarge the model's real context window.

When a request approaches the budget, DEcode compacts older recoverable history while retaining the initial task anchor and newest complete causal tool group. The chat records compaction. If required recent state alone cannot fit, the request fails explicitly rather than losing that state silently.

## Work modes

Open modes and persistent goals with `Ctrl+G`. Plan, Explore, Review, Goal, and Deep Thinking alter how the agent approaches work; they do not grant additional filesystem, shell, MCP, or network authority. Combine modes only when their behavior is useful for the current task.

The Queue & Steer panel (`Ctrl+J` outside composer editing) accepts follow-up direction for active work. Side Question (`Ctrl+Y`) asks a read-only question without turning it into an implementation instruction.

## Pixel

Pixel is the optional mascot panel at the top of the chat. Its state is presentation-only and does not affect model quality, permissions, or token accounting.

- Enable or disable Pixel from **View**; the preference persists across sessions.
- `F7` feeds or wakes Pixel only while the agent is idle.
- During generation, Pixel reflects runtime state and cannot be fed.

## Terminal tab

The Terminal tab runs user-controlled interactive PTYs and is separate from commands requested by the model. Open a terminal with `Ctrl+Shift+T`. Inside that tab, `F6` switches between raw terminal input and toolbar control; `Ctrl+Shift+X` stops the active process and `Ctrl+Shift+W` closes the terminal.

Text typed into an interactive terminal is not an approval for a model tool call. Model-triggered commands still follow their own sandbox and confirmation policy.

## Optional integrations

The menu and `/` palette expose:

- MCP connections and per-tool permissions;
- installed LSP servers;
- repository indexing and optional remote embeddings;
- skills, plugins, hooks, custom commands, and agent profiles;
- GitHub pull-request workflows through an authenticated `gh` CLI;
- sub-agents, reviews, notifications, usage, privacy, and auto-approval settings.

Enable one integration at a time and verify its trust boundary. DEcode does not install LSP executables automatically, and project-controlled configuration cannot silently add privileged MCP servers or executable hooks.

## Exit safely

Interrupt or pause active work before closing the terminal when possible. An operating-system termination still triggers recovery from the last durable boundary, but no terminal agent can guarantee completion of an external process after a forced kill or power loss.

Before committing agent changes:

```bash
git status --short
git diff --check
git diff
```

Continue with the [complete keymap](KEYMAP.md), [security model](SECURITY.md), or [troubleshooting guide](TROUBLESHOOTING.md).
