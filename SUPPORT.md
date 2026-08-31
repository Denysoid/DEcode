# Support

[Русская версия](SUPPORT.ru.md) · [Documentation](docs/README.md) · [Troubleshooting](docs/TROUBLESHOOTING.md)

## Before opening an issue

1. Build or install the latest `main` revision.
2. Read the relevant setup guide and [troubleshooting checklist](docs/TROUBLESHOOTING.md).
3. Search open and closed issues for the exact error.
4. Reproduce with the smallest safe workspace and the minimum optional integrations enabled.
5. Remove secrets and private data from every diagnostic artifact.

## Where to report

- Reproducible product defect: use the **Bug report** issue form.
- Focused improvement: use the **Feature request** form.
- Security vulnerability: follow [SECURITY.md](SECURITY.md) and report privately.
- Provider outage, quota, billing, account policy, or unsupported model: contact that provider; DEcode cannot change the account-side service.

## Include in a bug report

- commit hash or release;
- operating system and architecture;
- terminal name, version, and dimensions;
- selected provider and model/deployment without credentials or private endpoint details;
- exact steps, expected result, and actual result;
- sanitized error and relevant logs;
- whether the problem reproduces with optional MCP/LSP/plugins/hooks disabled.

Do not attach API keys, bearer tokens, authorization headers, private prompts, proprietary code, session journals, account identifiers, or unredacted user paths.

Support is provided on a best-effort basis. There is no guaranteed response time or compatibility guarantee for third-party provider changes.
