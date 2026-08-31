# DEcode architecture

[Русская версия](ARCHITECTURE.ru.md) · [Documentation](README.md) · [Security](SECURITY.md) · [Contributing](../CONTRIBUTING.md)

This guide maps runtime ownership, trust boundaries, durable state, and the expected path for cross-module changes. Read it before changing orchestration, provider adapters, tools, persistence, or UI state.

## Design goals

DEcode is built around five invariants:

1. one actor owns mutable agent state;
2. UI code renders snapshots and sends typed intent, but does not execute business operations;
3. model and repository data never become user authority;
4. streamed or external work becomes durable only at explicit causal boundaries;
5. every unbounded external resource receives count, byte, time, and cancellation limits.

## Runtime overview

```text
keyboard / mouse / paste
            |
            v
      ratatui UI state ---- typed OrchestratorCommand ----+
            ^                                             |
            |                                             v
 snapshots / events <---- agent::orchestrator <---- provider stream
                              |   |   |
                              |   |   +---- MCP / LSP / index / plugins
                              |   +-------- tools / approvals / checkpoints
                              +------------ history / sessions / sub-agents
```

`agent::orchestrator` is the single coordination actor. It owns API calls, causal history, tools, session persistence, checkpoints, sub-agents, MCP/LSP runtimes, repository indexing, plugins, and UI snapshots. Ordinary requests use a bounded command queue. Interrupt, reset, pause, and whip signals use a separate coalescing control plane so queue pressure cannot discard urgent control.

## Module map

| Module | Responsibility and invariant |
|---|---|
| `config`, `onboarding` | Typed CLI/TOML/environment merge, trust-aware validation, first-run setup, keyring references |
| `api` | Provider-specific request encoding, bounded HTTP/SSE/WebSocket transport, typed stream events, finite retry |
| `agent::orchestrator` | Single owner of the logical turn and all cross-runtime transitions |
| `agent::state` | Causal history, token accounting, context reconstruction, local compaction |
| `agent::persistence` | Checksummed append-only session journal, metadata, torn-tail recovery |
| `agent::checkpoint` | Git-backed snapshots and conflict-safe rewind of attributable changes |
| `agent::scheduler` | Sub-agent DAG validation, budgets, concurrency, and file-claim conflicts |
| `agent::subagents` | Recursive agent tree, isolated writer worktrees, WAL/recovery, scoped permissions |
| `parser` | Literal outer-tag scanner and complete-turn tool extraction |
| `tools` | Workspace capability sandbox, exact patching, atomic writes, bounded search and process execution |
| `attachments`, `clipboard` | MIME/kind validation, content-addressed blobs, digest verification, native clipboard ingestion |
| `mcp` | Bounded STDIO/HTTP clients, OAuth/keyring flow, per-server and per-tool decisions |
| `lsp` | Read-only JSON-RPC clients and privacy-filtered normalized results |
| `code_index` | Incremental lexical/symbol index and optional bounded remote embeddings |
| `plugins` | Manifest/marketplace validation, digest verification, atomic package materialization |
| `ui` | Ratatui widgets, focus and click registries, sanitized rendering of immutable snapshots |
| `terminal` | User-owned PTY/ConPTY lifecycle separated from model command execution |

## Logical turn lifecycle

1. Validate composer input and snapshot attachments into session storage.
2. Append the user input to logical history and build a bounded provider request.
3. Encode that request through the selected provider adapter.
4. Stream deltas into a cosmetic live preview and complete response buffer.
5. Reject torn/failed streams before authoritative parsing.
6. Parse complete native function calls or complete legacy tool envelopes.
7. Apply mode restrictions, privacy policy, approval policy, and lifecycle hooks.
8. Execute tools with cancellation, timeout, byte limits, panic containment, and atomic mutation.
9. Append exact results and replay-safe opaque provider items to durable history.
10. Continue until the response contains no tool calls or an iteration boundary returns control to the user.

The live preview is never authoritative. A visually displayed partial tool call cannot execute until the provider turn is complete and validated.

## Provider boundary

All adapters consume the same internal conversation and capability description. Each adapter is responsible for its own:

- endpoint and authentication validation;
- request serialization;
- attachment capability mapping;
- stream event normalization;
- token usage extraction;
- retry classification and sanitized errors.

Responses, Chat Completions, Anthropic Messages, Gemini Content, and Bedrock Runtime are not treated as interchangeable wire formats. Provider switching changes serialization and authentication, not filesystem or shell authority.

## History, context, and compaction

