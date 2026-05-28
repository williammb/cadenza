# Security Policy

## Supported versions

Cadenza has no public stable release yet. Security fixes are handled on the default branch until the first public release is published.

## Reporting a vulnerability

Please do not disclose exploitable details in a public issue.

Use GitHub private vulnerability reporting if it is enabled for this repository. If it is not available, open a minimal public issue asking for a private security contact, without reproduction steps or sensitive details.

Useful reports include:

- Affected operating system and Cadenza version or commit.
- Whether the app, CLI, local socket, updater, storage backend, or bundled UI is involved.
- Clear reproduction steps.
- Expected impact.
- Any temporary mitigation you know.

## Security model

Cadenza is a local desktop app. The CLI talks to the app over a local named pipe or Unix socket authenticated by a token stored under the user's Cadenza data directory. PostgreSQL passwords are stored in the OS keyring when that backend is used.

The app should not open a TCP server for agent communication. The UI must avoid unsanitized HTML assignment outside the documented markdown renderer allowlist.
