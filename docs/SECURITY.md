# DEcode security model

[Русская версия](SECURITY.ru.md) · [Security policy](../SECURITY.md) · [Documentation](README.md) · [Architecture](ARCHITECTURE.md)

DEcode reduces the risk of model-directed development work; it cannot make arbitrary code execution safe. The user remains responsible for provider trust, approvals, Git review, backups, and the consequences of commands they authorize.

## Threat model

DEcode treats these as untrusted input:

- model responses and streamed deltas;
- repository files, `AGENTS.md`, skills, profiles, and project configuration;
- tool, process, Git, MCP, LSP, index, plugin, and hook output;
- remote endpoints, package archives, OAuth responses, and marketplace metadata;
- filenames, terminal text, and attachment metadata.

The local user, explicit CLI/process environment, and user configuration outside the workspace carry more authority. A lower-trust source may narrow behavior but cannot grant itself credentials, executable hooks, privileged endpoints, or broader permissions.

## Filesystem isolation

Model file operations pass through a workspace capability sandbox. The resolver rejects:

- absolute paths outside the granted root;
- `..` traversal;
- symlink, junction, and reparse-point escape;
- privacy-denied paths;
- path replacement races detected before commit.

Reads, directory walks, searches, and outputs are bounded. Mutable files use exact single-match patches or a temporary file in the destination directory followed by atomic persistence. Patch policy can require per-hunk human review.

Checkpoint rewind is limited to changes attributed to the agent's recorded checkpoint. It does not use a repository-wide destructive reset and refuses to overwrite conflicting manual edits.

## Command execution

Model-triggered commands use:

- a fixed working directory;
- null stdin;
- execution and idle timeouts;
- bounded captured output;
- cancellation and process-tree cleanup;
- an independent harness confirmation boundary.

The strict allowlist matches exact resolved argument vectors from trusted configuration. Shell prefixes, interpreters, build tools, aliases, or a model's claim that a command is safe do not qualify. Session grants are exact, revisioned, non-persistent, and cannot override forced/destructive review.

Interactive Terminal tabs are controlled by the user and are not an auto-approval path for model tools.

## Streaming and tool calls

Live streamed text is presentation state. A tool call becomes executable only after the complete authoritative response is received and parsed. A truncated stream, invalid envelope, cancellation, timeout, or parse failure cannot execute a partial call.

Tool results are appended with their causal identifiers. Resume starts from the last durable boundary. An operation with an unknown external effect is surfaced for recovery instead of being silently repeated.

## Network and providers

- Requests have total and stream-idle timeouts, finite exponential backoff, and bounded `Retry-After`.
- Authentication and wire protocol are selected by a typed provider adapter.
- Responses `401` and `403` are not retried automatically.
- Plain HTTP is rejected except for explicit loopback development opt-in.
- URLs must be absolute and cannot embed credentials or fragments.
- Error payloads are size-bounded and scrubbed before display/logging.

Changing provider does not change filesystem or command permissions. A custom compatible endpoint is a new trust decision because it receives prompts and selected attachments.

## Attachments and clipboard

Human-selected files and clipboard images are bounded, typed, hashed, and copied into a content-addressed session store. Before network use, DEcode rechecks byte length and SHA-256. The provider adapter verifies the requested modality before sending data.

MIME or extension never makes a file executable. Symlinked selections are rejected. External and temporary paths are accepted only through an explicit human UI/paste action and are replaced by the stored blob reference in durable history.

## Sessions, pause, and recovery

Session records are checksummed append-only JSONL with flush/fsync and torn-tail recovery. Periodic journal compaction writes an atomic replacement. A process crash may discard an uncommitted tail but must not rewrite acknowledged earlier records.

Pause cancels active work and persists the confirmed causal boundary. It cannot preserve an unreceived provider token or guarantee that a third-party process stopped before an irreversible external effect. Unknown effects remain explicit.

## Context integrity

Compaction preserves the initial task anchor and newest complete causal group. Old recoverable output is compacted before required recent state. If required state cannot fit in the selected budget, DEcode rejects the request rather than sending a causally incomplete reconstruction.

The configured maximum is not proof of provider capacity. Set it to the real documented model/deployment limit.

## Sub-agent isolation

Research agents receive read-only capability. Writer agents mutate hierarchical isolated Git worktrees and are scheduled through DAG dependencies and file claims. Integration requires review, including nested writer descendants.

Depth, fan-out, concurrency, iterations, transcript, output, wall time, per-agent tokens, and total tree tokens are bounded. Parallel calls reserve budget atomically. Sub-agent MCP is disabled by default and, when enabled, applies independent per-server tool policies; it never inherits shell auto-approval.

## MCP, LSP, plugins, hooks, and embeddings

- MCP STDIO/HTTP connections are trusted user configuration; OAuth uses Authorization Code with PKCE, loopback callback, state validation, and keyring storage.
- LSP is read-only from the agent boundary and never downloads a server executable.
- Marketplace plugin packages require exact SHA-256 and bounded ZIP validation before atomic activation.
- Project hooks cannot execute; executable lifecycle hooks are user-local trusted configuration.
- Remote embeddings are disabled by default and receive only privacy-filtered, count/size-bounded chunks.

Audit every external server or package independently. A valid digest proves package identity, not that its contents are safe.

## Secrets and logs

API keys are read from process environment, an explicit env file outside the workspace, or the operating-system keyring. Normal TOML and session JSONL store keyring references rather than secret values. Runtime secret values use secrecy wrappers and sensitive HTTP headers.

Structured tracing records identifiers, outcomes, and timing, but excludes prompts, response bodies, headers, credentials, hook output, and child-process stderr. Provider and MCP errors are scrubbed against configured secrets. Public diagnostics must still be reviewed manually because repository paths and business data can be sensitive without being credentials.

## Terminal display safety

Provider, tool, process, file, MCP, LSP, GitHub, and plugin text is stripped of dangerous terminal controls and bidirectional formatting before Ratatui layout or syntax highlighting. TUI logs go to files rather than stdout/stderr so diagnostics cannot corrupt terminal ownership.

## Recommended secure setup

1. Work in a Git repository with a known baseline and independent backup.
2. Keep `agent.shell.confirmation_mode = "always"` initially.
3. Store secrets outside the workspace and never commit a populated config/env file.
4. Use HTTPS; allow insecure loopback only for an intentional local service.
5. Leave MCP for sub-agents, remote embeddings, plugins, and executable hooks disabled until needed.
6. Install LSP and plugin executables from sources you audit.
7. Set context and token budgets to real provider limits.
8. Read every destructive command and patch before approval.
9. Review Git status/diff and run project tests before committing.

## Security limitations

No local agent can fully prevent:

- a user approving a harmful but syntactically valid action;
- a trusted provider or integration retaining submitted data;
- a command exploiting a vulnerability in the operating system or dependency;
- side effects completed outside DEcode before cancellation;
- data exposure already allowed by an intentionally broad workspace or privacy rule.

Use a disposable environment or operating-system sandbox for untrusted repositories and high-risk commands.

## Reporting a vulnerability

Do not open a public issue containing exploit details, credentials, private code, or sensitive logs. Follow the repository [security policy](../SECURITY.md) and use GitHub private vulnerability reporting when it is available.