`agent::state` separates cumulative usage from the bounded input reconstructed for the next request. Context compaction is deterministic and causal:

- retain the initial task anchor;
- retain the newest complete user/assistant/tool group;
- compact older recoverable tool output before conversational intent;
- record a visible compaction event;
- fail closed if the required newest group cannot fit.

Stateful providers may carry a provider response identifier, but the local journal remains the recovery authority. Stateless mode always reconstructs a bounded request from local state.

## Tools and filesystem boundary

Model-selected paths are resolved through a workspace capability root. Traversal, absolute-path escape, symlink/junction/reparse escape, privacy-denied paths, and replacement races fail closed. Reads and search are bounded. Writes use exact patches or same-directory temporary files followed by atomic persistence.

Commands receive a fixed working directory, null stdin, time and output bounds, cancellation, process-tree cleanup, and independent approval. The interactive Terminal tab is user-controlled and does not create an auto-approval path for model commands.

## Attachments

An attachment journal entry stores a safe filename, detected kind, MIME, size, SHA-256, detail mode, and a reference to the session blob. It does not depend on the original Temp, Downloads, or clipboard path after ingestion.

Before every provider request, DEcode reopens the blob, verifies size and digest, and maps it to the selected provider's native part. Capability checks reject unsupported image, document, audio, or video input before HTTP transmission.

## Persistence and recovery

Session history uses checksummed append-only JSONL. Flush/fsync and torn-tail recovery protect acknowledged boundaries; periodic compaction writes a replacement atomically. The durable record distinguishes completed, failed, paused, cancelled, and unknown-effect operations.

Pause cancels active work and records the last confirmed causal boundary. Resume creates a new request from that boundary. It never pretends to continue a closed network stream. Rewind restores only changes attributed to a recorded checkpoint and refuses to overwrite conflicting manual edits.

## Sub-agents

Research agents are capability read-only. Writer agents use hierarchical isolated Git worktrees. The scheduler validates DAG dependencies, file claims, depth, fan-out, concurrency, iterations, wall time, output, per-agent tokens, and total tree tokens.

A nested writer integrates into its writer ancestor only after review. Durable acknowledgements and unknown-effect recovery prevent interrupted work from being silently treated as complete. Sub-agent MCP access is a separate opt-in and re-applies server permissions.

## UI architecture

The UI reads immutable/coalesced snapshots and emits typed commands. `FocusManager` and click-region registries map mouse and keyboard paths to the same action. A clipped or disabled control must not own an active click target.

Provider, tool, process, file, MCP, LSP, and plugin text is sanitized before layout or syntax highlighting. Pixel, the animated `D`, timers, and ETA are derived presentation state and cannot mutate agent behavior.

## Trust hierarchy

From highest to lowest authority:

1. compiled safety invariants;
2. explicit CLI and process environment from the local user;
3. explicit or platform user configuration outside the workspace;
4. restricted project configuration and repository instructions;
5. model output, files, tool/process output, MCP/LSP data, and remote responses.

A lower level may narrow behavior but cannot choose secrets, provider endpoints, executable hooks, or broader permissions.

## Persistence map

| Data | Location rule |
|---|---|
| User configuration | Platform configuration directory or explicit `--config` |
| Provider/OAuth secrets | Process environment, explicit env file, or OS keyring |
| Session journal and metadata | Configured `agent.session_dir` or platform data directory |
| Attachment blobs | Session-adjacent content-addressed store |
| Checkpoints and sub-agent WAL | Session/private data outside normal source files |
| Writer worktrees | Configured sub-agent worktree root |
| Index and embedding cache | Workspace/privacy/model-keyed cache under private data |
| Plugins and user-managed connections | Platform data/config stores |
| Structured logs | Explicit log directory, never TUI stdout |

## Adding or changing a feature

For every external, privileged, or mutating path:

1. define typed configuration, command, outcome, and error;
2. identify the source's trust level and deny lower-trust overrides;
3. set count, byte, time, retry, and iteration bounds;
4. make cancellation observable before irreversible commit;
5. keep filesystem operations capability-relative and privacy-filtered;
6. preserve replay-safe state at an explicit durable boundary;
7. provide mouse and keyboard access to every visible action;
8. sanitize untrusted display data before styling;
9. test success, denial, timeout, cancellation, duplicate input, torn state, and recovery;
10. update English and Russian documentation plus all UI translations.

Run the complete gates from [CONTRIBUTING.md](../CONTRIBUTING.md) before opening a pull request.
