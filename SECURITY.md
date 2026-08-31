# Security policy

[Русская версия](SECURITY.ru.md) · [Technical security model](docs/SECURITY.md)

## Supported versions

DEcode is an early public preview without a stable release branch.

| Version | Security updates |
|---|---|
| `main` | Supported |
| Older commits and untagged binaries | Update to `main` before reporting unless the regression is version-specific |

## Report a vulnerability

Use **Security → Report a vulnerability** in the GitHub repository when private vulnerability reporting is available.

If that option is unavailable, open a public issue that asks the maintainer for a private reporting channel, but do not include the vulnerability, exploit, private code, credentials, endpoints, prompts, or logs in that issue.

Include privately:

- affected commit or release;
- operating system and terminal when relevant;
- affected trust boundary;
- minimal reproduction;
- expected and actual behavior;
- realistic impact;
- sanitized supporting material;
- any suggested fix or mitigation.

Do not test against systems, accounts, repositories, or data you do not own or have permission to use.

## In scope

- workspace sandbox or path escape;
- command approval or auto-approval bypass;
- execution of incomplete or unauthoritative model output;
- credential exposure in UI, errors, logs, sessions, or network requests;
- unsafe session, checkpoint, pause, rewind, or sub-agent recovery;
- MCP, OAuth, plugin, hook, LSP, attachment, or marketplace trust-boundary bypass;
- terminal-control or bidirectional-text injection that causes a security impact.

## Usually not a vulnerability

- a model producing incorrect or low-quality code without crossing a security boundary;
- a user explicitly approving the exact harmful command shown;
- provider downtime, quotas, unsupported models, or account-specific pricing;
- denial of service that requires intentionally configured extreme local limits;
- behavior already documented as an explicit limitation without a boundary bypass.

Security-sensitive bugs that are not exploitable can use the normal bug form after all private data has been removed.

## Disclosure

Give the maintainer a reasonable opportunity to reproduce and fix a valid issue before public disclosure. The project will document the affected versions, mitigation, and credit in a release note when appropriate. No bounty program is currently offered.
