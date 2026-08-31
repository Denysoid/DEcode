# Configuring DEcode

[Русская версия](CONFIGURATION.ru.md) · [README](../README.md) · [Installation](INSTALLATION.md) · [Usage](USAGE.md)

DEcode can be configured interactively, through a trusted TOML file, with environment variables, or with CLI flags. Start with the wizard unless you need reproducible deployment-specific settings.

## Configuration precedence

For ordinary settings, the effective priority is:

1. command-line arguments;
2. matching process environment variables;
3. an explicitly selected or platform user `config.toml`;
4. built-in defaults.

An explicit file is selected with `--config PATH` or `DECODE_CONFIG_FILE`. Without it, DEcode looks only in its platform user configuration directory. It does not silently trust an arbitrary `config.toml` from the current repository.

The optional `<workspace>/.decode.toml` is a lower-trust project file. It can set only the restricted project-safe subset, such as context, Pixel, and UI preferences. It cannot inject credentials, provider endpoints, executable hooks, MCP servers, or broader permissions.

## Interactive setup

On first launch, the wizard collects the language, provider, model/deployment, endpoint, workspace, context ceiling, and connection settings. Provider secrets are stored in the operating-system keyring under the `decode-provider` service. The generated TOML stores only the keyring account name.

The wizard can be skipped. Run DEcode again to reopen setup when no completed user configuration exists.

## Explicit configuration file

Copy the annotated template outside the repository:

```powershell
New-Item -ItemType Directory -Force D:\decode-config
Copy-Item .\config.example.toml D:\decode-config\config.toml
Copy-Item .\examples\configuration\instructions.md D:\decode-config\instructions.md
```

```bash
mkdir -p "$HOME/.config/decode"
cp config.example.toml "$HOME/.config/decode/config.toml"
cp examples/configuration/instructions.md "$HOME/.config/decode/instructions.md"
```

Edit every placeholder path, endpoint, deployment, and pricing value before use. Then launch with an explicit path:

```text
decode --config /absolute/path/to/config.toml --workspace /absolute/path/to/project
```

The canonical field reference is [config.example.toml](../config.example.toml). The exact CLI accepted by a build is available through `decode --help`.

## Credentials

Credential lookup order is:

1. selected provider's process environment variable;
2. selected provider's key in an explicitly passed `--env-file`;
3. keyring account configured by the setup wizard.

AWS Bedrock Runtime uses the standard AWS SDK credential chain instead of an API key. Ollama can use its local default without a key.

| Provider | Accepted environment variables |
|---|---|
| Azure OpenAI | `AZURE_OPENAI_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google Gemini | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| Anthropic | `ANTHROPIC_API_KEY` |
| Bedrock Mantle | `AWS_BEARER_TOKEN_BEDROCK` |
| Bedrock Runtime | AWS SDK chain, optionally `AWS_REGION` and `AWS_PROFILE` |
| OpenRouter | `OPENROUTER_API_KEY` |
| xAI | `XAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Together | `TOGETHER_API_KEY` |
| Fireworks | `FIREWORKS_API_KEY` |
| Cerebras | `CEREBRAS_API_KEY` |
| Perplexity | `PERPLEXITY_API_KEY`, `PPLX_API_KEY` |
| NVIDIA NIM | `NVIDIA_API_KEY`, `NVIDIA_NIM_API_KEY` |
| SambaNova | `SAMBANOVA_API_KEY` |
| Moonshot | `MOONSHOT_API_KEY` |
| Alibaba DashScope | `DASHSCOPE_API_KEY` |
| Hugging Face | `HF_TOKEN`, `HUGGINGFACE_API_KEY` |
| GitHub Models | `GITHUB_TOKEN`, `GH_TOKEN` |
| Ollama | optional `OLLAMA_API_KEY`, `DECODE_PROVIDER_API_KEY` |
| Custom compatible endpoint | `DECODE_PROVIDER_API_KEY` |

Set a temporary process variable instead of putting a secret in the repository:

```powershell
$env:OPENAI_API_KEY = "your-key"
.\target\release\decode.exe --provider openai --model "your-model" --workspace "D:\project"
```

```bash
export OPENAI_API_KEY="your-key"
./target/release/decode --provider openai --model "your-model" --workspace /path/to/project
```

An env file is never discovered automatically. Pass it explicitly, keep it outside the workspace, and include only the selected provider's key:

