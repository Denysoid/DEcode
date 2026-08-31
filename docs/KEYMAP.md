# DEcode keymap and UI controls

[Русская версия](KEYMAP.ru.md) · [Documentation](README.md) · [Usage](USAGE.md)

Mouse and keyboard activate the same typed actions. `Tab` and `Shift+Tab` move focus, arrow keys navigate lists, `Space` toggles a choice, `Enter` activates it, and `Esc` closes or cancels without applying whenever the current dialog allows it.

The footer, **Help** menu, and `/` command palette are authoritative for the current screen and build.

## Global shortcuts

| Key | Action |
|---|---|
| `F10` | Open the menu bar |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next/previous main tab |
| `/` in an empty composer | Open command palette |
| `@` | Open attachment browser without discarding typed text |
| `Ctrl+N` | Create a new session |
| `Ctrl+O` | Open session manager |
| `Ctrl+Z` | Open checkpoint rewind |
| `Ctrl+M` | Select model, reasoning effort, and context budget |
| `Ctrl+K` | Open MCP manager |
| `Ctrl+L` | Open language intelligence/LSP |
| `Ctrl+B` | Open repository index/search |
| `Ctrl+G` | Open work modes and persistent goal |
| `Ctrl+J` | Open Queue & Steer follow-ups |
| `Ctrl+Y` | Open read-only side question (`/btw`) |
| `Ctrl+Shift+T` | Open or create an interactive Terminal tab |
| `End` | Return to and follow newest conversation output |
| `F6` | Pause active turn or resume paused turn |
| `F8` | Cancel a paused turn |
| `F7` | Feed or wake Pixel while idle |
| `Ctrl+R` | Reset current run through urgent control |
| `Ctrl+C` / `Esc` | Interrupt active turn or leave the current idle flow |

## Composer

| Key or action | Result |
|---|---|
| `Enter` | Send text and selected attachments |
| `Alt+Enter` | Insert a newline |
| Arrow keys | Move the text cursor; vertical movement applies to multiline input |
| `Home` / `End` | Move to start/end of the current line |
| `Backspace` / `Delete` | Delete the previous/next grapheme safely |
| Paste | Insert text, attach absolute file paths, convert a large paste to a text attachment, or capture a clipboard image |
| Click attachment chip | Select/remove the staged attachment through its visible action |

When the composer is empty, `Tab` and `Shift+Tab` select the next or previous tool card. `Enter` then expands or collapses it instead of sending an empty message.

## Command palette

- Type to filter built-in and custom slash commands.
- `Up` / `Down` selects an item.
- `Home` / `End` selects the first/last item.
- `Tab` / `Shift+Tab` moves between the list and action buttons.
- `Enter` runs the focused action.
- `Esc` closes without running it.

The palette exposes sessions, rewind, providers, MCP, LSP, repository index, privacy, permissions, auto-approval, usage, notifications, review, side chat, Queue & Steer, modes, instructions, skills, plugins, hooks, tabs, terminals, sidebars, and jump-to-latest.

## Attachment browser

| Key or action | Result |
|---|---|
| Type | Filter the current directory by name or path |
| `Up` / `Down` | Select a visible entry |
| `Home` / `End` | Select first/last visible entry |
| `Enter` on directory/shortcut | Navigate into it |
| `Backspace` with empty filter | Go to parent directory |
| `Space` on file | Add/remove it from multi-selection |
| `Enter` or **Attach** | Attach selected files, or the current file when none are selected |
| `Esc` | Close without adding files |

Visible shortcuts include workspace, home, Desktop, Downloads, roots/drives, and parent directory when available. Mouse clicks select rows and activate the same Close/Attach controls.

## Dialog conventions

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Next/previous enabled control |
| Arrow keys | Navigate lists or switch dialog panes |
| `PageUp` / `PageDown` | Scroll long content where supported |
| `Home` / `End` | First/last row or content boundary |
| `Space` | Toggle focused checkbox or binary hunk decision |
| `Enter` | Activate focused button/item |
| `Esc` | Close or reject without applying |

Confirmation and patch dialogs bind a decision to the exact turn, action, and digest. Long content must be reviewed through its final row before approval unlocks.

## Chat and tool cards

- Mouse wheel scrolls history while the pointer is over chat.
- Clicking a tool card selects it and opens its context menu.
- The card menu expands details or inserts a reference to the call/result into the composer.
- `End` restores follow-latest after manual scrolling.
- Untrusted tool/code text is sanitized before syntax highlighting.

## Sessions and lists

Arrow keys move selection; mouse wheel scrolls the list and keeps the selected row visible. `Home` and `End` jump to list boundaries. Action buttons can be reached with `Tab` or clicked directly. The exact actions depend on the screen: open/resume, fork, rename, pin, archive, restore, search, or close.

## Agents tab

The agent tree scrolls independently from the detail pane. Use the mouse wheel over the tree or keyboard navigation to move through off-screen descendants; selection and visual scroll position move together. Actions such as message, stop, review, raise budget, and cancel recovery appear only when valid for the selected agent state.

## Usage dialog

Select a deployment and activate **Set exact tariff**. Enter the three USD-per-million rates, then use **Save & recalculate** or cancel. Every editable field and action is available through mouse hit regions and `Tab`/`Shift+Tab` plus `Enter`/`Esc`.

## Interactive Terminal tab

The Terminal tab owns raw keyboard and paste input independently from the model command runner.

| Key | Action |
|---|---|
| `Ctrl+Shift+T` | Create/open a terminal |
| `F6` | Switch between raw terminal input and toolbar controls |
| `Ctrl+Shift+X` | Stop the active terminal process |
| `Ctrl+Shift+W` | Close the active terminal |
| Mouse wheel | Scroll terminal history when the child is not consuming mouse input |

Mouse sequences are forwarded only when the child application enables a supported terminal mouse mode.
