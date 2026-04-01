# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in StackUnderflow, please report it responsibly:

1. **Do not** open a public GitHub issue
2. Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability) to report vulnerabilities.
3. Include a description of the vulnerability and steps to reproduce

## Scope

StackUnderflow runs locally and processes local files. The main security considerations are:

- **API keys** — never hardcode keys in source. Use environment variables.
- **Share feature** — opt-in upload to external service. Users should review what they share.
- **SQLite databases** — services store data locally. These may contain conversation content.
- **CORS** — the local server allows all origins for localhost development convenience.

## Supported Versions

Only the latest release receives security updates.
