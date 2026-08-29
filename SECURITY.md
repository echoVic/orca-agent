# Security Policy

## Supported Versions

Only the latest released version receives security updates.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| Older releases | No |

## Runtime Security Boundary

Non-dangerous tool, workflow, hook, MCP, and subprocess launches must pass the
capability resolver and execution broker. If the selected OS sandbox cannot be
fully enforced, Orca refuses the launch; it does not fall back to a host shell.
Project `.orca/config.toml` files are not authoritative for execution
capabilities, and `danger-full-access` is an explicit user-trusted integration
reported as advisory rather than sandboxed.

Sandbox denial paths parsed from child stdout/stderr are diagnostics only. A
permission escalation must be backed by a structured denial receipt from the
kernel or sandbox backend.

## Reporting a Vulnerability

Report vulnerabilities privately through a
[GitHub security advisory](https://github.com/echoVic/blade-deepseek/security/advisories/new).
Do not open a public Issue.

Please include:

- The affected version and platform.
- The impact and attack scenario.
- A minimal reproduction or proof of concept.
- Affected files, commands, or protocol surfaces.
- Any known mitigations or workarounds.
- Whether the issue has been disclosed previously, and to whom.

Remove API keys, tokens, private source code, personal data, and sensitive logs
from the report.

The project does not promise a fixed response SLA. Reporters and maintainers are
asked to coordinate validation, remediation, and disclosure so users have time
to update before technical details are published.
