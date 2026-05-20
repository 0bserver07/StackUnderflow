# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in StackUnderflow, please report it
responsibly:

1. **Do not** open a public GitHub issue.
2. Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability) instead.
3. Include a description of the vulnerability and steps to reproduce it.

## Scope

StackUnderflow runs entirely on your machine. It reads the local session files
your coding tools write and stores everything under `~/.stackunderflow/`. It
sends no telemetry and uploads no session data.

Things worth knowing:

- **The store holds sensitive content.** `~/.stackunderflow/store.db` keeps
  your conversation transcripts and tool output in plain, unencrypted SQLite.
  So do the snapshots `stackunderflow backup create` writes under
  `~/.stackunderflow/backups/`. Protect these like any other local file.
- **The server binds to localhost.** The dashboard listens on `127.0.0.1:8081`
  by default. CORS is restricted to localhost origins (the configured port and
  the Vite dev server) — it does not allow arbitrary origins.
- **Webhook secrets.** The optional PR / CI webhook receiver verifies a shared
  secret read from an environment variable. Keep that secret out of source
  control.
- **Outbound network.** The only outbound calls are a pricing snapshot and FX
  rates, both from public endpoints, and neither sends any of your data. Both
  have offline fallbacks.

## Supported Versions

Only the latest release receives security updates.