```text
decode --env-file /outside/workspace/decode.env --workspace /path/to/project
```

## Provider and endpoint rules

- Azure requires either a full `responses_url` ending in the intended Responses route or an `azure_base_url`; setting both is rejected.
- OpenAI uses the official Responses endpoint by default.
- Google and Anthropic use their native protocols and default endpoints.
- OpenAI-compatible providers use explicit adapters and their standard HTTPS endpoints.
- `compatible` and Bedrock Mantle require an explicit endpoint.
- Plain HTTP is rejected except an explicitly allowed loopback address, primarily for local Ollama development.
- URLs must be absolute, contain a host, and contain no embedded credentials or fragment.

Example Azure launch:

```powershell
$env:AZURE_OPENAI_API_KEY = "your-key"
.\target\release\decode.exe `
  --provider azure `
  --azure-base-url "https://YOUR-RESOURCE.openai.azure.com/openai/v1" `
  --deployment "YOUR-DEPLOYMENT" `
  --workspace "D:\project"
```

Example local Ollama launch:

```bash
decode \
  --provider ollama \
  --model "your-installed-model" \
  --allow-insecure-loopback true \
  --workspace /path/to/project
```

Model names, deployment names, context limits, supported modalities, and API availability belong to the selected provider. DEcode validates its own routing but cannot turn an unsupported provider/model combination into a supported one.

## Context settings

`agent.context_budget` is the active per-session budget. `agent.max_context_budget` is the trusted ceiling exposed by the runtime picker. Set the ceiling to the documented context window of the actual model or deployment; a larger number does not increase provider capacity.

Each session remembers its selected budget. Changing a budget also changes the default used by subsequently created sessions, without rewriting older sessions.

Context modes:

- `stateless`: DEcode sends a bounded reconstruction of the required conversation state on each request;
- `stateful`: DEcode uses provider-managed response state when supported while preserving local recovery data.

Local compaction preserves the initial task anchor and the newest complete causal tool group. If the newest required group alone cannot fit, DEcode rejects the request instead of silently dropping required context.

## Workspace, sessions, and instructions

- `--workspace` must receive an existing directory path.
- `agent.workspace_root` provides the default when the CLI flag is omitted.
- `agent.session_dir` controls durable session storage; the platform data directory is used by default.
- `agent.instructions_file` must be an explicit absolute regular UTF-8 file. It is not discovered from the repository.
- Repository guidance is discovered through hierarchical `AGENTS.md` files and bounded `@include` directives.

Use [examples/configuration/instructions.md](../examples/configuration/instructions.md) as a safe starting point.

## Commands and approvals

Keep `agent.shell.confirmation_mode = "always"` until you understand the trust model. `strict_allowlist` can bypass confirmation only for exact, direct, read-only argument vectors configured by the trusted user. It never treats shell prefixes, interpreters, build tools, or model claims as proof of safety.

Timeout rules match configured command prefixes. Commands still have bounded output, cancellation, and process-tree cleanup.

Examples are available in:

- [agent-profile.toml](../examples/configuration/agent-profile.toml);
- [command.toml](../examples/configuration/command.toml);
- [hook.toml](../examples/configuration/hook.toml);
- [privacy.ignore](../examples/configuration/privacy.ignore).

## Optional integrations

- MCP servers are trusted user configuration. Project files cannot add them.
- LSP servers must already be installed; DEcode does not download executables.
- GitHub operations call an authenticated `gh` executable and require confirmation for mutating actions.
- Remote code embeddings are disabled by default because they send filtered code chunks to the configured provider.
- Plugins should be installed only from audited sources with a verified digest.

## Logs and diagnostics

Set `[logging].level` and `[logging].dir`, or use `--log-level` and `--log-dir`. TUI logs go to files rather than stdout so they cannot corrupt the terminal screen. Prompts, response bodies, headers, credentials, hook output, and child-process stderr are excluded from structured tracing.

## Configuration safety checklist

- Keep secrets outside the repository.
- Use HTTPS except for an intentional loopback service.
- Verify the exact model/deployment and its real context limit.
- Keep command confirmation enabled.
- Start with MCP, remote embeddings, and executable hooks disabled unless required.
- Keep session and sub-agent worktree directories outside the source tree.
- Run `decode --help` after updating; the executable is the final authority for available flags.
