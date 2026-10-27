# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately. Don't open a public issue.

- Use **Report a vulnerability** under this repository's **Security** tab (GitHub Security
  Advisories). This opens a private advisory that only the maintainer can see.
- If that option isn't available to you, contact the maintainer,
  [@ShriD5](https://github.com/ShriD5), privately through GitHub.

Include the Godspeed version (Godspeed menu → About, or `godspeed --version`), your OS version, the
steps to reproduce, and what an attacker could do. You should get a reply within a week. Godspeed is
maintained by one person, so fixes ship in the next release rather than on a fixed schedule.

## Supported versions

Only the latest release gets fixes.

## How Godspeed runs

Godspeed is a local app. By default it makes no account and syncs nothing:

- The engine listens for its UI and the `godspeed` CLI on a loopback WebSocket port
  (`127.0.0.1:27654` by default) and rejects handshakes that carry a browser `Origin` header.
- Chats, settings and agent memory live in `~/.godspeed` (or `GODSPEED_DATA_DIR`).
- Agents run as local processes under your user account, with the permissions the chat's access
  mode allows. Full access lets an agent run any command your user can.
- Multi-device sync is off unless you deploy `edge/` yourself and point Godspeed at it.

## In scope

- Anything that lets a web page, another user or a remote party reach the engine's IPC port or the
  zoomie (always-on agent) webhook endpoint (`POST /hooks/agents/{id}/{token}`) without the token.
- An agent action that runs without approval when the chat's access mode should have asked.
- Credentials or tokens leaking into logs, transcripts, error messages or exported data.
- Path traversal or code execution through chat import, file attachments, previews or markdown
  rendering.
- The `edge/` sync service, when deployed as documented.

## Out of scope

- What an agent does under full access, or actions you approved.
- Vulnerabilities in the agent CLIs themselves (Claude Code, Codex, Cursor and others). Report those
  to their vendors.
- Gatekeeper warnings caused by the app not being notarized yet.
- Attacks that need an attacker who already runs code as your user.
