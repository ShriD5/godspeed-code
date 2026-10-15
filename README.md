# Godspeed

One native app for every AI coding agent. Claude Code, Codex, Muse Code, Cursor, Grok Build,
OpenCode, Pi, Antigravity and Devin, in a Rust + GPU-rendered UI that opens instantly and stays
smooth on thousand-turn threads. Always-on agents keep working on a schedule or trigger and leave
the results in your inbox.

> Early. Expect rough edges. See [docs/godspeed/ROADMAP.md](docs/godspeed/ROADMAP.md) for what is
> done and what is next.

## Built on Zeron

Godspeed is a fork of [Zeron](https://github.com/zeronsh/zeron) by Wing, used under the MIT
license. Zeron did the hard part: the native GPUI shell, the headless engine, and the agent
drivers. Go star it.

What Godspeed adds on top (in progress, tracked in the roadmap):

- **Muse Code** support (native MSP driver).
- **Approvals and access modes**: supervised, auto-accept edits, auto, or full access, per thread.
- **Git from the app**: commit, push, pull and open PRs with generated messages and descriptions.
- **Always-on agents**: agents with memory, triggered by cron, webhooks, file changes or GitHub
  events, with an inbox.

## Build

Requires stable Rust (see `rust-toolchain.toml`) and, on macOS, Xcode.

```bash
cargo build -p godspeed
cargo run -p godspeed              # desktop app
cargo run -p godspeed -- headless  # engine only, for a server that keeps agents running
```

Install the agents you want to use (`claude`, `codex`, `muse`, `cursor-agent`, `grok`, `opencode`)
and sign in with each CLI. Godspeed drives the CLIs you already have; it never resells model access.

## Sync

Local-only by default. Multi-device sync needs your own deployment of `edge/` (Cloudflare Worker +
Durable Objects): set `GODSPEED_EDGE_URL` and `GODSPEED_WORKOS_CLIENT_ID`.

## License

MIT. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
