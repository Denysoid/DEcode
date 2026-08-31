# DEcode feature matrix

[Русская версия](FEATURES.ru.md) · [Documentation](README.md) · [Usage](USAGE.md) · [Security](SECURITY.md)

This matrix distinguishes implemented behavior from explicit boundaries. It describes the current source tree, not a promise that every third-party model, deployment, terminal, or account enables the same capabilities.

## Core capabilities

| Area | Implemented | Explicit boundary |
|---|---|---|
| Model APIs | Azure/OpenAI Responses; native Gemini and Anthropic; Bedrock Mantle bearer Responses; native Bedrock Runtime `ConverseStream`; explicit Chat Completions adapters for OpenRouter, xAI, Groq, Mistral, DeepSeek, Together, Fireworks, Cerebras, Perplexity, NVIDIA NIM, SambaNova, Moonshot, DashScope, Hugging Face, GitHub Models, and Ollama | Provider/model names, API availability, context windows, and account policy remain provider-owned; nonstandard fields require explicit configuration |
| Streaming | Typed SSE/WebSocket events, live preview, stream-idle timeout, finite retry/backoff, authoritative complete-turn parsing | A closed stream cannot be resumed at the next token; partial tool calls never execute |
| Attachments | Images, documents, text, audio, and video; `@` filesystem browser; multiple selection; native clipboard image paste; path paste/drop; content-addressed SHA-256 session storage | Actual modalities depend on provider/model; 16 files and 50 MiB per file/turn by default; unsupported input fails before the request |
| Large pastes | Oversized composer text becomes a durable text attachment | Input larger than the attachment ceiling is rejected, not silently truncated |
| Usage and cost | API token ledger, cached-token accounting, long-context tiers, built-in tariffs, bounded provider-scoped catalog refresh, local overrides, historical recalculation | Dynamic account/region/SKU pricing requires an exact local tariff; displayed cost is an estimate |
| Terminal UI | Mouse hit regions, full keyboard paths, menu bar, six tabs, cards, context menus, palettes, responsive sidebars, themes, animation | Terminal-native rather than a desktop/browser UI; host terminal behavior still matters |
| Localization | Twelve interface locales and cross-locale deterministic renders | English and Russian repository documentation are maintained directly; other documentation translations are not bundled |
| First run | Language, provider, model/deployment, endpoint, workspace, purpose, context, transport, retry, and relevant AWS settings; secret storage in keyring | The wizard cannot create provider access or infer an undocumented deployment |
| Configuration | Typed TOML, environment and CLI precedence; restricted `.decode.toml`; explicit instructions and env files | Repository-controlled config cannot choose credentials, privileged endpoints, executable hooks, MCP servers, or broader permissions |
| Sessions | JSONL resume, fork, rename, pin, archive, search, checksums, and torn-tail repair | In-flight network data is cancelled and never claimed as durably received |
| Pause and recovery | Urgent cancellation, durable `paused` state, resume from confirmed history, logical-turn usage aggregation, explicit cancel | Resume starts a new request; unknown external effects require inspection instead of blind replay |
| Context | Per-session budget, persisted default for new sessions, stateless/stateful modes, visible causal compaction | A configured ceiling cannot exceed the real model limit; required recent state is never silently removed to force a request |
| Checkpoints | Conversation plus attributable file rewind with Git conflict checks | Requires a Git baseline; no repository-wide hard reset or overwrite of conflicting manual edits |
| Diagnostics | Structured JSON tracing for session, turn, provider, tool, MCP, sub-agent, and checkpoint lifecycles | Prompts, bodies, headers, credentials, hook output, and child stderr are excluded |

## Agent workflow and integrations

| Area | Implemented | Explicit boundary |
|---|---|---|
| Modes | Independently composable Plan, Explore, Review, Goal, Deep Thinking, and reasoning effort | Prompt mode never overrides a tool or permission boundary |
| Approvals | Exact command decisions, revisioned session grants, patch hunk review, auto-approval center | Forced, destructive, ambiguous, and model-requested boundaries remain manual |
| Tools | Bounded read/search, exact patching, atomic writes, process execution, cancellation, output limits | Filesystem access remains workspace-capability scoped; interactive shell input is not exposed as a model bypass |
| Sub-agents | Recursive bounded tree, research/writer roles, DAG dependencies, hierarchical worktrees, file claims, messaging, review, WAL recovery | Depth, fan-out, concurrency, time, output, and token budgets are mandatory |
| MCP | STDIO/HTTP, OAuth/keyring, permissions, enable/disable, add/edit UI, sub-agent opt-in | Secrets remain in keyring/environment references; sub-agents do not inherit access implicitly |
| LSP | User-configured server lifecycle, diagnostics, symbols, definition, references, and hover | DEcode does not download or install language-server binaries |
| Repository index | Incremental lexical/symbol search plus optional hybrid embeddings and cache | Remote embeddings are explicit opt-in and send only bounded privacy-filtered chunks |
| Plugins | `plugin.json`, ZIP packages, marketplaces, digest verification, update/enable/remove, atomic activation | No arbitrary native-code loader; only known bounded contribution types are materialized |
| Extensibility | Skills, slash commands, agent profiles, user hooks, hierarchical `AGENTS.md` and bounded includes | Project hooks cannot execute; project profiles can narrow but not expand trust |
| GitHub | List, open, checkout, and create draft pull requests through authenticated `gh` | No unattended merge, force-push, or unrestricted push |
| Interactive terminals | User-owned PTY/ConPTY tabs with create, input, stop, close, paste, resize, and mouse forwarding | Separate from model tool execution and its approval system |
| Notifications | Bounded in-app inbox and per-class terminal bell | No bundled external push-notification service |
| Pixel | Persistent optional mascot, idle interaction, runtime-derived moods and animation | Presentation only; it does not alter model responses, costs, permissions, or scheduling |

## Sub-agent limits

Defaults under `[agent.subagents]` are:

```toml
max_parallel = 4
max_per_session = 16
max_depth = 3
max_children_per_agent = 4
max_tokens_per_agent = 150000
max_total_tokens_per_session = 500000
```

The root is depth 0. Parallel requests reserve estimated input plus maximum output before dispatch. When a budget guard is reached, the Agents tab exposes explicit **Raise budget +50K** and **Stop branch** decisions. Configuration is also constrained by compiled hard caps.

Writer isolation, durable acknowledgement, file-claim scheduling, DAG failure propagation, descendant review, and unknown-effect recovery make this stronger than a basic flat worker pool. They do not justify a categorical claim that DEcode outperforms every hosted coding agent: production infrastructure, private evaluation sets, operational scale, and current hosted behavior are not reproducible from this repository.

## Tested surface

CI runs formatting, compilation, Clippy with warnings denied, and all targets on Windows, Linux, and macOS. Separate jobs exercise Unix PTY, Windows ConPTY, and 288 deterministic UI renders across three screens, twelve locales, two themes, and four terminal sizes.

This coverage reduces regressions; it is not proof for every terminal emulator, proxy, model revision, filesystem, SSH/multiplexer combination, or provider outage.

## Intentional non-goals

- Hiding provider limitations behind lossy conversion or invented capability.
- Executing incomplete streamed output.
- Treating model statements as user approval.
- Discovering secrets or privileged config from an untrusted repository.
- Automatically installing arbitrary executables for LSP, MCP, or plugins.
- Replacing Git review, project tests, backups, or human judgment.
